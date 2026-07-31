//! The portable Windows build: the sensor sidecar travels inside the binary.
//!
//! A single `sensorview.exe` is not a portable app on Windows on its own.
//! Every sensor on this platform comes from the LibreHardwareMonitor sidecar,
//! and [`crate::source::default_source`] falls back to `LhmBridge::empty` when
//! it cannot find one — so a lone executable opens to an empty window. Building
//! with `--features portable` compiles the sidecar in (see `build.rs`); this
//! module unpacks it on first run so there is something to spawn.
//!
//! Unpacked to a per-version directory under the local app data folder rather
//! than beside the executable: a portable binary is expected to run from
//! read-only media, a network share or a USB stick, and must not assume it can
//! write next to itself.

use std::path::PathBuf;

/// The sidecar, as copied into `OUT_DIR` by `build.rs`.
const BRIDGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bridge.bin"));

const SIDECAR_EXE: &str = "sensorview-bridge.exe";

/// Unpack the embedded sidecar if needed and return its path.
///
/// `None` when there is nowhere to write it, which leaves the caller to carry
/// on with its other candidates rather than failing outright.
pub fn ensure_sidecar() -> Option<PathBuf> {
    // Keyed by version *and* length so an upgraded app never reuses the
    // previous release's sidecar, which would silently pair a new front end
    // with an old bridge protocol.
    let dir = dirs::data_local_dir()?
        .join("SensorView")
        .join("bridge")
        .join(format!("{}-{}", env!("CARGO_PKG_VERSION"), BRIDGE.len()));
    let exe = dir.join(SIDECAR_EXE);

    if is_complete(&exe) {
        return Some(exe);
    }

    std::fs::create_dir_all(&dir).ok()?;

    // Write to a private name and rename into place. A half-written 72 MB file
    // that the *next* run mistakes for a complete one is worse than not
    // unpacking at all, and two instances starting together must not interleave
    // their writes into the same file.
    let tmp = dir.join(format!("{SIDECAR_EXE}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, BRIDGE).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }

    match std::fs::rename(&tmp, &exe) {
        Ok(()) => Some(exe),
        Err(_) => {
            // On Windows rename fails when the destination exists, which is
            // exactly what another instance winning the race looks like. Use
            // its copy if it is complete.
            let _ = std::fs::remove_file(&tmp);
            is_complete(&exe).then_some(exe)
        }
    }
}

/// Whether an already-unpacked sidecar is the one we carry, judged by size.
///
/// Cheap on purpose: this runs on every start, and re-hashing 72 MB to prove
/// what the per-version directory name already implies would cost more than it
/// tells us.
fn is_complete(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() == BRIDGE.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The build script must have supplied a real sidecar. An empty blob would
    /// compile, ship, and then fail at runtime with nothing to spawn — the one
    /// failure this feature exists to prevent.
    #[test]
    fn the_embedded_sidecar_is_a_real_executable() {
        assert!(
            BRIDGE.len() > 1024 * 1024,
            "embedded sidecar is only {} bytes — did build.rs copy it?",
            BRIDGE.len()
        );
        assert_eq!(&BRIDGE[..2], b"MZ", "embedded blob is not a PE executable");
    }

    /// The unpack directory has to change when the payload does, or an upgrade
    /// keeps running the previous release's bridge.
    #[test]
    fn the_unpack_path_is_keyed_to_this_build() {
        let Some(p) = ensure_sidecar() else {
            eprintln!("SKIP: no local data directory on this machine");
            return;
        };
        let key = format!("{}-{}", env!("CARGO_PKG_VERSION"), BRIDGE.len());
        assert!(
            p.to_string_lossy().contains(&key),
            "unpack path {p:?} is not keyed by version and payload size"
        );
        assert!(is_complete(&p), "unpacked sidecar is the wrong size");
    }
}
