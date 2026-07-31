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

- **The tag→version stamping in `release.yml` has never actually run.** The logic
  is unit-tested against the real `Cargo.toml` and `.iss` files, but no `v*` tag
  has been pushed yet. The first release is the real test.
