//! Process enumeration — the data behind the Task Manager window and
//! `sensorview ps`.
//!
//! A separate lane from the sensor poller, for two reasons. Enumerating every
//! process costs tens of milliseconds and must never sit on the hardware loop;
//! and unlike sensors, nobody needs this running when they aren't looking at
//! it, so the collector starts when the window opens and stops when it closes.
//!
//! Publication follows the same shape as [`crate::inventory`]: a background
//! thread swaps an immutable snapshot into an `ArcSwap`, and readers take an
//! atomic pointer load that never blocks.
//!
//! Backed by the `sysinfo` crate rather than hand-rolled FFI. That is a
//! deliberate exception to this codebase's usual idiom: process enumeration
//! would otherwise mean three separate piles of `unsafe` (libproc + sysctl,
//! `/proc`, NtQuerySystemInformation) for data that is not the point of the
//! project.
//!
//! **Naming:** the crate is `sysinfo` and this project also has a
//! `crate::sysinfo` module (static machine facts). Inside this module the crate
//! is always spelled `::sysinfo` so which one is meant is never ambiguous.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;

/// How often the process list is refreshed while the window is open.
///
/// `sysinfo` requires at least `MINIMUM_CPU_UPDATE_INTERVAL` between refreshes
/// for CPU percentages to mean anything, and 1 Hz is what task managers use.
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(1000);

/// One process, flattened for display.
///
/// The `Option` fields are load-bearing: a value that this platform cannot
/// provide stays `None` and renders as `—`. Reporting 0 instead would claim a
/// process is using no CPU when the truth is that we cannot see.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProcessRow {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    /// Full command line; empty when the kernel won't disclose it.
    pub cmd: String,
    /// Owning user. `None` when the uid has no passwd entry.
    pub user: Option<String>,
    /// Percent of one core, times the number of cores, as every task manager
    /// reports it: 100 % is one core saturated, 800 % is eight.
    /// `None` on the first snapshot, before a baseline exists.
    pub cpu_pct: Option<f32>,
    pub mem_bytes: u64,
    pub virt_bytes: u64,
    /// Cumulative bytes read/written by this process. Zero can mean "idle" or
    /// "not permitted to look" — see the note in `build_snapshot`.
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    /// Combined read+write bytes per second. `None` until a baseline exists:
    /// the underlying counters are cumulative, so a rate needs two samples —
    /// exactly the constraint `cpu_pct` has, and reported the same way.
    pub disk_bps: Option<f64>,
    pub run_time_s: u64,
}

/// An immutable point-in-time view of every process.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ProcessSnapshot {
    /// Monotonic counter. `seq >= 2` is the first snapshot that can carry CPU
    /// percentages, for the same reason every rate-derived sensor here needs a
    /// baseline.
    pub seq: u64,
    pub unix_ms: u64,
    pub rows: Vec<ProcessRow>,
    /// Sum across all processes; can exceed 100 on a multi-core machine.
    pub total_cpu_pct: f32,
    pub cpu_count: usize,
}

impl ProcessSnapshot {
    /// True once CPU percentages are meaningful.
    pub fn has_cpu(&self) -> bool {
        self.seq >= 2
    }
}

/// Owns the collector thread. Dropping it stops the thread.
pub struct ProcessCollector {
    snapshot: Arc<ArcSwap<ProcessSnapshot>>,
    running: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl ProcessCollector {
    /// Start collecting. The first snapshot lands almost immediately; the first
    /// one *with CPU percentages* lands one interval later.
    pub fn start() -> Self {
        let snapshot = Arc::new(ArcSwap::from_pointee(ProcessSnapshot::default()));
        let running = Arc::new(AtomicBool::new(true));

        let sink = snapshot.clone();
        let flag = running.clone();
        let join = std::thread::Builder::new()
            .name("sensorview-procs".into())
            .spawn(move || collect_loop(&sink, &flag))
            .expect("spawn process collector");

        Self { snapshot, running, join: Some(join) }
    }

    /// Latest snapshot. Lock-free; never blocks the caller.
    pub fn snapshot(&self) -> Arc<ProcessSnapshot> {
        self.snapshot.load_full()
    }

    /// Stop the thread and wait for it. Idempotent.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

}

impl Drop for ProcessCollector {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Waits for a collector to reach some state, polling rather than sleeping a
/// fixed span.
///
/// Tests here care about *sequence* — a snapshot exists, a CPU baseline has
/// been established — not about wall-clock time. Sleeping
/// `REFRESH_INTERVAL * 2` and assuming two refreshes have landed conflates the
/// two, and it is wrong on a loaded machine: enumerating several hundred
/// processes twice can take longer than the interval itself. That is exactly
/// how these tests first went red on a macOS CI runner while passing on every
/// developer machine. Polling with a generous ceiling keeps the assertion
/// honest — a collector that never restarts still fails — while letting a slow
/// box simply take longer.
#[cfg(test)]
pub(crate) fn wait_for(
    collector: &ProcessCollector,
    what: &str,
    ready: impl Fn(&ProcessSnapshot) -> bool,
) -> Arc<ProcessSnapshot> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let snap = collector.snapshot();
        if ready(&snap) {
            return snap;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after 30s waiting for {what} (seq {})",
            snap.seq
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn collect_loop(sink: &ArcSwap<ProcessSnapshot>, running: &AtomicBool) {
    use ::sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, Users};

    let mut sys = System::new();
    // Refreshed rarely on purpose: the passwd database does not change at 1 Hz
    // and rebuilding it every tick is pure waste.
    let mut users = Users::new_with_refreshed_list();

    // Logical CPUs, not physical cores. `cpu_usage()` is per-thread, so on a
    // 4-core/8-thread machine one process can legitimately report 800 %;
    // labelling that against 4 cores would tell the user it is impossible.
    let cpu_count = std::thread::available_parallelism().map_or(0, |n| n.get());

    // What to refresh, spelled out. `UpdateKind` defaults to `Never`, so the
    // plain `refresh_processes` left user IDs unset — which happened to work on
    // macOS, where the uid comes free with the kernel struct, and left every
    // owner blank on Linux. Identity fields use `OnlyIfNotSet` because a
    // process's user and command line do not change; cpu/memory/disk are
    // re-read every tick because that is the whole point.
    let what = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_disk_usage()
        .with_user(UpdateKind::OnlyIfNotSet)
        .with_cmd(UpdateKind::OnlyIfNotSet);

    let mut seq: u64 = 0;

    // Last tick's cumulative I/O counters, for turning them into a rate.
    // Measured against the wall clock actually elapsed rather than
    // REFRESH_INTERVAL, because a refresh over several hundred processes can
    // take a sizeable fraction of the interval — assuming the nominal period
    // would overstate every rate on a slow machine.
    let mut prev_io: HashMap<u32, (u64, u64)> = HashMap::new();
    let mut last_tick = Instant::now();

    while running.load(Ordering::Relaxed) {
        // `true` removes processes that have exited since the last refresh.
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, what);
        seq += 1;

        let now = Instant::now();
        let dt_s = now.duration_since(last_tick).as_secs_f64();
        last_tick = now;

        // Occasionally, in case a user appeared mid-session.
        if seq.is_multiple_of(60) {
            users = Users::new_with_refreshed_list();
        }

        let (snapshot, next_io) =
            build_snapshot(&sys, &users, seq, cpu_count, &prev_io, dt_s);
        prev_io = next_io;
        sink.store(Arc::new(snapshot));

        // Sliced so stopping doesn't wait out a full interval.
        sleep_interruptible(running, REFRESH_INTERVAL);
    }
}

/// Build a snapshot, and return the I/O counters to difference against next
/// tick.
fn build_snapshot(
    sys: &::sysinfo::System,
    users: &::sysinfo::Users,
    seq: u64,
    cpu_count: usize,
    prev_io: &HashMap<u32, (u64, u64)>,
    dt_s: f64,
) -> (ProcessSnapshot, HashMap<u32, (u64, u64)>) {
    // CPU percentages are meaningless until sysinfo has two samples to
    // difference, so the first snapshot reports them absent rather than 0.
    let cpu_ready = seq >= 2;
    // Same rule for I/O rates, plus a positive interval to divide by.
    let io_ready = cpu_ready && dt_s > 0.0;
    let mut total_cpu_pct = 0.0f32;
    let mut next_io = HashMap::with_capacity(sys.processes().len());

    let rows: Vec<ProcessRow> = sys
        .processes()
        .iter()
        .map(|(pid, p)| {
            let cpu = p.cpu_usage();
            if cpu_ready {
                total_cpu_pct += cpu;
            }
            // NB: sysinfo reports 0 both for "did no I/O" and "not permitted to
            // look" (root-owned processes on macOS, other users on Linux).
            // Mapping 0 to None would hide genuinely idle processes, which is
            // the more common case, so 0 is reported as 0 and the caveat is
            // documented rather than guessed at.
            let disk = p.disk_usage();
            let pid_u32 = pid.as_u32();
            let cumulative = (disk.total_read_bytes, disk.total_written_bytes);
            // `saturating_sub` because pids are recycled: a new process
            // inheriting a dead one's pid starts its counters at zero, and an
            // unsigned wrap would report an absurd rate rather than none.
            let disk_bps = io_ready.then(|| {
                prev_io.get(&pid_u32).map(|(pr, pw)| {
                    let moved = cumulative.0.saturating_sub(*pr)
                        + cumulative.1.saturating_sub(*pw);
                    moved as f64 / dt_s
                })
            })
            .flatten();
            next_io.insert(pid_u32, cumulative);

            ProcessRow {
                pid: pid_u32,
                parent: p.parent().map(|p| p.as_u32()),
                name: p.name().to_string_lossy().into_owned(),
                cmd: p
                    .cmd()
                    .iter()
                    .map(|a| a.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                user: p
                    .user_id()
                    .and_then(|uid| users.get_user_by_id(uid))
                    .map(|u| u.name().to_string()),
                cpu_pct: cpu_ready.then_some(cpu),
                mem_bytes: p.memory(),
                virt_bytes: p.virtual_memory(),
                disk_read_bytes: cumulative.0,
                disk_write_bytes: cumulative.1,
                disk_bps,
                run_time_s: p.run_time(),
            }
        })
        .collect();

    let snapshot = ProcessSnapshot {
        seq,
        unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        rows,
        total_cpu_pct,
        cpu_count,
    };
    (snapshot, next_io)
}

fn sleep_interruptible(running: &AtomicBool, total: Duration) {
    let slice = Duration::from_millis(50);
    let mut left = total;
    while left > Duration::ZERO && running.load(Ordering::Relaxed) {
        let step = slice.min(left);
        std::thread::sleep(step);
        left -= step;
    }
}

/// How a process list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    Cpu,
    Memory,
    Disk,
    Pid,
    Name,
}

/// Filter by name/command substring (case-insensitive) and order.
///
/// Split out from rendering so the CLI and the window sort identically, and so
/// it is testable without either.
pub fn arrange(
    rows: &[ProcessRow],
    filter: Option<&str>,
    sort: Sort,
    descending: bool,
) -> Vec<ProcessRow> {
    let needle = filter.map(str::to_lowercase);
    let mut out: Vec<ProcessRow> = rows
        .iter()
        .filter(|r| {
            needle.as_ref().is_none_or(|n| {
                r.name.to_lowercase().contains(n)
                    || r.cmd.to_lowercase().contains(n)
                    || r.pid.to_string() == *n
            })
        })
        .cloned()
        .collect();

    out.sort_by(|a, b| {
        let ord = match sort {
            // An unread CPU sorts lowest rather than as zero, so a first-tick
            // list isn't silently ordered by nothing while looking deliberate.
            Sort::Cpu => a
                .cpu_pct
                .unwrap_or(f32::NEG_INFINITY)
                .total_cmp(&b.cpu_pct.unwrap_or(f32::NEG_INFINITY)),
            Sort::Memory => a.mem_bytes.cmp(&b.mem_bytes),
            // Unread I/O sorts below idle, for the same reason as CPU above.
            Sort::Disk => a
                .disk_bps
                .unwrap_or(f64::NEG_INFINITY)
                .total_cmp(&b.disk_bps.unwrap_or(f64::NEG_INFINITY)),
            Sort::Pid => a.pid.cmp(&b.pid),
            Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        };
        if descending {
            ord.reverse()
        } else {
            ord
        }
    });
    out
}

/// Ask a process to exit.
///
/// Returns `Err` when the process is gone or not ours to signal — the common
/// case for anything owned by root. Callers must surface that rather than
/// appearing to succeed.
pub fn kill(pid: u32, force: bool) -> Result<(), String> {
    use ::sysinfo::{Pid, ProcessesToUpdate, Signal, System};

    let mut sys = System::new();
    let target = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[target]), false);

    let Some(process) = sys.process(target) else {
        return Err(format!("no process with pid {pid}"));
    };
    let signal = if force { Signal::Kill } else { Signal::Term };

    // `kill_with` returns None when the platform has no such signal.
    match process.kill_with(signal) {
        Some(true) => Ok(()),
        Some(false) => Err(format!(
            "not permitted to signal pid {pid} (owned by another user?)"
        )),
        None => Err(format!("{signal:?} is not supported on this platform")),
    }
}

/// Human-readable transfer rate, the way a task manager shows a Disk column.
///
/// `None` is "no reading yet" and prints as `—`, keeping the distinction the
/// rest of this module makes between *unknown* and *idle*.
///
/// A known rate below 0.1 MB/s prints as `<0.1 MB/s`, not as `0 MB/s`: at one
/// decimal place a process doing steady small writes would otherwise be
/// indistinguishable from one doing nothing at all, and this column exists to
/// tell those apart. Exactly zero still prints `0 MB/s`.
pub fn format_rate(bps: Option<f64>) -> String {
    match bps {
        None => "—".to_string(),
        Some(v) if v <= 0.0 => "0 MB/s".to_string(),
        Some(v) => {
            let mb = v / (1024.0 * 1024.0);
            if mb >= 0.1 {
                format!("{mb:.1} MB/s")
            } else {
                "<0.1 MB/s".to_string()
            }
        }
    }
}

/// Human-readable byte size.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KB * KB * KB {
        format!("{:.1} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.0} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, name: &str, cpu: Option<f32>, mem: u64) -> ProcessRow {
        ProcessRow {
            pid,
            parent: None,
            name: name.into(),
            cmd: format!("/usr/bin/{name} --flag"),
            user: Some("someone".into()),
            cpu_pct: cpu,
            mem_bytes: mem,
            virt_bytes: mem * 4,
            disk_read_bytes: 0,
            disk_write_bytes: 0,
            disk_bps: None,
            run_time_s: 10,
        }
    }

    fn sample() -> Vec<ProcessRow> {
        vec![
            row(3, "zsh", Some(1.0), 300),
            row(1, "kernel_task", Some(50.0), 100),
            row(2, "Safari", Some(7.5), 900),
        ]
    }

    #[test]
    fn sorts_by_each_key_in_both_directions() {
        let by_cpu = arrange(&sample(), None, Sort::Cpu, true);
        assert_eq!(by_cpu.iter().map(|r| r.pid).collect::<Vec<_>>(), vec![1, 2, 3]);

        let by_mem = arrange(&sample(), None, Sort::Memory, true);
        assert_eq!(by_mem[0].name, "Safari");

        let by_pid = arrange(&sample(), None, Sort::Pid, false);
        assert_eq!(by_pid.iter().map(|r| r.pid).collect::<Vec<_>>(), vec![1, 2, 3]);

        let by_name = arrange(&sample(), None, Sort::Name, false);
        assert_eq!(by_name[0].name, "kernel_task");
    }

    /// On the first snapshot every `cpu_pct` is `None`. Treating that as 0
    /// would order the list arbitrarily while looking deliberate.
    #[test]
    fn unread_cpu_sorts_last_not_as_zero() {
        let mut rows = sample();
        rows.push(row(4, "unknown", None, 50));
        let sorted = arrange(&rows, None, Sort::Cpu, true);
        assert_eq!(
            sorted.last().unwrap().pid,
            4,
            "a process with no CPU reading must sort last, not above idle ones"
        );
    }

    #[test]
    fn filter_matches_name_command_and_exact_pid() {
        assert_eq!(arrange(&sample(), Some("SAFARI"), Sort::Pid, false).len(), 1);
        // Matches inside the command line too.
        assert_eq!(arrange(&sample(), Some("--flag"), Sort::Pid, false).len(), 3);
        // An exact pid, so `ps --filter 2` finds pid 2.
        assert!(arrange(&sample(), Some("2"), Sort::Pid, false)
            .iter()
            .any(|r| r.pid == 2));
        assert!(arrange(&sample(), Some("nothing-here"), Sort::Pid, false).is_empty());
    }

    /// A disk rate that has no baseline yet must read as unknown, not as 0 —
    /// the same distinction `cpu_pct` makes, and for the same reason.
    #[test]
    fn unread_disk_rate_sorts_last_and_prints_as_unknown() {
        assert_eq!(format_rate(None), "—");
        // Measured as genuinely idle.
        assert_eq!(format_rate(Some(0.0)), "0 MB/s");
        // Small but real: must not be flattened into "0 MB/s", which is the
        // whole point of having the column.
        assert_eq!(format_rate(Some(1024.0)), "<0.1 MB/s");
        assert_eq!(format_rate(Some(5.0 * 1024.0 * 1024.0)), "5.0 MB/s");

        let mut rows = sample();
        rows[0].disk_bps = Some(4.0 * 1024.0 * 1024.0);
        rows[1].disk_bps = Some(0.0);
        // rows[2] keeps `None`.
        let sorted = arrange(&rows, None, Sort::Disk, true);
        assert_eq!(sorted[0].pid, 3, "busiest disk user must sort first");
        assert_eq!(
            sorted.last().unwrap().pid,
            2,
            "an unread rate must sort below a known-idle one"
        );
    }

    #[test]
    fn byte_formatting_picks_sensible_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn killing_a_nonexistent_pid_reports_rather_than_panics() {
        // 0 is never a signalable user process on any supported platform.
        assert!(kill(0, false).is_err());
    }

    /// Exercises the real collector: it must produce processes, and must reach
    /// `seq >= 2`, the point at which CPU percentages become meaningful.
    #[test]
    fn collector_produces_processes_and_a_cpu_baseline() {
        let mut collector = ProcessCollector::start();

        let first = wait_for(&collector, "the first snapshot", |s| s.seq >= 1);
        assert!(!first.rows.is_empty(), "collector produced no processes");
        // The invariant that must hold of *any* snapshot: percentages appear
        // exactly when a baseline exists, never independently of one.
        assert_eq!(
            first.has_cpu(),
            first.rows.iter().any(|r| r.cpu_pct.is_some()),
            "cpu_pct disagreed with has_cpu() at seq {}",
            first.seq
        );
        // Polling at 25 ms always catches seq 1, which is published for a full
        // interval; the guard only keeps a pathological stall from failing the
        // run spuriously.
        if first.seq == 1 {
            assert!(!first.has_cpu(), "seq 1 must not yet claim CPU data");
            assert!(
                first.rows.iter().all(|r| r.cpu_pct.is_none()),
                "first snapshot must not report CPU percentages"
            );
        }

        let later = wait_for(&collector, "a CPU baseline", |s| s.has_cpu());
        assert!(
            later.rows.iter().any(|r| r.cpu_pct.is_some()),
            "no process reported CPU after a baseline existed"
        );

        collector.stop();
        // Idempotent — Drop calls it again, and a second join must not hang.
        collector.stop();
    }

    /// The window opens, closes and reopens. A restart must produce a working
    /// collector with its own fresh baseline; a leaked thread or carried-over
    /// state shows up here and nowhere else.
    #[test]
    fn restarting_the_collector_re_establishes_a_baseline() {
        let mut first = ProcessCollector::start();
        wait_for(&first, "the first collector to publish", |s| s.seq >= 1);
        first.stop();
        drop(first);

        let mut second = ProcessCollector::start();
        let later = wait_for(&second, "a CPU baseline after restart", |s| s.has_cpu());
        assert!(!later.rows.is_empty());
        second.stop();
    }
}
