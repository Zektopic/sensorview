fn main() {
    #[cfg(windows)]
    windows_resources();
    embed_portable_sidecar();
}

/// `--features portable` compiles the LibreHardwareMonitor sidecar *into* the
/// binary, so one .exe is genuinely self-contained.
///
/// Without it a lone `sensorview.exe` on Windows is not a portable build at
/// all: `default_source()` falls back to `LhmBridge::empty` when the sidecar
/// is not on disk beside it, and the app opens reporting no sensors whatsoever.
///
/// Only meaningful for a Windows target — macOS and Linux read their sensors
/// natively and have no sidecar to carry.
fn embed_portable_sidecar() {
    println!("cargo:rerun-if-changed=sidecar/publish/sensorview-bridge.exe");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_PORTABLE");

    if std::env::var("CARGO_FEATURE_PORTABLE").is_err()
        || std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
    {
        return;
    }

    let src = std::path::Path::new("sidecar/publish/sensorview-bridge.exe");
    if !src.exists() {
        // Failing loudly beats emitting a portable build whose whole selling
        // point silently does not work.
        panic!(
            "--features portable needs the sidecar at {}; run \
             `dotnet publish sidecar -c Release -o sidecar/publish` first",
            src.display()
        );
    }

    let out = std::path::PathBuf::from(
        std::env::var("OUT_DIR").expect("OUT_DIR is always set for a build script"),
    );
    std::fs::copy(src, out.join("bridge.bin")).expect("copy the sidecar into OUT_DIR");
}

// `winresource` is a Windows-host-only build-dependency (see Cargo.toml), so
// this whole function must be cfg'd out elsewhere — the `if` below is a runtime
// check on the *target*, which is not enough to keep the crate path resolvable
// when building on macOS/Linux.
#[cfg(windows)]
fn windows_resources() {
    // Embed the app icon + version info into the Windows executable, and — for
    // release GUI builds only — a requireAdministrator manifest so the app
    // elevates at launch like HWiNFO does (full sensor access needs admin:
    // Super-I/O, MSR, SMBus via the kernel driver). Debug builds stay asInvoker
    // so `cargo test` / dev runs don't trip UAC.
    //
    // Gated on the `gui` feature as well as the profile. Keying on the profile
    // alone also stamped the manifest onto the *headless* release binary — the
    // one offered for servers and containers — and a non-elevated parent then
    // could not start it at all: CreateProcess fails outright with
    // ERROR_ELEVATION_REQUIRED, with no UAC prompt available to a service,
    // scheduled task or container entrypoint. A CLI that cannot see MSR
    // sensors should report fewer sensors, not refuse to launch.
    //
    // macOS needs no equivalent: sensors there are read through IOKit with no
    // driver and no elevation, and cargo-packager generates the Info.plist.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if std::env::var("PROFILE").as_deref() == Ok("release")
            && std::env::var("CARGO_FEATURE_GUI").is_ok()
        {
            res.set_manifest(
                r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>"#,
            );
        }
        res.compile().expect("failed to embed Windows resources");
    }
}
