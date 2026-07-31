//! `sensorview ps` — the process list from a terminal.
//!
//! Pairs with `stream` and `top` as the third way to get data out without a
//! window, and it is the only way to exercise the process collector on a
//! headless machine — which is how the Linux path gets verified at all.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::procs::{self, ProcessCollector, ProcessRow, Sort};

/// Long enough for two refreshes at `REFRESH_INTERVAL`, plus slack for a busy
/// machine. Without a second refresh every CPU column would be blank.
const READY_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SortKey {
    Cpu,
    Mem,
    Disk,
    Pid,
    Name,
}

impl From<SortKey> for Sort {
    fn from(k: SortKey) -> Self {
        match k {
            SortKey::Cpu => Sort::Cpu,
            SortKey::Mem => Sort::Memory,
            SortKey::Disk => Sort::Disk,
            SortKey::Pid => Sort::Pid,
            SortKey::Name => Sort::Name,
        }
    }
}

pub fn run(
    json: bool,
    sort: SortKey,
    limit: Option<usize>,
    filter: Option<String>,
    ascending: bool,
) -> ExitCode {
    let mut collector = ProcessCollector::start();

    // Wait for a snapshot that can actually carry CPU percentages. Printing the
    // first one would give a table with an empty CPU column and an ordering
    // that looks deliberate but isn't — the same first-frame trap as `get`.
    let deadline = Instant::now() + READY_TIMEOUT;
    let snapshot = loop {
        let snap = collector.snapshot();
        if snap.has_cpu() {
            break snap;
        }
        if Instant::now() >= deadline {
            if snap.rows.is_empty() {
                eprintln!("sensorview: no processes after {READY_TIMEOUT:?}");
                collector.stop();
                return ExitCode::FAILURE;
            }
            eprintln!("sensorview: warning — no CPU baseline yet; CPU column will be blank.");
            break snap;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    collector.stop();

    let rows = procs::arrange(&snapshot.rows, filter.as_deref(), sort.into(), !ascending);
    let rows = match limit {
        Some(n) => &rows[..n.min(rows.len())],
        None => &rows[..],
    };

    let out = if json {
        serde_json::to_string_pretty(rows).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    } else {
        render_table(rows, snapshot.cpu_count)
    };
    super::print_or_exit(&out)
}

/// Fixed-width table, `ps`-like, so it lines up in a terminal and pipes into
/// `awk` without surprises.
pub fn render_table(rows: &[ProcessRow], cpu_count: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:>7}  {:>6}  {:<12}  {:>9}  {:>9}  {:>10}  {}\n",
        "PID", "CPU%", "USER", "MEM", "VIRT", "DISK", "NAME"
    ));

    for r in rows {
        let cpu = match r.cpu_pct {
            // An em dash, not 0.0: the value is unknown, not zero.
            None => "—".to_string(),
            Some(v) => format!("{v:.1}"),
        };
        out.push_str(&format!(
            "{:>7}  {:>6}  {:<12}  {:>9}  {:>9}  {:>10}  {}\n",
            r.pid,
            cpu,
            truncate(r.user.as_deref().unwrap_or("—"), 12),
            procs::format_bytes(r.mem_bytes),
            procs::format_bytes(r.virt_bytes),
            procs::format_rate(r.disk_bps),
            r.name,
        ));
    }

    if rows.is_empty() {
        out.push_str("(no matching processes)\n");
    } else if cpu_count > 0 {
        // Without this, "800%" in the CPU column looks like a bug.
        out.push_str(&format!(
            "\n{} processes · CPU% is per-thread-summed ({} logical CPUs, so {}% = fully busy)\n",
            rows.len(),
            cpu_count,
            cpu_count * 100
        ));
    }
    out.trim_end().to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, name: &str, cpu: Option<f32>, user: Option<&str>) -> ProcessRow {
        ProcessRow {
            pid,
            parent: None,
            name: name.into(),
            cmd: String::new(),
            user: user.map(str::to_string),
            cpu_pct: cpu,
            mem_bytes: 5 * 1024 * 1024,
            virt_bytes: 20 * 1024 * 1024,
            disk_read_bytes: 0,
            disk_write_bytes: 0,
            disk_bps: None,
            run_time_s: 1,
        }
    }

    /// The whole reason `cpu_pct` is an `Option`: an unknown value must not
    /// print as 0.0, which a script would happily treat as "idle".
    #[test]
    fn unknown_cpu_renders_as_a_dash_not_zero() {
        let out = render_table(&[row(1, "init", None, Some("root"))], 8);
        assert!(out.contains('—'), "{out}");
        assert!(!out.contains("0.0"), "unknown CPU must not look like zero:\n{out}");
    }

    #[test]
    fn missing_user_also_renders_as_a_dash() {
        let out = render_table(&[row(1, "ghost", Some(1.0), None)], 8);
        assert!(out.contains('—'), "{out}");
    }

    /// 800% on an 8-core box is correct but startling, so the footer says so.
    #[test]
    fn footer_explains_per_core_summed_percentages() {
        let out = render_table(&[row(1, "busy", Some(800.0), Some("me"))], 8);
        assert!(out.contains("8 logical CPUs"), "{out}");
        assert!(out.contains("800%"), "{out}");
    }

    /// The disk column belongs to the headless build too, not just the GUI.
    ///
    /// This is load-bearing beyond cosmetics: when `Sort::Disk` and
    /// `format_rate` were reachable only from the Task Manager window, a
    /// `--no-default-features` build saw them as dead code and CI failed on
    /// `-D warnings` for every GUI-less feature combination. Exercising them
    /// from the CLI is what keeps that honest, so this test failing is the
    /// signal that the headless build is about to break again.
    #[test]
    fn disk_rate_appears_in_the_table_and_distinguishes_idle_from_unknown() {
        let mut busy = row(1, "writer", Some(1.0), Some("me"));
        busy.disk_bps = Some(3.0 * 1024.0 * 1024.0);
        let mut idle = row(2, "quiet", Some(1.0), Some("me"));
        idle.disk_bps = Some(0.0);
        // `unknown` keeps disk_bps: None from the fixture.
        let unknown = row(3, "cold", Some(1.0), Some("me"));

        let out = render_table(&[busy, idle, unknown], 8);
        assert!(out.contains("DISK"), "header must carry the column:\n{out}");
        assert!(out.contains("3.0 MB/s"), "{out}");
        assert!(out.contains("0 MB/s"), "measured idle should read as zero:\n{out}");
        assert!(out.contains('—'), "an unread rate must stay unknown:\n{out}");
    }

    #[test]
    fn empty_result_says_so_rather_than_printing_a_bare_header() {
        let out = render_table(&[], 8);
        assert!(out.contains("no matching processes"), "{out}");
    }

    #[test]
    fn long_user_names_are_truncated_to_keep_columns_aligned() {
        let out = render_table(&[row(1, "x", Some(1.0), Some("averyverylongusername"))], 4);
        let header_width = out.lines().next().unwrap().len();
        let row_width = out.lines().nth(1).unwrap().len();
        // The name column is last and free-width, so compare the fixed prefix.
        assert!(
            row_width <= header_width + 4,
            "row did not stay aligned:\nheader: {header_width}\nrow:    {row_width}\n{out}"
        );
    }

    #[test]
    fn sort_key_maps_onto_the_collector_ordering() {
        assert_eq!(Sort::from(SortKey::Cpu), Sort::Cpu);
        assert_eq!(Sort::from(SortKey::Mem), Sort::Memory);
        assert_eq!(Sort::from(SortKey::Disk), Sort::Disk);
        assert_eq!(Sort::from(SortKey::Pid), Sort::Pid);
        assert_eq!(Sort::from(SortKey::Name), Sort::Name);
    }
}
