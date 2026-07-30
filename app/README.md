# SensorView — application

This directory holds the Rust application, which is what actually ships.

**See the [root README](../README.md)** for features, platform support, install
instructions, the comparison against other monitors and the full technical
breakdown. This file only covers working inside `app/`.

## Quick start

```bash
cargo run --release                  # real sensors for your platform
SENSORVIEW_SOURCE=demo cargo run     # synthetic data — no drivers, good for UI work
cargo run --no-default-features      # GUI only, no listening socket
```

## Layout

```
src/
├── main.rs        # entry point; wires the poll, inventory, web and UI threads
├── model/         # Sensor / Hardware model, storage health, PCIe topology, hex blobs
├── source/        # SensorSource trait + backends
│   ├── lhm_bridge.rs   Windows — .NET LibreHardwareMonitor sidecar over stdout JSON
│   ├── macos/          macOS   — IOKit (iokit, hid, ioreport, freq, dvfs, gpu, …)
│   ├── linux.rs        Linux   — sysfs hwmon + procfs
│   ├── firmware.rs     ACPI/SMBIOS tables (Windows + Linux)
│   └── demo.rs         synthetic
├── poll.rs        # fast lane (~1 Hz) + min/max/avg
├── inventory.rs   # slow lane (~30 s): S.M.A.R.T., SPD, PCIe, firmware
├── sysinfo.rs     # one-shot static system info, per platform
├── ui/            # egui windows: main, sensors, summary, graphs, hex, settings
└── web/           # axum server: REST, WebSocket, Prometheus, embedded dashboard
sidecar/           # C# LibreHardwareMonitor bridge (Windows only)
web-dashboard/     # static assets, embedded into the binary
installer/         # Inno Setup script (Windows)
assets/            # window + executable icons
```

## Adding a sensor backend

Implement `SensorSource` (see `src/source/mod.rs`), add an arm to
`default_source()`, and **widen the fallback arm's `not(any(...))` to exclude
your platform**. Those are sequential exclusive `cfg` blocks, not `else if` — a
platform named in an arm *and* left in the fallback compiles both, and the last
one wins.

The data model mirrors OpenHardwareMonitor's `Hardware/ISensor.cs` and
`Hardware/IHardware.cs`, so the C# reference in the repository root and this port
share a vocabulary.

## Checks

These are exactly what CI runs:

```bash
cargo test
cargo clippy --all-targets -- -D warnings -D clippy::await_holding_lock
cargo check --no-default-features --all-targets
```

Hardware-dependent tests print `SKIP: … not available` and pass when the device
or API is absent, so they stay green on virtualized CI runners while still
asserting ranges, uniqueness and stability on real hardware. Two `#[ignore]`d
probes dump live readings:

```bash
cargo test -- --ignored --nocapture dump_live_tree
cargo test -- --ignored --nocapture dump_system_summary
```

## Packaging

```bash
cargo install cargo-packager --version 0.11.8 --locked
cargo packager --release --formats dmg     # or nsis / deb / appimage
```

On Windows, build the sidecar first:
`dotnet publish sidecar -c Release -o sidecar/publish`.

[LibreHardwareMonitor]: https://github.com/LibreHardwareMonitor/LibreHardwareMonitor
[cargo-packager]: https://github.com/crabnebula-dev/cargo-packager
