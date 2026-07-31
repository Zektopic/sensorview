# TODO

Known issues and follow-up work. Things that are *deliberately* not done live in
the README's "Status and known gaps" section instead — this file is for things
that should eventually change.

## Build

### `ring` needs a C compiler

Enabling `--features push` pulls in `rustls` → `ring`, which compiles C and
assembly rather than being pure Rust. This is a **build requirement** for anyone
compiling the default feature set, notably MSVC on Windows.

Verified on Windows 11 with VS 2022 (MSVC 14.36.32532) + Windows SDK
10.0.18362: `cargo build --release` compiles `ring` 0.17.14 and `rustls` 0.23.43
with no workaround beyond running inside the MSVC environment. So the
requirement is real but ordinary — it is now stated in the README's "Build from
source" section rather than left implicit here.

What remains open:

- Cross-compiling to `x86_64-pc-windows-msvc` from macOS still fails at `ring`'s
  build script. Windows-target checks have to skip `push`:
  `cargo clippy --target x86_64-pc-windows-msvc --no-default-features --features gui,web,tui`
- Worth investigating: whether `rustls` can be pointed at a pure-Rust crypto
  provider (`aws-lc-rs` has the same problem; `rustls` supports pluggable
  providers) so `push` needs no C toolchain at all.

## Confirmed Windows bugs

Found by running the **release** binaries on Windows 11 for the first time
(2026-07-31), which is also what retired the old "Windows CLI console handling
is untested" entry. Three reproducible defects, plus the reason CI missed all of
them. None are fixed yet.

What *does* work, so it is not re-investigated: `AttachConsole` itself (a
GUI-subsystem binary's subcommand output does reach a real console), the
feature-gated `windows_subsystem` attribute (GUI subsystem only with `gui` +
release, console otherwise), exit codes (0/1/2 as documented), stdout/stderr
separation, redirection and piping, and the TUI's panic-hook terminal
restoration under `panic = "abort"`.

### `kill` without `--force` can never succeed on Windows

`procs::kill` maps `force = false` to `Signal::Term`, and `sysinfo` supports no
signal but `Kill` on Windows, so the call always returns:

```
sensorview: Term is not supported on this platform
```

Confirmed against a disposable child process: it survived the plain `kill` and
died on `kill --force`.

This also breaks the GUI. The Task Manager's confirmation modal passes
`force = false` for its default **"End process"** button — described in the
dialog as *"SIGTERM — asks the process to exit"* — so on Windows that button can
only ever report an error. Only "Force kill" works. The error is surfaced rather
than swallowed, so this is a capability gap, not a reporting bug.

Fix: on Windows either fall back to `Signal::Kill` with the wording changed to
match, or disable the non-force action rather than offering something the
platform cannot do.

### Every release build demands elevation, including the headless one

`build.rs` gates the `requireAdministrator` manifest on `PROFILE == "release"`
alone, not on the `gui` feature. So the headless binary — the one the README
offers "for servers and containers" — also requires admin. A non-elevated
parent cannot start it at all:

```
CreateProcess FAILED: The requested operation requires elevation   (error 740)
```

With `UseShellExecute = false` there is no UAC prompt to accept: the process
simply never starts, so a service, scheduled task, container entrypoint or CI
step running as a normal user fails outright — and with it every documented
scripting use of the CLI, since nothing can be piped or redirected from a
process that never ran.

Scope of the claim, measured: this is specific to **non-interactive parents**.
An interactive `ShellExecute` launch (double-click, a shortcut, `Start-Process`
without `-NoNewWindow`) would raise a UAC prompt and succeed if the user
consents — that path was not tested here. The failure case is the
server/container/CI one, which is exactly what the headless build exists for.

Fix: gate the manifest on the `gui` feature as well as the profile. An
unelevated CLI should degrade to reading fewer sensors — which is what
`lhm_bridge.rs` already documents — not refuse to launch.

### `--help`, `--version` and every clap error print nothing

In the shipped GUI-subsystem build these produce **zero** console output, while
the same commands on a console-subsystem build print normally. A mistyped
subcommand fails in complete silence — clap's "a similar subcommand exists"
hint is never seen.

Cause: `main()` calls `Cli::parse()`, and clap handles `--help`, `--version` and
all parse errors *inside* that call, printing and exiting there. But
`attach_console()` is not called until `cli::run()`, which is only reached after
parsing succeeds. Everything clap emits goes to a process with no console yet.

Fix: call `attach_console()` at the top of `main()`, before `Cli::parse()`. It
is already a no-op when there is no parent console and on non-Windows builds, so
moving it earlier costs nothing.

### CI cannot catch any of the above — it never runs a release binary

The headless smoke test builds and runs `target/debug/sensorview`, which is
`asInvoker` and console-subsystem, so it passes while telling us nothing about
the artifact that ships. The `Build (release)` step only compiles; nothing ever
executes a release binary on any platform. That is precisely why all three bugs
above survived a green CI.

Worth adding: a step that runs the *release* binary's `--help` and asserts it
produces output and exits 0.

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
- **The TUI is only partly verified.** `sensorview top` has now been run on a
  real Windows console — rendering, `q`/`Esc`/`Ctrl-C`, and panic-hook terminal
  restoration under `panic = "abort"` all check out (see below). Two things
  still have not been observed: whether `r` actually resets min/max (no visible
  sensor had a spread to reset at the time), and behaviour under legacy
  `conhost` rather than the default terminal. It has also never run on a Linux
  or macOS terminal.
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
