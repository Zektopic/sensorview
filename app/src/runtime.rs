//! Process-wide startup and shutdown, independent of any front end.
//!
//! Every mode — GUI, `sensors`/`get`, `stream`, `top`, `daemon` — needs the
//! same pipeline: telemetry store, static system info, slow-lane inventory
//! collector, hardware poller, and (optionally) the web server. This module
//! owns that sequence so the front ends don't each reimplement it, and so the
//! documented shutdown order lives in exactly one place.
//!
//! Nothing here references `eframe`, `egui` or `crate::ui`: it must compile
//! with `--no-default-features`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::settings::AppSettings;
use crate::state::{TelemetryFrame, TelemetryStore};
use crate::sysinfo::SystemInfoHandle;
use crate::{inventory, poll, source, sysinfo};

/// How many ticks a broadcast subscriber may fall behind before it is told it
/// lagged and resyncs. Small on purpose: for live telemetry, the newest frame
/// is the only interesting one.
const BROADCAST_CAPACITY: usize = 16;

/// Everything the process owns, in the order it was started.
pub struct Runtime {
    pub store: Arc<TelemetryStore>,
    pub sysinfo: SystemInfoHandle,
    /// Shared with whichever front end can toggle logging; the poll thread
    /// does the writing.
    pub logger: poll::LoggerSlot,
    pub poller: poll::PollHandle,
    #[cfg(feature = "web")]
    pub web: crate::web::WebHandle,
    pub started: Instant,
}

/// Start the pipeline. Returns once the poller is running and the web server
/// has either bound or failed — never blocks waiting for sensor data.
pub fn start(settings: &AppSettings) -> Runtime {
    let store = Arc::new(TelemetryStore::new(BROADCAST_CAPACITY));
    let sysinfo_handle = sysinfo::spawn_query();
    let logger: poll::LoggerSlot = Arc::new(Mutex::new(None));

    let poll_config = poll::PollConfig {
        fast: Duration::from_millis(settings.poll_interval_ms),
        // Floor of 5 s: S.M.A.R.T. and SPD reads are expensive and keep drives
        // awake, so no setting may turn the slow lane into a second fast one.
        slow: Duration::from_secs(settings.inventory_interval_s.max(5)),
    };

    // Thread 1b: slow lane (S.M.A.R.T. / SPD / topology).
    let collector = inventory::spawn_collector(inventory::default_inventory(), poll_config.slow);

    // Thread 1: hardware poller.
    let poller = poll::spawn(
        store.clone(),
        source::default_source(),
        collector,
        logger.clone(),
        poll_config,
    );

    // Thread 3: web dashboard. Started before any front end so a bind failure
    // (port in use) is known before the first frame is drawn, and can be shown
    // rather than guessed at.
    #[cfg(feature = "web")]
    let web = {
        use std::net::{IpAddr, Ipv4Addr};
        crate::web::spawn(
            store.clone(),
            sysinfo_handle.clone(),
            crate::web::WebConfig {
                enabled: settings.web_enabled,
                bind: if settings.web_lan_access {
                    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
                } else {
                    IpAddr::V4(Ipv4Addr::LOCALHOST)
                },
                port: settings.web_port,
            }
            .with_env_overrides(),
        )
    };

    Runtime {
        store,
        sysinfo: sysinfo_handle,
        logger,
        poller,
        #[cfg(feature = "web")]
        web,
        started: Instant::now(),
    }
}

impl Runtime {
    /// Ordered shutdown: release the port first so a quick restart can rebind,
    /// then the sensor driver (which the Windows sidecar holds open). The
    /// inventory collector stops via `Drop` when the poll thread's loop returns.
    ///
    /// Idempotent — both `PollHandle::stop` and `WebHandle::stop` tolerate being
    /// called twice, so a Ctrl-C path and a `Drop` can both run.
    pub fn shutdown(&mut self) {
        #[cfg(feature = "web")]
        self.web.stop();
        self.poller.stop();
    }

    /// Block until a published frame satisfies `ready`, or `timeout` elapses.
    ///
    /// Front ends must not wait merely for `seq > 0`. Rate-derived sensors —
    /// power, clocks and throughput on macOS, CPU load on Linux — are computed
    /// from the delta between two polls and are therefore **absent from the
    /// first frame**. Waiting for the wrong condition makes a cold-start query
    /// silently return nothing.
    ///
    /// Returns the last frame seen on timeout, so callers can report what is
    /// missing rather than hanging.
    pub fn wait_for(
        &self,
        ready: impl Fn(&TelemetryFrame) -> bool,
        timeout: Duration,
    ) -> Result<Arc<TelemetryFrame>, Arc<TelemetryFrame>> {
        let deadline = Instant::now() + timeout;
        loop {
            let frame = self.store.load();
            if ready(&frame) {
                return Ok(frame);
            }
            if Instant::now() >= deadline {
                return Err(frame);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The default readiness condition for anything that displays sensors.
    ///
    /// `seq >= 2` because the second frame is the first one that can carry
    /// rate-derived values (see [`Runtime::wait_for`]).
    pub fn wait_for_usable_frame(
        &self,
        timeout: Duration,
    ) -> Result<Arc<TelemetryFrame>, Arc<TelemetryFrame>> {
        self.wait_for(|f| f.seq >= 2, timeout)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// First sensor identifier whose name contains `needle` (case-insensitive).
///
/// Backs the GUI's `SENSORVIEW_OPEN_GRAPH` dev hook. The CLI uses
/// `cli::render::find_sensor` instead, which returns the sensor itself.
#[cfg(feature = "gui")]
pub fn first_sensor_matching(tree: &[crate::model::Hardware], needle: &str) -> Option<String> {
    fn walk(tree: &[crate::model::Hardware], needle: &str) -> Option<String> {
        for hw in tree {
            for s in &hw.sensors {
                if s.name.to_lowercase().contains(needle) {
                    return Some(s.identifier.clone());
                }
            }
            if let Some(found) = walk(&hw.sub_hardware, needle) {
                return Some(found);
            }
        }
        None
    }
    walk(tree, &needle.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "gui")]
    use crate::model::{Hardware, HardwareType, Sensor, SensorType};

    #[cfg(feature = "gui")]
    fn sensor(name: &str, id: &str) -> Sensor {
        Sensor {
            identifier: id.into(),
            name: name.into(),
            sensor_type: SensorType::Temperature,
            index: 0,
            value: Some(1.0),
            min: None,
            max: None,
            avg: None,
        }
    }

    #[cfg(feature = "gui")]
    fn tree() -> Vec<Hardware> {
        vec![Hardware {
            identifier: "/cpu/0".into(),
            name: "CPU".into(),
            hardware_type: HardwareType::Cpu,
            sensors: vec![sensor("CPU Package Power", "/cpu/0/power/0")],
            sub_hardware: vec![Hardware {
                identifier: "/cpu/0/sio".into(),
                name: "Super I/O".into(),
                hardware_type: HardwareType::SuperIO,
                sensors: vec![sensor("System Fan", "/sio/fan/0")],
                sub_hardware: Vec::new(),
            }],
        }]
    }

    #[cfg(feature = "gui")]
    #[test]
    fn sensor_lookup_is_case_insensitive_and_recurses() {
        assert_eq!(
            first_sensor_matching(&tree(), "package power").as_deref(),
            Some("/cpu/0/power/0")
        );
        // Only present in a sub_hardware node.
        assert_eq!(
            first_sensor_matching(&tree(), "SYSTEM FAN").as_deref(),
            Some("/sio/fan/0")
        );
        assert!(first_sensor_matching(&tree(), "no such sensor").is_none());
    }

    /// Guards the reason `wait_for` exists: a front end must be able to wait
    /// past frame 1, because rate-derived sensors aren't in it.
    #[test]
    fn wait_for_times_out_and_returns_the_last_frame() {
        let store = Arc::new(TelemetryStore::new(4));
        let rt = Runtime {
            store: store.clone(),
            sysinfo: Arc::new(std::sync::RwLock::new(None)),
            logger: Arc::new(Mutex::new(None)),
            poller: poll::spawn(
                store.clone(),
                Box::new(crate::source::demo::DemoSource::new()),
                inventory::spawn_collector(Box::new(inventory::NullInventory), Duration::from_secs(60)),
                Arc::new(Mutex::new(None)),
                poll::PollConfig::default(),
            ),
            #[cfg(feature = "web")]
            web: crate::web::WebHandle::disabled(None),
            started: Instant::now(),
        };

        // A condition that can never hold must time out rather than hang, and
        // hand back whatever the latest frame was.
        let out = rt.wait_for(|f| f.seq == u64::MAX, Duration::from_millis(120));
        assert!(out.is_err(), "impossible condition should time out");
    }
}
