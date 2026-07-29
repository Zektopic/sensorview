fn main() {
    #[cfg(windows)]
    windows_resources();
}

// `winresource` is a Windows-host-only build-dependency (see Cargo.toml), so
// this whole function must be cfg'd out elsewhere — the `if` below is a runtime
// check on the *target*, which is not enough to keep the crate path resolvable
// when building on macOS/Linux.
#[cfg(windows)]
fn windows_resources() {
    // Embed the app icon + version info into the Windows executable, and — for
    // release builds only — a requireAdministrator manifest so the app elevates
    // at launch like HWiNFO does (full sensor access needs admin: Super-I/O,
    // MSR, SMBus via the kernel driver). Debug builds stay asInvoker so
    // `cargo test` / dev runs don't trip UAC.
    //
    // macOS needs no equivalent: sensors there are read through IOKit with no
    // driver and no elevation, and cargo-packager generates the Info.plist.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if std::env::var("PROFILE").as_deref() == Ok("release") {
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
