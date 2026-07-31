//! **Thread 1 — the hardware poller.**
//!
//! Owns the [`SensorSource`] and the running statistics, and is the *single
//! writer* to [`TelemetryStore`]. Nothing else in the app mutates telemetry, so
//! readers never need a lock (see `state.rs` for the full argument).
//!
//! # Two cadences, deliberately
//!
//! A blanket 1 s loop over *everything* would be actively harmful:
//!
//! - S.M.A.R.T. / NVMe log reads keep drives out of low-power states.
//! - SPD and Embedded-Controller reads go over SMBus/I²C; polling that bus
//!   aggressively collides with firmware and other tools and provokes SMI
//!   storms — audio dropouts and micro-stutter.
//! - PCIe topology only changes on hotplug.
//!
//! So the fast lane (this thread, ~1 s) reads clocks/temps/fans/load/power,
//! while the slow lane ([`crate::inventory::spawn_collector`], ~30 s) runs on
//! its own thread and publishes into an atomic slot. The fast lane picks up
//! whatever the slow lane last produced — a multi-second S.M.A.R.T. read can
//! therefore never delay a sensor tick.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::inventory::InventoryHandle;
use crate::logging::CsvLogger;
use crate::model::Hardware;
use crate::source::SensorSource;
use crate::state::{now_ms, TelemetryFrame, TelemetryStore};

/// Mutations the UI can ask of the poll thread. Sent over an `mpsc` channel
/// rather than performed under a shared lock, so the render loop never contends
/// with the poller.
#[derive(Debug, Clone)]
pub enum Command {
    /// Rebase min/max/avg to the current readings. Sent by interactive front
    /// ends (GUI "Reset Min/Max", TUI `r`); a one-shot CLI build has no caller.
    #[allow(dead_code)]
    ResetMinMax,
    /// Change the fast-lane interval (clamped on receipt).
    SetInterval(Duration),
    /// Stop the loop promptly, without waiting out the current tick.
    Shutdown,
}

#[derive(Debug, Clone, Copy)]
pub struct PollConfig {
    pub fast: Duration,
    pub slow: Duration,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self { fast: Duration::from_millis(1000), slow: Duration::from_secs(30) }
    }
}

/// Bounds on the fast lane. The lower bound is not arbitrary: below ~250 ms,
/// Super-I/O and SMBus-backed sensors start colliding with firmware access.
pub const MIN_FAST: Duration = Duration::from_millis(250);
pub const MAX_FAST: Duration = Duration::from_millis(10_000);

type TickFn = Box<dyn Fn() + Send + Sync>;

/// The active CSV logger, shared with the UI (which toggles it on and off).
/// Writes happen here, on the poll thread, so file I/O never blocks rendering.
/// A leaf lock: held only for the duration of one row, never while holding
/// anything else.
pub type LoggerSlot = Arc<Mutex<Option<CsvLogger>>>;

/// Handle to the running poll thread.
pub struct PollHandle {
    running: Arc<AtomicBool>,
    /// Kept so `stop()` can *wake* the loop. Clearing `running` alone is not
    /// enough: the thread is parked in `recv_timeout`, so without a message it
    /// would not notice until the current interval elapsed — up to 10 s.
    commands: Sender<Command>,
    /// Called after each publish, to wake an interactive front end. Set once,
    /// after `eframe` has created the context; `OnceLock` keeps the hot path
    /// atomic-read cheap. A CLI-only build never registers one, so the lock
    /// stays empty and the poll loop skips the wake.
    #[allow(dead_code)]
    on_tick: Arc<OnceLock<TickFn>>,
    join: Option<JoinHandle<()>>,
}

impl PollHandle {
    /// A sender for [`Command`]s, handed to the UI.
    pub fn sender(&self) -> Sender<Command> {
        self.commands.clone()
    }

    /// Register the repaint callback. Called once from the eframe setup closure.
    #[allow(dead_code)]
    pub fn on_tick(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.on_tick.set(Box::new(f));
    }

    /// Signal the thread to stop and wait for it. Idempotent.
    ///
    /// The loop waits on the command channel rather than sleeping, so shutdown
    /// is observed immediately instead of after the current interval.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Wake the loop out of recv_timeout so shutdown is immediate rather
        // than "some time within the polling interval".
        let _ = self.commands.send(Command::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for PollHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawn Thread 1. Owns the command channel; the UI gets a sender via
/// [`PollHandle::sender`].
pub fn spawn(
    store: Arc<TelemetryStore>,
    source: Box<dyn SensorSource>,
    inventory: InventoryHandle,
    logger: LoggerSlot,
    config: PollConfig,
) -> PollHandle {
    let (cmd_tx, commands) = channel();
    let running = Arc::new(AtomicBool::new(true));
    let on_tick: Arc<OnceLock<TickFn>> = Arc::new(OnceLock::new());

    let join = std::thread::Builder::new()
        .name("sensor-poll".into())
        .spawn({
            let running = running.clone();
            let on_tick = on_tick.clone();
            move || run(store, source, inventory, logger, commands, config, running, on_tick)
        })
        .expect("spawning the poll thread");

    PollHandle { running, commands: cmd_tx, on_tick, join: Some(join) }
}

#[allow(clippy::too_many_arguments)]
fn run(
    store: Arc<TelemetryStore>,
    source: Box<dyn SensorSource>,
    inventory: InventoryHandle,
    logger: LoggerSlot,
    commands: Receiver<Command>,
    config: PollConfig,
    running: Arc<AtomicBool>,
    on_tick: Arc<OnceLock<TickFn>>,
) {
    let mut monitor = Monitor::new(source);
    let mut interval = config.fast.clamp(MIN_FAST, MAX_FAST);
    let mut seq: u64 = 0;

    while running.load(Ordering::Relaxed) {
        let tick_start = Instant::now();

        seq += 1;
        let frame = TelemetryFrame {
            seq,
            unix_ms: now_ms(),
            tree: monitor.poll(),
            // Lock-free read of whatever the slow lane last produced.
            inventory: inventory.latest(),
            diagnostics: monitor.diagnostics(),
            // Cached in the backend and refreshed on its own slow cadence, so
            // this is a small clone rather than a drive read.
            sensor_storage: monitor.storage_health(),
            source: monitor.source_name().to_string(),
        };
        // CSV logging runs here rather than in the UI so a slow disk stalls
        // neither rendering nor (via the store) the web clients. Log the frame
        // we just built — reading it back from the store would record the
        // *previous* tick.
        if let Ok(mut slot) = logger.lock() {
            if let Some(csv) = slot.as_mut() {
                csv.log(&frame.tree);
            }
        }

        store.publish(frame);

        if let Some(wake) = on_tick.get() {
            wake();
        }

        // Wait out the rest of the tick *on the command channel*, so commands
        // (especially Shutdown) are acted on immediately.
        let deadline = tick_start + interval;
        loop {
            let now = Instant::now();
            if now >= deadline || !running.load(Ordering::Relaxed) {
                break;
            }
            match commands.recv_timeout(deadline - now) {
                Ok(Command::Shutdown) => return,
                Ok(Command::ResetMinMax) => {
                    monitor.reset_min_max();
                    store.clear_history();
                }
                Ok(Command::SetInterval(d)) => interval = d.clamp(MIN_FAST, MAX_FAST),
                Err(RecvTimeoutError::Timeout) => break,
                // The UI dropped its sender: the app is going away.
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Monitor — running statistics
// ---------------------------------------------------------------------------

/// Running statistics for a single sensor, keyed by its identifier.
///
/// History lives in [`TelemetryStore`] rather than here, because the graphs and
/// the web API both read it; keeping one copy behind the store's leaf lock
/// avoids duplicating ~600 samples per sensor.
struct Stat {
    min: f32,
    max: f32,
    sum: f64,
    count: u64,
    last: f32,
}

impl Stat {
    fn new(v: f32) -> Self {
        Self { min: v, max: v, sum: v as f64, count: 1, last: v }
    }

    fn update(&mut self, v: f32) {
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.sum += v as f64;
        self.count += 1;
        self.last = v;
    }

    fn avg(&self) -> f32 {
        if self.count == 0 { 0.0 } else { (self.sum / self.count as f64) as f32 }
    }
}

/// Owns the active source and folds each snapshot into running min/max/avg.
pub struct Monitor {
    source: Box<dyn SensorSource>,
    stats: HashMap<String, Stat>,
}

impl Monitor {
    pub fn new(source: Box<dyn SensorSource>) -> Self {
        Self { source, stats: HashMap::new() }
    }

    pub fn source_name(&self) -> &'static str {
        self.source.name()
    }

    pub fn diagnostics(&self) -> crate::source::Diagnostics {
        self.source.diagnostics()
    }

    pub fn storage_health(&self) -> Vec<crate::model::storage::StorageHealth> {
        self.source.storage_health()
    }

    /// Poll the source once and fold the readings into the running statistics.
    pub fn poll(&mut self) -> Vec<Hardware> {
        let mut tree = self.source.snapshot();
        for hw in &mut tree {
            Self::enrich(hw, &mut self.stats);
        }
        tree
    }

    fn enrich(hw: &mut Hardware, stats: &mut HashMap<String, Stat>) {
        for s in &mut hw.sensors {
            if let Some(v) = s.value {
                // Seed on first sight, otherwise fold in the new reading.
                let stat = stats
                    .entry(s.identifier.clone())
                    .and_modify(|st| st.update(v))
                    .or_insert_with(|| Stat::new(v));
                s.min = Some(stat.min);
                s.max = Some(stat.max);
                s.avg = Some(stat.avg());
            }
        }
        for sub in &mut hw.sub_hardware {
            Self::enrich(sub, stats);
        }
    }

    /// Rebase min/max/avg onto the latest reading. Mirrors OHM's Reset.
    pub fn reset_min_max(&mut self) {
        for stat in self.stats.values_mut() {
            *stat = Stat::new(stat.last);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HardwareType, Sensor, SensorType};

    /// Source that emits one temperature sensor cycling through a fixed sequence.
    struct SeqSource {
        values: Vec<f32>,
        i: usize,
    }

    impl SensorSource for SeqSource {
        fn name(&self) -> &'static str {
            "seq"
        }
        fn snapshot(&mut self) -> Vec<Hardware> {
            let v = self.values[self.i % self.values.len()];
            self.i += 1;
            vec![Hardware {
                identifier: "/t/0".into(),
                name: "t".into(),
                hardware_type: HardwareType::Cpu,
                sensors: vec![Sensor {
                    identifier: "/t/0/temperature/0".into(),
                    name: "temp".into(),
                    sensor_type: SensorType::Temperature,
                    index: 0,
                    value: Some(v),
                    min: None,
                    max: None,
                    avg: None,
                }],
                sub_hardware: vec![],
            }]
        }
    }

    fn only_sensor(tree: &[Hardware]) -> Sensor {
        tree[0].sensors[0].clone()
    }

    #[test]
    fn tracks_min_max_avg_over_polls() {
        let mut m = Monitor::new(Box::new(SeqSource { values: vec![10.0, 30.0, 20.0], i: 0 }));

        let s = only_sensor(&m.poll()); // 10
        assert_eq!((s.min, s.max, s.avg), (Some(10.0), Some(10.0), Some(10.0)));

        m.poll(); // 30
        let s = only_sensor(&m.poll()); // 20 -> seen 10,30,20
        assert_eq!(s.min, Some(10.0));
        assert_eq!(s.max, Some(30.0));
        assert_eq!(s.avg, Some(20.0));
    }

    #[test]
    fn reset_min_max_rebases_to_latest() {
        let mut m = Monitor::new(Box::new(SeqSource { values: vec![10.0, 30.0], i: 0 }));
        m.poll(); // 10
        m.poll(); // 30
        m.reset_min_max();
        let s = only_sensor(&m.poll()); // 10 again
        assert_eq!(s.min, Some(10.0));
        assert_eq!(s.max, Some(30.0)); // 30 was the rebase point, 10 < 30
        assert_eq!(s.avg, Some(20.0)); // (30 + 10) / 2
    }

    #[test]
    fn interval_is_clamped_to_a_bus_safe_range() {
        // Below ~250 ms, SMBus/Super-I/O polling starts colliding with firmware.
        assert_eq!(Duration::from_millis(10).clamp(MIN_FAST, MAX_FAST), MIN_FAST);
        assert_eq!(Duration::from_secs(60).clamp(MIN_FAST, MAX_FAST), MAX_FAST);
        assert_eq!(Duration::from_millis(500).clamp(MIN_FAST, MAX_FAST), Duration::from_millis(500));
    }

    #[test]
    fn poll_thread_publishes_and_stops_promptly() {
        let store = Arc::new(TelemetryStore::new(16));
        let mut handle = spawn(
            store.clone(),
            Box::new(SeqSource { values: vec![21.0], i: 0 }),
            crate::inventory::InventoryHandle::inert(),
            LoggerSlot::default(),
            // The *maximum* interval on purpose: the loop parks in
            // `recv_timeout`, so if `stop()` only cleared the running flag it
            // would not be noticed for a full 10 s. This is the regression this
            // test exists to catch.
            PollConfig { fast: MAX_FAST, slow: Duration::from_secs(30) },
        );

        // First frame is published before the thread ever waits.
        let deadline = Instant::now() + Duration::from_secs(5);
        while store.load().seq == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let f = store.load();
        assert!(f.seq >= 1, "poller published a frame");
        assert_eq!(f.source, "seq");
        assert_eq!(f.tree[0].sensors[0].value, Some(21.0));

        let t0 = Instant::now();
        handle.stop();
        assert!(t0.elapsed() < Duration::from_secs(1), "stop took {:?}", t0.elapsed());
        handle.stop(); // idempotent
    }

    #[test]
    fn ui_commands_reach_the_poll_thread() {
        let store = Arc::new(TelemetryStore::new(16));
        let mut handle = spawn(
            store.clone(),
            Box::new(SeqSource { values: vec![10.0, 30.0], i: 0 }),
            crate::inventory::InventoryHandle::inert(),
            LoggerSlot::default(),
            PollConfig { fast: MIN_FAST, slow: Duration::from_secs(30) },
        );
        let tx = handle.sender();

        // Let a couple of ticks build up history and a min/max spread.
        let deadline = Instant::now() + Duration::from_secs(5);
        while store.load().seq < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!store.history("/t/0/temperature/0").is_empty());

        tx.send(Command::ResetMinMax).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !store.history("/t/0/temperature/0").is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // ResetMinMax clears history as well as rebasing the statistics.
        assert!(store.history("/t/0/temperature/0").len() <= 1);

        handle.stop();
    }
}
