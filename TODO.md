# TODO

Known issues and follow-up work. Things that are *deliberately* not done live in
the README's "Status and known gaps" section instead — this file is for things
that should eventually change.

## Build

### `ring` needs a C compiler (debug on Windows)

Enabling `--features push` pulls in `rustls` → `ring`, which compiles C and
assembly rather than being pure Rust. GitHub's runners all ship a toolchain and
CI is green on all three platforms, but this is a **new build requirement** for
anyone compiling that feature locally — notably MSVC on Windows.

Two known consequences:

- Cross-compiling to `x86_64-pc-windows-msvc` from macOS fails at `ring`'s build
  script for exactly this reason. Windows-target checks have to skip `push`:
  `cargo clippy --target x86_64-pc-windows-msvc --no-default-features --features gui,web,tui`
- A contributor on Windows without Build Tools installed will hit this on a
  default `cargo build`, since `push` is in the default feature set.

Worth investigating: whether `rustls` can be pointed at a pure-Rust crypto
provider (`aws-lc-rs` has the same problem; `rustls` supports pluggable
providers) so `push` needs no C toolchain at all. Until then, document it in the
build instructions.

## Task Manager

Deliberately left out of the first version, in rough order of how much they are
missed:

- **Applications, not just processes.** Mission Center groups a browser's forty
  helper processes under one "Firefox" row. Doing it properly means reading
  `.desktop` files on Linux and bundle identifiers on macOS, then mapping
  processes onto them — a separate collector, not a UI change.
- **A Services page.** systemd-only, so it cannot exist on macOS or Windows. It
  would be a Linux-only tab rather than a parity feature, which is why it was
  not folded in.
- **Per-process GPU usage.** Mission Center gets this from NVTOP. There is no
  public per-process GPU API on macOS at all, so this can only ever be
  platform-partial.
- **Fans in the Performance sidebar.** The sensors already exist on Windows and
  Linux; showing them here is presentation work, not collection.
- **Per-process network I/O.** Not available on macOS without elevated
  privileges, and the CLI would be the honest place to expose it first.

## Platform gaps

- **macOS fan sensors are not implemented.** Fans would come from the same HID
  sensor plane as temperatures, on a different usage page, but development
  happened on a fanless MacBook Air with nothing to read or verify against.
- **Windows CLI console handling is untested.** The `AttachConsole` path and the
  feature-gated `windows_subsystem` attribute compile and are exercised by CI,
  but nobody has run `sensorview.exe get ...` from a real `cmd.exe`.
- **The TUI has never run on a real terminal.** `sensorview top` is covered by
  ratatui's `TestBackend` (layout, values, filtering), but the keystroke handling
  and the panic-hook terminal restoration have only been reasoned about.
- **The Linux GUI has never been rendered.** It compiles and its tests pass in
  CI, but the build server is headless, so every window has only ever been
  *looked at* on macOS. Fonts, DPI scaling and the Task Manager's table layout
  under a real Wayland/X11 session are unverified.
- **macOS `.dmg` is unsigned and un-notarized.** Gatekeeper blocks it; users need
  System Settings → Privacy & Security → Open Anyway.
- **`ttf-parser` is unmaintained** (RUSTSEC-2026-0192). It reaches us through
  `ab_glyph` → egui, so it needs an upstream change. GUI-only: headless builds
  do not contain it.

## Code quality

- **~95 `unsafe` FFI blocks in the macOS backend have not been audited
  line-by-line.** They are all read-only IOKit/Mach calls and each private-API
  signature is documented beside its declaration, but "written carefully" is not
  "audited", and a wrong signature is undefined behaviour.
- **Panic-prone `unwrap`/`expect` in non-test code** — roughly 12 in
  `model/hexblob.rs`, 8 each in `web/mod.rs`, `model/storage.rs` and `main.rs`.
  The release profile is `panic = "abort"`, so any of them takes the whole
  process down.
- **`settings.val_col_width`** is declared, persisted and never read.

## Release

- **`release.yml` has never completed successfully.** The last manual run failed
  because `workflow_dispatch` sets `GITHUB_REF_NAME` to the *branch*, so the
  workflow tried to release a version called `master`. That is fixed — a
  `prepare` job now resolves the version from the typed input, validates it as
  semver and creates the tag — but the rewritten workflow itself has still never
  been executed. The version stamping is unit-tested against the real
  `Cargo.toml` and `.iss` files; the first real release is the actual test.

## Dependency floor

- **MSRV is 1.95, and it is load-bearing.** Cargo honours `rust-version` during
  resolution, so an older toolchain does not fail — it quietly resolves *older
  dependency versions* instead. That is how `sysinfo` 0.38 was first pulled in
  when 0.39 was wanted. Anything that raises the floor again should say so in
  the `Cargo.toml` comment, as the current bump does.
