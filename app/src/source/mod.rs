//! Sensor data sources.
//!
//! Everything the app displays comes through the [`SensorSource`] trait, so the
//! backend that actually reads hardware can change without touching the polling
//! engine or the UI. Two backends are planned:
//!
//! - `demo` — synthetic data (this branch), keeps the app useful with no drivers.
//! - `lhm_bridge` — LibreHardwareMonitor .NET sidecar (feature/lhm-bridge).
//! - `native` — pure-Rust engine (feature/native-*).

pub mod demo;
pub mod firmware;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(windows)]
pub mod lhm_bridge;
#[cfg(target_os = "macos")]
pub mod macos;

use serde::{Deserialize, Serialize};

use crate::model::Hardware;

/// Backend health/driver diagnostics, surfaced in Settings → Driver Management
/// to explain why some sensors read zero. Serialized into the telemetry frame
/// so the web dashboard can show the same explanation as the native UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Diagnostics {
    /// Sensor-engine version string.
    pub engine_version: String,
    /// Kernel-driver report (WinRing0 open/install status, blocklist errors).
    pub driver_report: String,
}

/// A backend that can produce a full snapshot of the machine's hardware tree.
///
/// Implementations are polled on a fixed interval by [`crate::poll::Monitor`];
/// they should return current instantaneous values and leave min/max/avg
/// tracking to the monitor.
pub trait SensorSource: Send {
    /// Human-readable backend name (shown in the status bar / about box).
    fn name(&self) -> &'static str;

    /// Read a fresh snapshot of the hardware tree. Called once per poll tick.
    fn snapshot(&mut self) -> Vec<Hardware>;

    /// Driver/elevation diagnostics (default: none).
    fn diagnostics(&self) -> Diagnostics {
        Diagnostics::default()
    }

    /// Per-drive S.M.A.R.T. / health (default: none).
    ///
    /// Read every tick like [`Self::diagnostics`], but backends must return a
    /// *cached* value rather than querying the drive here: re-reading
    /// S.M.A.R.T. at sensor cadence keeps disks out of low-power states and
    /// burns a limited log-read budget. Refreshing it on a slow cadence is the
    /// backend's job.
    fn storage_health(&self) -> Vec<crate::model::storage::StorageHealth> {
        Vec::new()
    }
}

/// Pick the best available source for the current build/platform.
///
/// Windows: the LibreHardwareMonitor bridge (full real sensors), falling back
/// to the demo source if the sidecar is missing or fails. macOS: the native
/// IOKit backend — no sidecar, no kernel driver, no elevation. Linux: sysfs
/// hwmon. Anything else (and `SENSORVIEW_SOURCE=demo`): the demo source.
///
/// Every platform arm must also be excluded from the fallback's `not(any(…))`.
/// These are exclusive `cfg` blocks in sequence, not `else if`, so a platform
/// listed in an arm *and* left in the fallback compiles both — and the last one
/// wins, which presents as "my backend silently isn't being used".
pub fn default_source() -> Box<dyn SensorSource> {
    if std::env::var("SENSORVIEW_SOURCE").as_deref() == Ok("demo") {
        return Box::new(demo::DemoSource::new());
    }
    #[cfg(windows)]
    {
        match lhm_bridge::LhmBridge::spawn() {
            Ok(bridge) => Box::new(bridge),
            Err(e) => {
                eprintln!("LHM bridge error: {e}");
                Box::new(lhm_bridge::LhmBridge::empty(e))
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacSource::new())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxSysfsSource::new())
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Box::new(demo::DemoSource::new())
    }
}
