//! LibreHardwareMonitor bridge backend (Windows).
//!
//! Spawns the bundled .NET sidecar (`sensorview-bridge.exe`, see `sidecar/`)
//! which prints one JSON hardware-tree snapshot per line on stdout, in exactly
//! the shape of [`crate::model`]. A reader thread keeps the latest parsed tree;
//! [`SensorSource::snapshot`] hands out clones of it.
//!
//! Full coverage (Super-I/O, MSR, SMBus) requires the app to run elevated —
//! the release build carries a `requireAdministrator` manifest, matching
//! HWiNFO's own behavior. Without elevation the sidecar still reports the
//! subset it can reach (GPU, storage, network, battery, …).

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::model::storage::{
    attribute_name, HealthStatus, Ideal, SmartAttribute, StorageHealth, StorageProtocol,
};
use crate::model::Hardware;

/// Driver / elevation diagnostics from the sidecar's first line — surfaced in
/// the Settings → Driver Management tab to explain zero sensors.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BridgeMeta {
    pub lhm_version: String,
    /// Present in the JSON but unused — elevation is detected app-side
    /// ([`crate::sysinfo::is_elevated`]), which is authoritative.
    #[allow(dead_code)]
    pub is_elevated: bool,
    pub ring0_report: String,
}

#[derive(Deserialize)]
struct MetaLine {
    meta: BridgeMeta,
}

/// The sidecar's drive-health line, tagged so it is distinguishable from a
/// hardware-tree snapshot without inspecting the shape.
#[derive(Deserialize)]
struct StorageLine {
    storage: Vec<WireDrive>,
}

/// One drive as the sidecar sends it.
///
/// A separate type from [`StorageHealth`] on purpose: this mirrors what
/// LibreHardwareMonitor happens to expose, and the conversion below is where
/// that becomes the model the rest of the app is written against. Keeping them
/// welded together would let a sidecar change reach straight into the UI.
#[derive(Deserialize)]
struct WireDrive {
    identifier: String,
    model: String,
    serial: String,
    firmware: String,
    protocol: String,
    capacity_bytes: Option<u64>,
    temperature_c: Option<f32>,
    power_on_hours: Option<u64>,
    power_cycles: Option<u64>,
    life_remaining_pct: Option<f32>,
    /// LHM reports host reads/writes in gigabytes.
    host_reads_gb: Option<f64>,
    host_writes_gb: Option<f64>,
    #[serde(default)]
    attributes: Vec<WireAttribute>,
}

#[derive(Deserialize)]
struct WireAttribute {
    id: u8,
    name: String,
    current: u8,
    worst: u8,
    threshold: u8,
    raw: u64,
}

impl WireDrive {
    fn into_health(self) -> StorageHealth {
        let attributes: Vec<SmartAttribute> = self
            .attributes
            .into_iter()
            .map(|a| SmartAttribute {
                // Our own table knows which direction is healthy, which LHM
                // does not report; fall back to its name when the id is
                // vendor-specific and ours has no entry.
                ideal: attribute_name(a.id).map_or(Ideal::None, |(_, i)| i),
                name: attribute_name(a.id).map_or(a.name, |(n, _)| n.to_string()),
                id: a.id,
                current: a.current,
                worst: a.worst,
                threshold: a.threshold,
                raw: a.raw,
            })
            .collect();

        let gb_to_bytes = |gb: f64| (gb * 1_000_000_000.0) as u128;
        let mut health = StorageHealth {
            identifier: self.identifier,
            model: self.model.trim().to_string(),
            serial: self.serial.trim().to_string(),
            firmware: self.firmware.trim().to_string(),
            protocol: match self.protocol.as_str() {
                "Nvme" => StorageProtocol::Nvme,
                "Scsi" => StorageProtocol::Scsi,
                _ => StorageProtocol::Ata,
            },
            capacity_bytes: self.capacity_bytes,
            temperature_c: self.temperature_c,
            power_on_hours: self.power_on_hours,
            power_cycles: self.power_cycles,
            life_remaining_pct: self.life_remaining_pct,
            total_bytes_written: self.host_writes_gb.map(gb_to_bytes),
            total_bytes_read: self.host_reads_gb.map(gb_to_bytes),
            status: HealthStatus::Unknown,
            warnings: Vec::new(),
            attributes,
            nvme: None,
        };
        let (status, warnings) = assess(&health);
        health.status = status;
        health.warnings = warnings;
        health
    }
}

/// Decide a drive's overall status from the evidence we have.
///
/// Deliberately ours rather than the sidecar's: LHM's own `DiskStatus` is a
/// single opaque word, and a health verdict that a user is expected to act on
/// should be able to say *why*. Every non-`Good` result carries its reason.
fn assess(h: &StorageHealth) -> (HealthStatus, Vec<String>) {
    let mut warnings = Vec::new();
    let mut status = HealthStatus::Good;
    let mut worst = |s: HealthStatus| {
        // Bad outranks Caution; nothing downgrades a verdict once reached.
        if matches!(s, HealthStatus::Bad) || matches!(status, HealthStatus::Good) {
            status = s;
        }
    };

    for a in &h.attributes {
        if a.failing() {
            warnings.push(format!(
                "{} ({}) is at {} against a threshold of {}",
                a.name, a.id, a.current, a.threshold
            ));
            worst(HealthStatus::Bad);
        }
    }

    // Reallocated or pending sectors mean the drive has already had to move
    // data off failing media. Not yet fatal, but never normal.
    for id in [0x05u8, 0xC5, 0xC6] {
        if let Some(a) = h.attributes.iter().find(|a| a.id == id) {
            if a.raw > 0 {
                warnings.push(format!("{} = {}", a.name, a.raw));
                worst(HealthStatus::Caution);
            }
        }
    }

    if let Some(life) = h.life_remaining_pct {
        if life <= 10.0 {
            warnings.push(format!("{life:.0}% of rated endurance remaining"));
            worst(HealthStatus::Caution);
        }
    }

    // No attributes and no endurance figure is "we cannot see", not "healthy" —
    // the distinction this codebase keeps everywhere else.
    if h.attributes.is_empty() && h.life_remaining_pct.is_none() && h.temperature_c.is_none() {
        return (HealthStatus::Unknown, warnings);
    }
    (status, warnings)
}

pub struct LhmBridge {
    child: Option<Child>,
    latest: Arc<Mutex<Vec<Hardware>>>,
    meta: Arc<Mutex<Option<BridgeMeta>>>,
    /// Latest drive health. Refreshed on the sidecar's slow cadence, not every
    /// tick — re-reading S.M.A.R.T. at 1 Hz keeps drives out of low-power
    /// states, so the sidecar sends it rarely and this just holds the last one.
    storage: Arc<Mutex<Vec<StorageHealth>>>,
    error_msg: String,
}

const SIDECAR_EXE: &str = "sensorview-bridge.exe";

impl LhmBridge {
    pub fn empty(err: String) -> Self {
        Self {
            child: None,
            latest: Arc::new(Mutex::new(Vec::new())),
            meta: Arc::new(Mutex::new(None)),
            storage: Arc::new(Mutex::new(Vec::new())),
            error_msg: err,
        }
    }

    /// Spawn the sidecar and wait (briefly) for its first snapshot.
    pub fn spawn() -> Result<Self, String> {
        let exe = find_sidecar().ok_or_else(|| format!("{SIDECAR_EXE} not found"))?;

        // Kill any stale sidecars first. Orphans from a previous crash keep the
        // WinRing0 driver / AMD SMU open; a second instance then contends for it
        // and the SMU-derived sensors (package/core power, effective clocks)
        // read 0 while VIDs still work — exactly the "0 W / 0 MHz" symptom.
        #[cfg(windows)]
        kill_stale_sidecars();

        let mut cmd = Command::new(&exe);
        cmd.stdout(Stdio::piped()).stderr(Stdio::null());
        // So the sidecar can watch us and self-exit if we die (no orphans).
        cmd.env("SENSORVIEW_PARENT_PID", std::process::id().to_string());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn {}: {e}", exe.display()))?;
        let stdout = child.stdout.take().ok_or("sidecar stdout unavailable")?;

        let latest: Arc<Mutex<Vec<Hardware>>> = Arc::new(Mutex::new(Vec::new()));
        let meta: Arc<Mutex<Option<BridgeMeta>>> = Arc::new(Mutex::new(None));
        let storage: Arc<Mutex<Vec<StorageHealth>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = latest.clone();
        let meta_sink = meta.clone();
        let storage_sink = storage.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // Three line kinds: the diagnostics meta object (first), an
                // occasional tagged drive-health object, and hardware-tree
                // arrays for everything else.
                if let Ok(m) = serde_json::from_str::<MetaLine>(&line) {
                    if let Ok(mut slot) = meta_sink.lock() {
                        *slot = Some(m.meta);
                    }
                    continue;
                }
                if let Ok(s) = serde_json::from_str::<StorageLine>(&line) {
                    let drives: Vec<StorageHealth> =
                        s.storage.into_iter().map(WireDrive::into_health).collect();
                    if let Ok(mut slot) = storage_sink.lock() {
                        *slot = drives;
                    }
                    continue;
                }
                if let Ok(tree) = serde_json::from_str::<Vec<Hardware>>(&line) {
                    if let Ok(mut slot) = sink.lock() {
                        *slot = tree;
                    }
                }
            }
        });

        // LHM's Computer.Open() enumerates all buses; give it a moment. Return
        // Ok even if initial enumeration is still completing so the background
        // reader thread populates `latest` as soon as the first snapshot lands.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if !latest.lock().map(|t| t.is_empty()).unwrap_or(true) {
                return Ok(Self {
                    child: Some(child),
                    latest,
                    meta,
                    storage,
                    error_msg: String::new(),
                });
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("sidecar exited early: {status}"));
            }
            if Instant::now() >= deadline {
                return Ok(Self {
                    child: Some(child),
                    latest,
                    meta,
                    storage,
                    error_msg: String::new(),
                });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl super::SensorSource for LhmBridge {
    fn name(&self) -> &'static str {
        "LibreHardwareMonitor bridge"
    }

    fn snapshot(&mut self) -> Vec<Hardware> {
        self.latest.lock().map(|t| t.clone()).unwrap_or_default()
    }

    fn storage_health(&self) -> Vec<StorageHealth> {
        self.storage.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn diagnostics(&self) -> super::Diagnostics {
        let meta = self.meta.lock().ok().and_then(|m| m.clone()).unwrap_or_default();
        let driver_report = if !self.error_msg.is_empty() {
            format!("Bridge error: {}", self.error_msg)
        } else {
            meta.ring0_report
        };
        super::Diagnostics {
            engine_version: if meta.lhm_version.is_empty() {
                String::new()
            } else {
                format!("LibreHardwareMonitor {}", meta.lhm_version)
            },
            driver_report,
        }
    }
}

impl Drop for LhmBridge {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Force-kill any leftover sidecar processes (orphans from a prior crash).
/// When we run elevated this also clears elevated orphans that would otherwise
/// hog the SMU and zero out power/clock sensors.
#[cfg(windows)]
fn kill_stale_sidecars() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", SIDECAR_EXE])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    // Give the driver a moment to release before the fresh instance opens it.
    std::thread::sleep(Duration::from_millis(300));
}

/// Locate the sidecar: next to our exe (packaged install), then the dev
/// publish folder (repo checkout), then — in a `portable` build — the copy
/// carried inside the binary.
fn find_sidecar() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            candidates.push(dir.join(SIDECAR_EXE));
            candidates.push(dir.join("sidecar").join(SIDECAR_EXE));
        }
    }
    // Developer conveniences only. CARGO_MANIFEST_DIR is baked in at compile
    // time, so in a shipped binary these point at whatever directory the build
    // machine used. That path will not normally exist on a user's system, but
    // if it ever did — and were writable — it would be a path from which we
    // would happily launch an executable. A release build looks next to itself
    // and nowhere else.
    #[cfg(debug_assertions)]
    {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(
        manifest_dir
            .join("sidecar")
            .join("publish")
            .join(SIDECAR_EXE),
    );
    candidates.push(
        manifest_dir
            .join("sidecar")
            .join("bin")
            .join("Release")
            .join("net8.0")
            .join("win-x64")
            .join(SIDECAR_EXE),
    );
    candidates.push(
        manifest_dir
            .join("sidecar")
            .join("bin")
            .join("Debug")
            .join("net8.0")
            .join("win-x64")
            .join(SIDECAR_EXE),
    );
    }
    if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(found);
    }

    // Last: the copy a portable build carries inside itself. Deliberately after
    // the on-disk candidates, so an installed layout or a developer's freshly
    // published sidecar still wins — and so the 72 MB unpack only happens when
    // there is genuinely nothing else to run.
    #[cfg(all(windows, feature = "portable"))]
    {
        return crate::portable::ensure_sidecar();
    }
    #[cfg(not(all(windows, feature = "portable")))]
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drive line exactly as the sidecar emits it, so the parse is exercised
    /// against the real wire shape. This machine cannot enumerate drives
    /// without elevation, so a fixture is the only way this path is covered at
    /// all — if the sidecar's field names drift, this is what catches it.
    const SAMPLE: &str = r#"{"storage":[{
        "identifier":"/nvme/0","model":" Samsung SSD 990 PRO 2TB ","serial":"S7DZNU0X ",
        "firmware":"4B2QJXD7","protocol":"Nvme","is_ssd":true,"bus":"BusTypeNvme",
        "capacity_bytes":2000398934016,"free_bytes":812345678,"temperature_c":41.0,
        "power_on_hours":1234,"power_cycles":57,"life_remaining_pct":98.0,
        "host_reads_gb":18000.0,"host_writes_gb":24000.0,"nand_writes_gb":26000.0,
        "status":"Good","attributes":[
            {"id":5,"name":"Vendor Name","current":100,"worst":100,"threshold":10,"raw":0},
            {"id":9,"name":"Power On Hours","current":99,"worst":99,"threshold":0,"raw":1234}
        ]}]}"#;

    fn parse(s: &str) -> Vec<StorageHealth> {
        serde_json::from_str::<StorageLine>(s)
            .expect("sample must parse")
            .storage
            .into_iter()
            .map(WireDrive::into_health)
            .collect()
    }

    #[test]
    fn parses_a_drive_line_from_the_sidecar() {
        let d = &parse(SAMPLE)[0];
        // Identity strings arrive padded off the drive; they are trimmed so the
        // UI does not render ragged columns.
        assert_eq!(d.model, "Samsung SSD 990 PRO 2TB");
        assert_eq!(d.serial, "S7DZNU0X");
        assert_eq!(d.protocol, StorageProtocol::Nvme);
        assert_eq!(d.temperature_c, Some(41.0));
        assert_eq!(d.power_on_hours, Some(1234));
        assert_eq!(d.life_remaining_pct, Some(98.0));
        // LHM reports host traffic in GB; the model stores bytes.
        assert_eq!(d.total_bytes_written, Some(24_000_000_000_000));
        assert_eq!(d.status, HealthStatus::Good);
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    /// Our own attribute table supplies the canonical name and the healthy
    /// direction, neither of which the sidecar knows.
    #[test]
    fn attribute_names_come_from_our_table_not_the_wire() {
        let d = &parse(SAMPLE)[0];
        let realloc = d.attributes.iter().find(|a| a.id == 5).unwrap();
        assert_eq!(realloc.name, "Reallocated Sectors Count");
        assert_eq!(realloc.ideal, Ideal::Low);
    }

    /// A normalised value at or below its threshold is the one unambiguous
    /// "this drive is failing" signal S.M.A.R.T. offers.
    #[test]
    fn an_attribute_under_its_threshold_is_bad_and_says_why() {
        let failing = SAMPLE.replace(
            r#""id":5,"name":"Vendor Name","current":100"#,
            r#""id":5,"name":"Vendor Name","current":8"#,
        );
        let d = &parse(&failing)[0];
        assert_eq!(d.status, HealthStatus::Bad);
        assert!(
            d.warnings.iter().any(|w| w.contains("Reallocated") && w.contains("threshold")),
            "a Bad verdict must explain itself: {:?}",
            d.warnings
        );
    }

    /// Reallocated sectors are not yet fatal but are never normal, so they
    /// downgrade to Caution rather than passing as Good.
    #[test]
    fn reallocated_sectors_downgrade_to_caution() {
        let worn = SAMPLE.replace(
            r#""threshold":10,"raw":0"#,
            r#""threshold":10,"raw":7"#,
        );
        let d = &parse(&worn)[0];
        assert_eq!(d.status, HealthStatus::Caution);
        assert!(d.warnings.iter().any(|w| w.contains('7')), "{:?}", d.warnings);
    }

    /// A drive we cannot read must not be reported as healthy — the same
    /// unknown-vs-zero distinction the sensor model makes everywhere.
    #[test]
    fn a_drive_with_no_readable_health_is_unknown_not_good() {
        let blank = r#"{"storage":[{"identifier":"/hdd/0","model":"Mystery","serial":"",
            "firmware":"","protocol":"Ata","capacity_bytes":null,"temperature_c":null,
            "power_on_hours":null,"power_cycles":null,"life_remaining_pct":null,
            "host_reads_gb":null,"host_writes_gb":null,"attributes":[]}]}"#;
        assert_eq!(parse(blank)[0].status, HealthStatus::Unknown);
    }

    /// The three line kinds must stay mutually exclusive: a tree snapshot must
    /// never be mistaken for drive health, or a tick would blank the table.
    #[test]
    fn a_tree_snapshot_is_not_mistaken_for_a_storage_line() {
        let tree = r#"[{"identifier":"/amdcpu/0","name":"CPU","type":"Cpu","sensors":[],"sub_hardware":[]}]"#;
        assert!(serde_json::from_str::<StorageLine>(tree).is_err());
        assert!(serde_json::from_str::<MetaLine>(tree).is_err());
    }
}
