<div align="center">

<img src="app/assets/128x128.png" alt="SensorView" width="128" height="128">

# SensorView

**A native, cross-platform hardware monitor in pure Rust.**

Real sensors on Windows, macOS and Linux. Dense HWiNFO-style tables, live graphs,
CSV logging, a LAN dashboard and a Prometheus endpoint — in a single ~7 MB binary
with no Electron, no webview, and no telemetry.

[![CI](https://github.com/Zektopic/sensorview/actions/workflows/ci.yml/badge.svg)](https://github.com/Zektopic/sensorview/actions/workflows/ci.yml)
![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20macOS%20%7C%20Linux-blue)
![Rust](https://img.shields.io/badge/rust-1.92%2B-orange)

</div>

> SensorView is a ground-up Rust rewrite that lives alongside the C#
> OpenHardwareMonitor sources in this repository. The C# tree remains the
> authoritative reference for low-level sensor access; the Rust app in `app/` is
> what ships.
>
> HWiNFO is a proprietary product. SensorView reproduces a HWiNFO-*style* dense
> sensor UI; it is not affiliated with or endorsed by HWiNFO.

---

## Why SensorView

Most hardware monitors make you choose between three compromises. SensorView is
an attempt to avoid all three.

| | Typical Windows-only tool | Electron/webview monitors | Per-platform CLI tools | **SensorView** |
|---|---|---|---|---|
| Windows / macOS / Linux | Windows only | varies | one platform each | **all three, one codebase** |
| Real sensors, not estimates | yes | often estimated | yes | **yes, per-platform native** |
| Native UI | yes | no (Chromium) | terminal | **yes (egui, GPU-drawn)** |
| Remote / headless access | paid tier or none | sometimes | no | **built-in HTTP + WebSocket** |
| Prometheus / Grafana | rarely | rarely | no | **`/metrics` built in** |
| Kernel driver required | yes (Ring 0) | n/a | sometimes | **Windows only — never on macOS** |
| Cost / licence | freeware or paid | varies | free | **open source** |

### What actually separates it

**One data model, three real backends.** Everything the UI renders comes through a
single `SensorSource` trait, so the Windows, macOS and Linux backends are
interchangeable and the UI, web tier, logger and graphs never branch on platform.
Adding a backend is one trait impl and one line in a factory.

**No kernel driver on macOS.** Apple Silicon sensors are read entirely through
IOKit as an ordinary user — no helper daemon, no signed kext, no privilege
prompt. On Windows, full sensor access still requires a Ring-0 driver, as it does
for every tool on that platform.

**Remote monitoring is not a paid tier.** The embedded HTTP server, live
WebSocket feed and Prometheus exporter are part of the normal build. Bind it to
loopback for local scripting or to your LAN for a headless box.

**Honest blanks.** Where a value genuinely isn't available on a platform — CPUID
on ARM, ACPI tables on Apple Silicon, S.M.A.R.T. behind Apple's storage
controller — the field renders `—` rather than a plausible-looking guess.

---

## Features

### Sensor monitoring
- Dense, sortable **Sensors Status** table with current / min / max / average
  columns, per-type icons, draggable column widths and font zoom
- Per-sensor **history graphs** in their own windows
- **System Summary** — CPU, motherboard, memory, GPU, drives, OS, ISA feature grid
- **Hex Viewer** for raw firmware blobs (ACPI/SMBIOS on Windows and Linux)
- Configurable poll interval, min/max reset, light/dark/grey themes

### Data out
- **CSV logging** at 1 Hz to Documents, one column per sensor
- **Text report export** for pasting into bug reports
- **REST API** — `/api/telemetry`, `/api/system`, `/api/history/{id}`, `/api/health`
- **WebSocket** — `/ws/telemetry`, push-based live feed
- **Prometheus** — `/metrics`, scrape straight into Grafana
- **Web dashboard** — responsive, embedded in the binary, no external assets

### Security posture
The web tier binds to loopback by default. Bound off-loopback it becomes
token-gated automatically: telemetry exposes hardware serials, SPD contents and
PCI configuration space, so it is not something to leave open on a LAN. The token
is generated per run and never written to disk.

---

## Platform support

| | Windows | macOS (Apple Silicon) | Linux |
|---|---|---|---|
| Backend | LibreHardwareMonitor sidecar | native IOKit | native sysfs / procfs |
| Temperatures | ✅ | ✅ | ✅ hwmon |
| Power | ✅ | ✅ | ✅ hwmon |
| Voltages | ✅ | ✅ (core VID) | ✅ hwmon |
| Clocks | ✅ | ✅ | ✅ cpufreq |
| Fan speeds | ✅ | —¹ | ✅ hwmon |
| Load / memory | ✅ | ✅ | ✅ procfs |
| GPU | ✅ | ✅ | ✅ via amdgpu/i915/nouveau hwmon |
| Storage | ✅ throughput | ✅ throughput | temperature only |
| Battery | ✅ | ✅ | — |
| Firmware tables | ACPI + SMBIOS | —² | ACPI |
| Needs elevation | **yes** (Ring-0 driver) | **no** | no (ACPI tables need root) |

Linux coverage is whatever your loaded `hwmon` drivers expose — `coretemp`,
`k10temp`, `amdgpu`, `nct6775`, `drivetemp` and so on. With none loaded you get
CPU load and memory from procfs and little else.

¹ Not implemented. Fans would come from the same HID sensor plane as
temperatures (a different usage page), but development happened on a fanless
MacBook Air where there was nothing to read or verify against.
² Apple Silicon has no ACPI or SMBIOS; the firmware uses an ARM device tree.

Intel Macs are **not** supported. They would need a completely different
`AppleSMC` backend, which does not exist on M-series hardware and could not be
tested during development.

---

## Install

Download the installer for your platform from [Releases](https://github.com/Zektopic/sensorview/releases):

| Platform | Artifact | Notes |
|---|---|---|
| Windows | `SensorView-setup.exe` (NSIS) | Bundles the sensor sidecar; optionally installs the PawnIO driver |
| macOS | `SensorView_<ver>_aarch64.dmg` | Apple Silicon only; **unsigned** — see below |
| Linux | `.deb` / `.AppImage` | |

**macOS first launch:** the `.dmg` is currently unsigned and un-notarized, so
Gatekeeper will block it. Open **System Settings → Privacy & Security**, find the
blocked-app notice, and choose **Open Anyway**.

**Windows:** full sensor coverage needs a Ring-0 driver. If Memory Integrity/HVCI
blocks the classic WinRing0 driver, install [PawnIO](https://pawnio.eu/) — the
installer offers this, and Settings → Driver Management explains the state.

---

## Build from source

Requires [Rust](https://rustup.rs) 1.92+.

```bash
git clone https://github.com/Zektopic/sensorview.git
cd sensorview/app
cargo run --release
```

Platform prerequisites:

```bash
# Windows — also build the sensor sidecar (needs .NET 8 SDK)
dotnet publish sidecar -c Release -o sidecar/publish

# Linux
sudo apt-get install -y libgtk-3-dev libxcb-render0-dev libxcb-shape0-dev \
    libxcb-xfixes0-dev libxkbcommon-dev libwayland-dev libssl-dev

# macOS — Xcode Command Line Tools are sufficient
xcode-select --install
```

Useful flags:

```bash
SENSORVIEW_SOURCE=demo cargo run     # synthetic data, no drivers — good for UI work
cargo run --no-default-features      # GUI only, drops the web tier (no listening socket)
cargo packager --release --formats dmg   # or nsis / deb / appimage
```

---

## Technical breakdown

### Threading model

Four threads, deliberately decoupled so the UI can never be blocked by hardware.

```
┌──────────────┐   ArcSwap    ┌──────────────┐
│ poll thread  │─────────────▶│  UI (main)   │  eframe/egui, winit needs main
│  fast lane   │              └──────────────┘
│  ~1 Hz       │  broadcast   ┌──────────────┐
└──────┬───────┘─────────────▶│  web thread  │  tokio + axum
       │                      └──────────────┘
┌──────▼───────┐
│ slow lane    │  ~30 s — S.M.A.R.T., SPD, PCIe topology, firmware tables
└──────────────┘
```

The latest telemetry frame is published through an `ArcSwap`, so a UI read is a
single atomic pointer load — the render loop never contends with the poller and
never holds a lock across a frame. Settings changes are *sent* to the poll thread
as commands rather than applied under a shared lock.

**Why two polling lanes.** S.M.A.R.T. and NVMe log pages keep drives out of
low-power states and burn a limited read budget; SPD and EC reads go over
SMBus/I²C, where polling faster than ~2 Hz collides with firmware and shows up as
audio dropouts; PCIe topology only changes on hotplug. Those are separated into a
30-second lane behind a distinct `InventorySource` trait so the two cadences can
never be accidentally coupled.

### Extension surface

```rust
pub trait SensorSource: Send {
    fn name(&self) -> &'static str;
    fn snapshot(&mut self) -> Vec<Hardware>;
    fn diagnostics(&self) -> Diagnostics { Diagnostics::default() }
}

pub trait InventorySource: Send {
    fn name(&self) -> &'static str;
    fn collect(&mut self) -> Inventory;   // may block for seconds
}
```

That is the whole platform abstraction — no registry, no plugin system. The data
model (`SensorType`, `HardwareType`, `Sensor`, `Hardware`) mirrors
OpenHardwareMonitor's `ISensor.cs` / `IHardware.cs` so the C# reference and the
Rust port speak the same vocabulary.

### How each platform reads hardware

**Windows** spawns a .NET 8 LibreHardwareMonitor sidecar and reads
newline-delimited JSON from its stdout — one `{"meta": …}` line, then one tree
per tick. Keeping LHM out-of-process means a driver fault takes down the sidecar,
not the UI. Static system info comes from WMI in-process; ACPI/SMBIOS come from
`EnumSystemFirmwareTables`, which needs no driver at all.

**macOS (Apple Silicon)** reads everything in-process through IOKit:

| Data | Mechanism |
|---|---|
| Die temperatures | `IOHIDEventSystemClient` sensor plane (private) |
| CPU/GPU/ANE/DRAM power | `libIOReport` "Energy Model" (private, `dlopen`ed) |
| CPU/GPU clocks + VID | IOReport DVFS residency × `pmgr` `voltage-states` |
| CPU load, memory | Mach `host_processor_info` / `host_statistics64` |
| GPU load, memory | `IOAccelerator` `PerformanceStatistics` |
| SSD throughput | `IOBlockStorageDriver` statistics |
| Battery | `AppleSmartBattery` |

Apple Silicon exposes no "current MHz" register, so clocks are *reconstructed*:
IOReport gives residency per DVFS state, the `pmgr` device-tree node gives the
frequency/voltage table, and weighting one by the other yields the effective
clock and voltage — the same quantity `powermetrics` reports, without needing
root.

Two of those are private frameworks. Every symbol is resolved at runtime and
every collector degrades to producing no sensors rather than panicking, because
the release profile is `panic = "abort"` and a missing symbol must not take down
the app. This is fine for `.dmg` distribution and notarization, but rules out the
Mac App Store. `AppleSMC` is deliberately unused — it does not exist on M-series
Macs.

**Linux** reads `/sys/class/hwmon`, `/proc/stat` and `/proc/meminfo` directly,
plus `/sys/class/dmi/id` for board identity and `/sys/firmware/acpi/tables` for
the Hex Viewer.

### Design constraints worth knowing

- **Rate-derived sensors never disappear.** Power, clocks and throughput are all
  deltas, so they produce nothing on the first poll. A sensor that has no reading
  publishes `value: None` (rendered `—`) rather than being dropped from the tree
  — otherwise rows flicker in and out and history graphs break. There is a
  regression test that polls repeatedly and asserts the published identifier set
  never changes.
- **Sensor identifiers are stable strings**, not indices. Graph history, CSV
  columns and the web API all key off them.
- **`panic = "abort"` in release.** Anything that can be absent is an `Option`,
  never an `unwrap`.

### Repository layout

```
sensorview/
├── app/                       # the Rust application — this is what ships
│   ├── src/
│   │   ├── main.rs            # entry point, thread wiring
│   │   ├── model/             # Sensor/Hardware model, storage, topology, hex blobs
│   │   ├── source/            # SensorSource trait + backends
│   │   │   ├── lhm_bridge.rs  #   Windows: .NET sidecar
│   │   │   ├── macos/         #   macOS: IOKit (iokit, hid, ioreport, freq, dvfs, …)
│   │   │   ├── linux.rs       #   Linux: sysfs/procfs
│   │   │   └── demo.rs        #   synthetic
│   │   ├── poll.rs            # fast lane + min/max/avg
│   │   ├── inventory.rs       # slow lane
│   │   ├── ui/                # egui windows
│   │   └── web/               # axum server, REST, WebSocket, Prometheus
│   ├── sidecar/               # C# LibreHardwareMonitor bridge (Windows)
│   ├── web-dashboard/         # embedded static dashboard
│   └── installer/             # Inno Setup script
├── Hardware/ GUI/ WMI/ …      # original C# OpenHardwareMonitor reference
└── .github/workflows/         # CI + release (3-platform matrix)
```

---

## Development

```bash
cd app
cargo test                                              # unit + hardware-probe tests
cargo clippy --all-targets -- -D warnings               # CI gate
cargo check --no-default-features --all-targets         # GUI-only build must keep working
```

Hardware-dependent tests skip with a `SKIP:` note when the underlying device or
API isn't present, so they stay green on virtualized CI runners while still
asserting ranges, uniqueness and stability on real hardware. Two `#[ignore]`d
diagnostic probes dump live readings:

```bash
cargo test -- --ignored --nocapture dump_live_tree
cargo test -- --ignored --nocapture dump_system_summary
```

CI builds and packages on `windows-latest`, `ubuntu-22.04` and `macos-latest`;
tagged `v*` pushes publish installers to Releases.

---

## Status and known gaps

- macOS fan sensors are **not implemented** — see the platform table footnote.
- macOS `.dmg` is unsigned and un-notarized.
- Intel Macs are unsupported.
- NVMe S.M.A.R.T. health on macOS reports identity only — Apple's controller
  isn't a standard NVMe endpoint, so the health log page is unreachable; those
  fields report `Unknown` rather than a fabricated "Good".
- Linux storage reports temperature only (via `drivetemp`), not throughput.
- Linux coverage depends on which `hwmon` drivers are loaded.

## Contributing

Adding a platform backend means implementing `SensorSource`, adding one arm to
`source::default_source()`, and — importantly — widening the fallback arm's
`not(any(...))` so two definitions don't end up live at once.

## Licence

The Rust application and the OpenHardwareMonitor reference sources are covered by
the licences in [`Licenses/`](Licenses/).
