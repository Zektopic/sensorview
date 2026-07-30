//! Command-line front end.
//!
//! Always compiled — `argv` is the entry point for every mode, so gating this
//! behind a feature would mean two dispatch paths. A build without the `gui`
//! feature has the CLI as its *only* front end.
//!
//! Bare `sensorview` (no subcommand) must keep launching the GUI: installers,
//! Dock entries and Start-menu shortcuts all invoke the binary with no
//! arguments. Hence [`Cli::command`] is an `Option`.

pub mod daemon;
pub mod procs;
#[cfg(feature = "push")]
pub mod push;
pub mod render;
pub mod stream;
#[cfg(feature = "tui")]
pub mod tui;

use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::runtime::{self, Runtime};
use crate::settings::AppSettings;

/// How long a one-shot command waits for usable sensor data before giving up.
/// Generous because the Windows sidecar can take several seconds to start.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Parser, Debug)]
#[command(
    name = "sensorview",
    version,
    about = "Native hardware monitor — sensors, temperatures, power and clocks",
    long_about = None,
)]
pub struct Cli {
    /// No subcommand launches the graphical interface.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Print the sensor tree and exit.
    Sensors {
        /// Emit JSON instead of a human-readable tree.
        #[arg(long)]
        json: bool,
        /// Only show sensors whose name contains this (case-insensitive).
        #[arg(long, value_name = "TEXT")]
        filter: Option<String>,
    },
    /// Print a single sensor's value and exit; exit code 1 if not found.
    Get {
        /// Sensor name, or any case-insensitive substring of one.
        #[arg(value_name = "NAME")]
        name: String,
        /// Emit JSON instead of a bare value.
        #[arg(long)]
        json: bool,
        /// Print only the number, with no unit — for `$(...)` in scripts.
        #[arg(long, conflicts_with = "json")]
        raw: bool,
    },
    /// Print static system information (the System Summary) and exit.
    Info {
        #[arg(long)]
        json: bool,
    },
    /// Write a full text report, the same one the GUI's Report button produces.
    Report,
    /// Live terminal dashboard. Press q to quit, r to reset min/max.
    #[cfg(feature = "tui")]
    Top {
        /// Only show sensors whose name contains this (case-insensitive).
        #[arg(long, value_name = "TEXT")]
        filter: Option<String>,
    },
    /// List processes and exit — a task manager for the terminal.
    Ps {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Order by.
        #[arg(long, value_enum, default_value_t = procs::SortKey::Cpu)]
        sort: procs::SortKey,
        /// Show only the top N.
        #[arg(short = 'n', long, value_name = "COUNT")]
        limit: Option<usize>,
        /// Only processes whose name, command or exact pid matches.
        #[arg(long, value_name = "TEXT")]
        filter: Option<String>,
        /// Ascending instead of descending.
        #[arg(long)]
        ascending: bool,
    },
    /// Terminate a process by pid.
    Kill {
        /// Process id, as shown by `sensorview ps`.
        #[arg(value_name = "PID")]
        pid: u32,
        /// Send SIGKILL instead of SIGTERM — no chance to clean up.
        #[arg(long)]
        force: bool,
    },
    /// Stream telemetry to stdout until interrupted (or `-n` records).
    Stream {
        /// Output format.
        #[arg(long, value_enum, default_value_t = stream::Format::Ndjson)]
        format: stream::Format,
        /// Only stream sensors whose name contains this (case-insensitive).
        #[arg(long, value_name = "TEXT")]
        filter: Option<String>,
        /// Stop after this many records. Unlimited if omitted.
        #[arg(short = 'n', long, value_name = "COUNT")]
        count: Option<u64>,
    },
    /// Run headless: sensors, web dashboard, no window. Ctrl-C to stop.
    Daemon {
        /// Port for the web dashboard.
        #[arg(long)]
        port: Option<u16>,
        /// Bind address. Anything other than loopback requires a token.
        #[arg(long, value_name = "IP")]
        bind: Option<String>,
        /// Also write a CSV log, as the GUI's "Start Logging" button does.
        #[arg(long)]
        log: bool,
        /// Poll interval in milliseconds (clamped to the engine's 250–10000 range).
        #[arg(long, value_name = "MS")]
        interval: Option<u64>,
        /// Push telemetry to a collector, e.g. influx://host:8086/write?db=sensors
        /// or http://collector/ingest. Repeatable.
        #[cfg(feature = "push")]
        #[arg(long = "push", value_name = "URL")]
        push_to: Vec<String>,
        /// Seconds between pushes.
        #[cfg(feature = "push")]
        #[arg(long, value_name = "SECS", default_value_t = 10)]
        push_interval: u64,
    },
}

/// Dispatch a subcommand. Only called when one was actually given.
pub fn run(command: Command, settings: AppSettings) -> ExitCode {
    attach_console();

    match command {
        // Runs its own short-lived collector; the sensor pipeline is not
        // involved, so don't pay to start it.
        Command::Ps { json, sort, limit, filter, ascending } => {
            procs::run(json, sort, limit, filter, ascending)
        }
        Command::Kill { pid, force } => match crate::procs::kill(pid, force) {
            Ok(()) => {
                let how = if force { "killed" } else { "asked to exit" };
                super::cli::print_or_exit(&format!("pid {pid} {how}"))
            }
            Err(e) => {
                eprintln!("sensorview: {e}");
                ExitCode::FAILURE
            }
        },
        Command::Daemon {
            port,
            bind,
            log,
            interval,
            #[cfg(feature = "push")]
            push_to,
            #[cfg(feature = "push")]
            push_interval,
        } => daemon::run(daemon::Options {
            settings,
            port,
            bind,
            log,
            interval,
            #[cfg(feature = "push")]
            push_to,
            #[cfg(feature = "push")]
            push_interval,
        }),
        // Everything else is a one-shot: start the pipeline, wait for usable
        // data, print, exit. `Runtime::drop` shuts the threads down.
        other => {
            let rt = runtime::start(&settings);
            one_shot(&rt, other)
        }
    }
}

fn one_shot(rt: &Runtime, command: Command) -> ExitCode {
    match command {
        Command::Sensors { json, filter } => {
            // `seq >= 2`, not `seq > 0`: power, clocks and throughput are all
            // computed from the delta between two polls, so they are absent
            // from the very first frame. Waiting for the wrong condition
            // prints a tree missing its most interesting rows.
            let (frame, timed_out) = match rt.wait_for_usable_frame(READY_TIMEOUT) {
                Ok(f) => (f, false),
                Err(f) => (f, true),
            };
            if timed_out && frame.seq == 0 {
                eprintln!("sensorview: no sensor data after {READY_TIMEOUT:?} — is a backend available?");
                return ExitCode::FAILURE;
            }
            if timed_out {
                eprintln!("sensorview: warning — only {} frame(s) polled; rate-based sensors (power, clocks) may be missing.", frame.seq);
            }
            let out = if json {
                render::sensors_json(&frame, filter.as_deref())
            } else {
                render::sensors_text(&frame, filter.as_deref())
            };
            print_or_exit(&out)
        }

        Command::Get { name, json, raw } => {
            // Wait for *this* sensor to actually carry a value, rather than for
            // any frame — otherwise a cold-start `get "CPU Package Power"`
            // reliably prints nothing.
            let needle = name.to_lowercase();
            let found = rt.wait_for(
                |f| render::find_sensor(f, &needle).is_some_and(|s| s.value.is_some()),
                READY_TIMEOUT,
            );
            let frame = match found {
                Ok(f) => f,
                Err(f) => f,
            };
            let Some(sensor) = render::find_sensor(&frame, &needle) else {
                eprintln!("sensorview: no sensor matching {name:?}");
                return ExitCode::FAILURE;
            };
            let out = render::one_sensor(sensor, json, raw);
            print_or_exit(&out)
        }

        Command::Info { json } => {
            // Static info is queried on its own thread and is independent of
            // the poll cadence.
            let deadline = std::time::Instant::now() + READY_TIMEOUT;
            let info = loop {
                if let Ok(guard) = rt.sysinfo.read() {
                    if let Some(info) = guard.clone() {
                        break Some(info);
                    }
                }
                if std::time::Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            let Some(info) = info else {
                eprintln!("sensorview: system information unavailable");
                return ExitCode::FAILURE;
            };
            let out = if json {
                serde_json::to_string_pretty(&info).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
            } else {
                render::info_text(&info)
            };
            print_or_exit(&out)
        }

        Command::Report => {
            let (frame, _) = match rt.wait_for_usable_frame(READY_TIMEOUT) {
                Ok(f) => (f, false),
                Err(f) => (f, true),
            };
            let info = rt.sysinfo.read().ok().and_then(|g| g.clone());
            match crate::report::write_report(&frame.tree, info.as_ref()) {
                Ok(path) => print_or_exit(&format!("Report written to {}", path.display())),
                Err(e) => {
                    eprintln!("sensorview: {e}");
                    ExitCode::FAILURE
                }
            }
        }

        #[cfg(feature = "tui")]
        Command::Top { filter } => tui::run(rt, filter, READY_TIMEOUT),

        Command::Stream { format, filter, count } => {
            stream::run(rt, format, filter, count, READY_TIMEOUT)
        }

        Command::Daemon { .. } | Command::Ps { .. } | Command::Kill { .. } => {
            unreachable!("these are dispatched in run(), before the runtime starts")
        }
    }
}

/// Write to stdout, treating a closed pipe as success.
///
/// `sensorview sensors | head -1` is a normal thing to type. Rust sets SIGPIPE
/// to ignore at startup, so the write returns `EPIPE` rather than killing the
/// process; without this, the canonical pipeline would report a spurious error.
fn print_or_exit(text: &str) -> ExitCode {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    match writeln!(lock, "{text}").and_then(|()| lock.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sensorview: write failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Windows release builds with the GUI compiled in are linked into the *Windows*
/// subsystem, which allocates no console — `println!` would go nowhere. When
/// such a binary is run from an existing terminal, borrow that terminal.
///
/// No-op everywhere else, and on a `--no-default-features` build, which is a
/// console program already.
#[cfg(all(windows, feature = "gui"))]
fn attach_console() {
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    // Failure just means there was no parent console (launched from Explorer),
    // in which case there is nothing to attach to and nothing to fix.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(not(all(windows, feature = "gui")))]
fn attach_console() {}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Catches a malformed clap tree (duplicate flags, bad conflicts) at test
    /// time rather than on first run.
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// The single most regression-prone behaviour: a bare invocation must not
    /// be an error, because that is how every installer launches the app.
    #[test]
    fn bare_invocation_has_no_subcommand() {
        let cli = Cli::try_parse_from(["sensorview"]).expect("bare invocation must parse");
        assert!(cli.command.is_none(), "no args must mean 'launch the GUI'");
    }

    #[test]
    fn subcommands_parse() {
        let cli = Cli::try_parse_from(["sensorview", "get", "CPU Package Power", "--raw"]).unwrap();
        match cli.command {
            Some(Command::Get { name, raw, json }) => {
                assert_eq!(name, "CPU Package Power");
                assert!(raw);
                assert!(!json);
            }
            other => panic!("expected Get, got {other:?}"),
        }
    }

    /// `--raw` and `--json` are mutually exclusive; clap must reject both.
    #[test]
    fn raw_and_json_conflict() {
        assert!(Cli::try_parse_from(["sensorview", "get", "x", "--raw", "--json"]).is_err());
    }
}
