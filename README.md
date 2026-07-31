<div align="center">

<img src="app/assets/128x128.png" alt="SensorView" width="128" height="128">

# SensorView

**A native, cross-platform hardware monitor in pure Rust.**

Real sensors on Windows, macOS and Linux. Dense HWiNFO-style tables, live graphs,
CSV logging, a LAN dashboard and a Prometheus endpoint — in a single ~7 MB binary
with no Electron, no webview, and no telemetry.

[![CI](https://github.com/Zektopic/sensorview/actions/workflows/ci.yml/badge.svg)](https://github.com/Zektopic/sensorview/actions/workflows/ci.yml)
![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20macOS%20%7C%20Linux-blue)
![Rust](https://img.shields.io/badge/rust-1.95%2B-orange)

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

Laid out after the Windows Task Manager, because that is the layout people
already know.

- **Processes** — name, CPU, memory, disk rate, PID and owner. The measured
  columns are **heat-shaded**: the tint strengthens and shifts from amber to red
  with the value, so the processes worth looking at are found by glancing rather
  than reading. Each column header carries its machine-wide total. Sortable
  columns, a filter box, and end/force-kill behind a confirmation. Rows are
  virtualised, so a 700-process list scrolls without cost
- **Performance** — a sidebar of device cards (CPU, GPU, Memory, Disk *n*,
  Network *n*), each with a live filled thumbnail and its current value, and the
  selected device filling the pane with a large filled area graph over a fixed
  grid. Every device class has its own colour — CPU blue, memory purple, disk
  green, network rose, GPU teal — used for its card, its thumbnail and its
  graph, so colour identifies the device rather than just decorating. Utilisation
  charts are pinned to 0–100 % so an idle machine reads as idle. It reads the
  telemetry that is already being collected, so it adds no polling

CPU percentages in the **Processes** table are a share of the whole machine, as
Windows reports them: a process saturating 4 of 16 threads reads 25 %. Hovering
gives the per-thread-summed figure (400 %), which is what `sensorview ps` prints.
- **The same data from the terminal** — `sensorview ps` and `sensorview kill`,
  which is what you want over SSH on a box with no display
- The process collector runs **only while the window is open** — enumerating
  every process is not free, and a monitor that costs CPU when nobody is
  looking at it is a bug

### Command line
- **One-shot queries** — `sensors`, `get`, `info`, `report`; JSON or text; real
  exit codes, so `sensorview get … || alert` works
- **Daemon mode** — `sensorview daemon`, with clean Ctrl-C shutdown
- **Live terminal UI** — `sensorview top`, for watching a headless box over SSH
- **Streaming** — `sensorview stream`, NDJSON or CSV on stdout, pipe-friendly
- **Push sinks** — InfluxDB line protocol or a JSON webhook, on their own
  thread with backoff, so a collector that is down never stalls the poller
- **GUI-free builds** from 704 KB, for servers and containers

### Data out
- **CSV logging** at 1 Hz to Documents, one column per sensor
- **Text report export** for pasting into bug reports
- **REST API** — `/api/telemetry`, `/api/system`, `/api/history/{id}`, `/api/health`
- **WebSocket** — `/ws/telemetry`, push-based live feed
- **Prometheus** — `/metrics`, scrape straight into Grafana
- **Web dashboard** — responsive, embedded in the binary, no external assets

### Security posture
Dependencies are audited on every dependency change and weekly on a schedule
(`.github/workflows/security.yml`), because a tree that is clean today can be
vulnerable next week with no code change. `cargo deny` additionally gates
licences and dependency sources.

The dashboard sets a strict Content-Security-Policy (it is fully
self-contained, so nothing external is permitted), `Referrer-Policy:
no-referrer` — which matters because the access token may be passed as
`?token=` — plus `nosniff`, `X-Frame-Options: DENY` and `Cache-Control:
no-store` on telemetry.

Push targets are redacted before being logged: `http://user:pass@host` and
InfluxDB's `?u=&p=` credentials never reach a terminal or journal. Pushing
cleartext off-machine prints a warning, since telemetry carries hardware
serials.

The web tier binds to loopback by default. Bound off-loopback it becomes
token-gated automatically: telemetry exposes hardware serials, SPD contents and
PCI configuration space, so it is not something to leave open on a LAN. The token
is generated per run and never written to disk.

---

## Command line

`sensorview` works from a terminal, not just as a window. Bare `sensorview`
still opens the GUI.

```bash
sensorview sensors                     # the whole tree
sensorview sensors --filter temp       # only matching sensors
sensorview sensors --json | jq '.[]'   # flat JSON array, one object per sensor

sensorview get "CPU Package Power"     # -> CPU Package Power: 21.5 W
sensorview get "PMU tdie6" --raw       # -> 38.77   (bare number, for scripts)
sensorview get "no such sensor"        # exit code 1

sensorview info                        # System Summary as text
sensorview report                      # the GUI's text report, from the CLI

# Processes — the Task Manager's data, without the window
sensorview ps                          # busiest first
sensorview ps --sort mem --limit 10    # sort by cpu | mem | disk | pid | name
sensorview ps --filter chrome --json   # pipe into jq
sensorview kill 4711                   # SIGTERM; --force sends SIGKILL

sensorview top                         # live terminal dashboard (q quits)
sensorview daemon --port 9090 --log    # headless; Ctrl-C stops it cleanly

# Streaming — one record per poll, until Ctrl-C or -n
sensorview stream                              # NDJSON, the full frame per line
sensorview stream --filter temp -n 10          # compact records, 10 then stop
sensorview stream --format csv --filter power  # timestamped CSV
sensorview stream | head -5                    # closed pipe exits 0

# Push to a collector instead of pulling — daemon posts on an interval
sensorview daemon --push influx://influx:8086/write?db=sensors --push-interval 5
sensorview daemon --push http://collector/ingest        # JSON webhook
```

`stream --format csv` emits a real `unix_ms` timestamp column, not the row
counter the GUI's CSV logger uses, and keeps its columns fixed from the first
record so the output stays rectangular. A sensor with no reading in a given
tick leaves its field empty rather than writing a fabricated `0`.

One-shot commands wait for *usable* data before printing. Power, clock and
throughput readings are computed from the delta between two polls, so they do
not exist in the first frame — a naive implementation would return blank on a
cold start. `get` waits for the named sensor to actually carry a value.

### Headless builds

Dropping the `gui` feature compiles the windowing stack out entirely:

| Build | Size | Contents |
|---|---|---|
| `cargo build --release` | 7.1 MB | GUI + dashboard + CLI |
| `--no-default-features --features web` | **1.3 MB** | CLI + dashboard — for servers and containers |
| `--no-default-features` | **704 KB** | CLI only |

Features are `gui`, `web`, `push` and `tui`; all are on by default and CI builds
every combination that ships.

## Platform support

| | Windows | macOS (Apple Silicon) | Linux |
|---|---|---|---|
| Backend | LibreHardwareMonitor sidecar | native IOKit | native sysfs / procfs |
| Temperatures | ✅ | ✅ | ✅ hwmon |
| Per-core CPU load | ✅ | ✅ | ✅ procfs |
| Power | ✅ | ✅ | ✅ hwmon |
| Voltages | ✅ | ✅ (core VID) | ✅ hwmon |
| Clocks | ✅ | ✅ | ✅ cpufreq |
| Fan speeds | ✅ | —¹ | ✅ hwmon |
| Load / memory | ✅ | ✅ | ✅ procfs |
| GPU | ✅ | ✅ | ✅ utilisation on amdgpu; temp/fan on i915/nouveau |
| Storage | ✅ throughput | ✅ throughput | ✅ throughput + temp |
| Network | ✅ | — | ✅ procfs |
| Battery | ✅ | ✅ | — |
| Firmware tables | ACPI + SMBIOS | —² | ACPI |
| Needs elevation | **yes** (Ring-0 driver) | **no** | no (ACPI tables need root) |

Linux reads per-core CPU from `/proc/stat`, disk throughput from
`/proc/diskstats`, network from `/proc/net/dev` and GPU utilisation from
amdgpu's `gpu_busy_percent` — none of which need a driver beyond what the
kernel already provides. Temperatures, fans and voltages come from whatever
`hwmon` drivers are loaded (`coretemp`, `k10temp`, `amdgpu`, `nct6775`,
`drivetemp`).

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
| Windows | `SensorView_<ver>_x64-setup.exe` (NSIS) | Bundles the sensor sidecar; optionally installs the PawnIO driver |
| Windows | `SensorView-<ver>-portable.exe` | One file, no install — see below |
| macOS | `SensorView_<ver>_aarch64.dmg` | Apple Silicon only; **unsigned** — see below |
| Linux | `.deb` / `.AppImage` | |

**The portable Windows build** is a single self-contained `.exe`. Every Windows
sensor comes from the LibreHardwareMonitor sidecar, so the portable build
carries that sidecar inside the binary and unpacks it to
`%LOCALAPPDATA%\SensorView\bridge\` the first time it runs — which is why it is
several times the size of the installer, and why it works from a USB stick with
nothing beside it. Delete that folder to reclaim the space once you are done.

**macOS first launch:** the `.dmg` is currently unsigned and un-notarized, so
Gatekeeper will block it. Open **System Settings → Privacy & Security**, find the
blocked-app notice, and choose **Open Anyway**.

**Windows:** full sensor coverage needs a Ring-0 driver. If Memory Integrity/HVCI
blocks the classic WinRing0 driver, install [PawnIO](https://pawnio.eu/) — the
installer offers this, and Settings → Driver Management explains the state.

---

## Build from source

Requires [Rust](https://rustup.rs) 1.95+. The floor is not cosmetic: cargo
honours `rust-version` during resolution, so an older toolchain silently picks
*older dependency versions* rather than failing — which is how the process
collector once ended up building against `sysinfo` 0.38 instead of 0.39.

```bash
git clone https://github.com/Zektopic/sensorview.git
cd sensorview/app
cargo run --release
```

Platform prerequisites:

```bash
# Windows — MSVC Build Tools are required, not optional: the default feature set
# includes `push`, which pulls in rustls → ring, and ring compiles C and assembly.
# Build from a developer prompt (vcvars64.bat) so cl.exe and the Windows SDK are
# on PATH. Without a C toolchain, use --no-default-features --features gui,web,tui.
#
# Also build the sensor sidecar (needs .NET 8 SDK):
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
cargo run --no-default-features --features gui   # GUI without the web tier (no listening socket)
cargo run --no-default-features -- sensors       # headless: CLI only, no windowing stack
cargo packager --release --formats dmg   # or nsis / deb / appimage
```

---

## Technical breakdown

### Threading model

Deliberately decoupled, so the UI can never be blocked by hardware.

```
┌──────────────┐   ArcSwap    ┌──────────────┐
│ poll thread  │─────────────▶│  UI (main)   │  eframe/egui, winit needs main
│  fast lane   │              └──────▲───────┘
│  ~1 Hz       │  broadcast   ┌──────┴───────┐
└──────┬───────┘─────────────▶│  web thread  │  tokio + axum
       │                      └──────────────┘
┌──────▼───────┐              ┌──────────────┐
│ slow lane    │              │ process      │  only while the Task Manager
│  ~30 s       │              │ collector    │  window is open
└──────────────┘              └──────────────┘
```

The slow lane runs S.M.A.R.T., SPD, PCIe topology and firmware tables. Push
sinks, when configured, get a thread of their own as well, so a collector that
is down applies backoff without ever stalling the poller.

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

**Linux** reads sysfs and procfs directly, with no daemon and nothing beyond
what a stock kernel already exposes:

| Data | Path |
|---|---|
| Temperatures, fans, voltages | `/sys/class/hwmon` |
| Total and per-core CPU load | `/proc/stat` — the `cpu` and `cpuN` lines |
| Memory | `/proc/meminfo` |
| Clocks | `cpufreq` |
| Disk throughput | `/proc/diskstats`, differenced per poll |
| Network throughput | `/proc/net/dev` |
| GPU utilisation | `/sys/class/drm/*/device/gpu_busy_percent` (amdgpu) |
| Board identity | `/sys/class/dmi/id` |
| Firmware tables | `/sys/firmware/acpi/tables` |

Only the hwmon row depends on which drivers happen to be loaded; everything
else is always there.

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
│   │   ├── procs/             # process enumeration (Task Manager + `ps`)
│   │   ├── cli/               # clap commands: sensors, get, ps, stream, top, daemon
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
```

CI runs that clippy gate over **eight** feature configurations — `default`,
`--no-default-features`, and each of `gui`, `web`, `push`, `tui`, `gui,web`,
`web,push,tui`. That is not busywork: a module reachable from the CLI but not
the GUI (or the reverse) produces dead-code warnings in exactly one
configuration, and the headless build is a shipped artifact, so it has to hold
the same bar as the GUI one.

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
- GPU **utilisation** on Linux is amdgpu-only; Intel and nouveau expose no
  equivalent and NVIDIA needs NVML.
- Temperature/fan/voltage coverage on Linux still depends on which `hwmon`
  drivers are loaded.
- The Task Manager lists **processes, not applications** — no grouping of a
  browser's helpers under one row, and no Services page. Per-process **GPU**
  usage is absent too: there is no public API for it on macOS.
- The Linux **GUI** is compiled and tested in CI but has not been run against a
  display; Linux development happens on a headless server, so the window itself
  is exercised on macOS only.

## Contributing

Adding a platform backend means implementing `SensorSource`, adding one arm to
`source::default_source()`, and — importantly — widening the fallback arm's
`not(any(...))` so two definitions don't end up live at once.

## Licence

MPL-2.0 — see [`Licenses/`](Licenses/). The Rust application and the
OpenHardwareMonitor reference sources are covered by the same terms.
