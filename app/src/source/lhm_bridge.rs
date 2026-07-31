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

pub struct LhmBridge {
    child: Option<Child>,
    latest: Arc<Mutex<Vec<Hardware>>>,
    meta: Arc<Mutex<Option<BridgeMeta>>>,
    error_msg: String,
}

const SIDECAR_EXE: &str = "sensorview-bridge.exe";

impl LhmBridge {
    pub fn empty(err: String) -> Self {
        Self {
            child: None,
            latest: Arc::new(Mutex::new(Vec::new())),
            meta: Arc::new(Mutex::new(None)),
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
        let sink = latest.clone();
        let meta_sink = meta.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // The first line is the diagnostics meta object; the rest are
                // hardware-tree arrays.
                if let Ok(m) = serde_json::from_str::<MetaLine>(&line) {
                    if let Ok(mut slot) = meta_sink.lock() {
                        *slot = Some(m.meta);
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
