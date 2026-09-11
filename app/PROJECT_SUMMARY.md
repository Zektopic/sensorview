# SensorView — Project Primer

> Context document for an LLM assistant working on this repository.
> Written 2026-07-23. Describes what **exists today**, not the roadmap.

---

## 0. The one fact to get right first: this repo holds TWO codebases

| Path | What it is | Status |
| --- | --- | --- |
| **repo root** (`Hardware/`, `GUI/`, `Collections/`, `WMI/`, `*.csproj`, `*.sln`) | The original **C# / .NET WinForms OpenHardwareMonitor** | **Reference only — do not modify.** Kept as the authoritative source for low-level sensor logic. |
| **`app/`** | **SensorView** — the active **Rust** rewrite | **This is where all work happens.** |

If a task says "fix a bug", "add a feature", "run the app" — it means `app/`, unless the
user explicitly names the C# tree.

---

## 1. What SensorView is

A **native** (not web, not Electron, no webview) cross-platform hardware monitor written in
pure Rust, styled and structured after **HWiNFO64 8.50**. Goal: HWiNFO-like UI density,
window layout, and feature set. Windows-first; Linux/macOS builds are produced by CI.

HWiNFO is proprietary; SensorView is an independent HWiNFO-*style* app, not affiliated with it.

---

## 2. Tech stack

| Concern | Choice |
| --- | --- |
| GUI | **eframe / egui 0.35** (GPU-drawn immediate-mode, real OS windows) |
| Web tier | **axum 0.8 + tokio** on a dedicated thread — bundled SPA + `/ws/telemetry` |
| Shared state | **arc-swap** — lock-free publish/subscribe of the latest frame |
| Multi-window | egui **deferred viewports** (`ctx.show_viewport_deferred`) — each extra window is a real OS window |
| Sensor engine (Windows) | **LibreHardwareMonitor 0.9.6** running as a **.NET 8 sidecar process** |
| Static system info (Windows) | `wmi` crate 0.18 + raw `CPUID` + Win32 token FFI |
| Serialization | `serde` / `serde_json` |
| Paths | `dirs` 6 |
| Packaging | **cargo-packager 0.11.8** → NSIS `.exe`, `.deb`, `.AppImage`, `.dmg` |
| Rust edition / MSRV | 2021 / **1.92** (dev machine has 1.97.1) |

`app/Cargo.toml` release profile: `panic = "abort"`, `lto = true`, `opt-level = "s"`, `strip = true`.
`build.rs` uses `winresource` to embed a `requireAdministrator` manifest **in release builds only**.

---

## 3. Layout of `app/`

```
app/
├── Cargo.toml            # deps + [package.metadata.packager] (NSIS/deb/dmg config)
├── build.rs              # winresource: icon + requireAdministrator manifest (release only)
├── Directory.Build.props # stops repo-root LangVersion 7.3 leaking into the sidecar
├── assets/               # icon.png, icon.ico, 32x32.png (32x32 is include_bytes!'d)
├── web-dashboard/        # SPA source (index.html/app.js/style.css), embedded
├── sidecar/
│   ├── Program.cs        # .NET 8 LibreHardwareMonitor bridge (~193 lines)
│   ├── SensorViewBridge.csproj
│   └── publish/          # dotnet publish output → sensorview-bridge.exe
└── src/
    ├── main.rs           # thread supervisor: spawns/joins Threads 1, 1b, 2, 3
    ├── state.rs          # TelemetryStore / TelemetryFrame (ArcSwap + broadcast)
    ├── model/            # mod.rs (sensors) + storage.rs (S.M.A.R.T./NVMe),
    │                     #   topology.rs (PCIe), hexblob.rs (raw dumps)
    ├── poll.rs           # Thread 1: poll loop, Command channel, Monitor stats
    ├── inventory.rs      # Thread 1b: Inventory, InventorySource, slow collector
    ├── web/              # Thread 3: mod.rs, ws.rs, api.rs, auth.rs, assets.rs
    ├── source/
    │   ├── mod.rs        # SensorSource trait, Diagnostics, default_source()
    │   ├── lhm_bridge.rs # spawn/parse the .NET sidecar (Windows)
    │   └── demo.rs       # synthetic data — driverless fallback / CI / non-Windows
    ├── sysinfo.rs        # per-platform static info, CPUID decode, SMBIOS memory
    │                    # types, is_elevated()  (~1300 lines)
    ├── settings.rs       # AppSettings (JSON in %APPDATA%\SensorView\settings.json)
    ├── logging.rs        # CsvLogger
    ├── report.rs         # write_report() → SensorView_Report_<stamp>.txt
    └── ui/
        ├── mod.rs        # Shared, Palette, theme, fonts, viewport registration
        ├── main_window.rs      # toolbar + device tree + "Feature" detail pane
        ├── sensors_window.rs   # HWiNFO "Sensors Status" (the main event, ~435 lines)
        ├── summary_window.rs   # HWiNFO "System Summary"
        ├── settings_dialog.rs  # 5-tab settings dialog
        ├── graph_window.rs     # per-sensor line chart (hand-painted)
        └── widgets.rs          # painted icons, vendor badges, rows, formatting
```

---

## 4. Data flow (the core mental model)

**Three threads plus a slow lane. One writer, lock-free readers.**

```
Thread 1  poll::spawn ......... fast lane ~1 s
   SensorSource::snapshot() -> Monitor (min/max/avg) -> TelemetryFrame
   the ONLY writer to TelemetryStore
        |                                    |
   ArcSwap (atomic ptr)            broadcast::Sender<Arc<String>>
        v                                    v
Thread 2  GUI (main thread)        Thread 3  web::spawn
   eframe/egui, store.load()          tokio + axum, one task per WS client

Thread 1b  inventory::spawn_collector ..... slow lane ~30 s
   S.M.A.R.T. / SPD / PCIe topology -> ArcSwap, read by Thread 1
```

Key invariants (see the module docs in `state.rs` and `main.rs`):

1. **Single writer** — only Thread 1 mutates telemetry. `TelemetryStore::load()`
   is an atomic pointer read: never blocks, never poisons.
2. **UI mutations are messages, not locks** — "Reset Min/Max" and interval
   changes go over an `mpsc` channel as `poll::Command`s.
3. **Serialized once per tick** — `publish()` produces one `Arc<String>` that
   every WebSocket client forwards, so N clients cost O(1), not O(N).
4. **No guard across `.await`** — enforced by `#![deny(clippy::await_holding_lock)]`
   in `web/` and a CI clippy gate.
5. **Backpressure stops at the channel** — a stalled browser gets
   `RecvError::Lagged` and resyncs; it can never slow the poller or the GUI.
6. **Two cadences on purpose** — S.M.A.R.T. polling keeps drives awake and
   sub-500 ms SMBus polling provokes SMI storms, so those live on the slow lane,
   on their own thread, where a multi-second read can't delay a sensor tick.
7. **Ordered shutdown** — GUI exit -> stop Thread 3 (releases the port) -> stop
   Thread 1 (releases the sensor driver) -> stop Thread 1b. `PollHandle::stop()`
   *sends* `Command::Shutdown` rather than only clearing a flag, because the loop
   parks in `recv_timeout` and would otherwise take up to a full interval to notice.

### Web tier (`src/web/`, feature `web`, on by default)

| Route | Purpose |
| --- | --- |
| `GET /` + assets | Bundled SPA (`web-dashboard/`, embedded via rust-embed) |
| `GET /api/telemetry` | Latest frame, served from the pre-serialized string |
| `GET /api/system` | Static WMI/IOKit inventory |
| `GET /api/history/{id}` | Retained samples for one sensor |
| `GET /api/health` | Liveness — **unauthenticated by design**, exposes no telemetry |
| `GET /metrics` | Prometheus exposition (one family per `SensorType`) |
| `GET /ws/telemetry` | Live broadcast, one frame per tick |

**Security:** binds `127.0.0.1:8080` by default. Any non-loopback bind (settings
toggle or `SENSORVIEW_WEB_BIND`) makes a per-run access token **mandatory** on
every route except `/api/health`. The token is shown in Settings -> Remote Access,
printed to stderr at startup, and accepted as `Authorization: Bearer` or `?token=`.
Env overrides: `SENSORVIEW_WEB_PORT`, `SENSORVIEW_WEB_BIND`.

### Key types (`model/mod.rs`) — mirror OHM's `ISensor.cs` / `IHardware.cs`, extended to LHM's full surface

```rust
enum SensorType { Voltage, Current, Clock, Temperature, Load, Frequency, Fan, Flow,
                  Control, Level, Factor, Power, Data, SmallData, Throughput,
                  TimeSpan, Energy, Noise, Conductivity, Humidity }   // .unit() → "V","MHz","°C",…

enum HardwareType { Mainboard, SuperIO, Cpu, Ram, GpuNvidia, GpuAti, GpuIntel,
                    TBalancer, Heatmaster, Hdd, Storage, Network, Cooler,
                    EmbeddedController, Psu, Battery }

struct Sensor  { identifier, name, sensor_type, index, value/min/max/avg: Option<f32> }
struct Hardware{ identifier, name, hardware_type, sensors: Vec<Sensor>,
                 sub_hardware: Vec<Hardware> }   // recursive
```

`identifier` is an OHM-style path (`/amdcpu/0/temperature/0`) and is **the stable key** used for
stats, history, graph windows, and CSV columns.

### The sidecar protocol (`sidecar/Program.cs` ↔ `source/lhm_bridge.rs`)

- Sidecar = **.NET 8, self-contained, single-file** → `sensorview-bridge.exe`.
- It writes **one JSON document per line to stdout**:
  - **Line 1** = meta: `{"meta":{"lhm_version","is_elevated","ring0_report"}}`
    (`ring0_report` = the driver section of LHM's `computer.GetReport()`; surfaced in
    Settings → Driver Management).
  - **Every subsequent line** = a full `Vec<Hardware>` snapshot in exactly `model.rs`'s shape.
- Rust side spawns it with `CREATE_NO_WINDOW`, sets `SENSORVIEW_PARENT_PID` so the sidecar
  self-exits if the app dies (no orphans), runs a reader thread, and waits ≤15 s for the first
  snapshot before declaring failure.
- `kill_stale_sidecars()` (`taskkill /F /IM sensorview-bridge.exe`) runs before spawn — orphans
  hold the kernel driver / AMD SMU open.
- `find_sidecar()` search order: **next to the exe** → `<exedir>/sidecar/` → `CARGO_MANIFEST_DIR/sidecar/publish/`.
  (The "next to the exe" case is what makes a portable zip work.)
- `SensorSource::diagnostics()` returns `Diagnostics { engine_version, driver_report }`.
- **Backend selection** (`source::default_source()`): `SENSORVIEW_SOURCE=demo` forces demo;
  otherwise try `LhmBridge`; on failure fall back to demo.

---

## 5. UI surface (all four HWiNFO windows exist)

| Window | File | Contents |
| --- | --- | --- |
| **Main** | `main_window.rs` | Toolbar (Summary / Save Report / Sensors / Memory / About / Settings), left device tree with painted icons (`Computer, Central Processor(s), Motherboard, Memory, Video Adapter, Drives, Network`), right "Feature" detail pane fed by `sysinfo`, machine-name status bar |
| **Sensors Status** | `sensors_window.rs` | HWiNFO flow-column dense sensor table: grouped bands, Current/Min/Max/Average columns, type-colored values, collapsible groups, right-click → Show Graph, CSV logging toggle, uptime clock, **diagnostic banners** |
| **System Summary** | `summary_window.rs` | CPU / motherboard / memory-module / GPU / drive panels from WMI + CPUID. The CPU panel prints the raw CPUID signature *and* the decoded microarchitecture codename (`sysinfo::codename_for`); memory modules print the decoded SMBIOS type-17 code (`sysinfo::smbios_memory_type`), so LPDDR parts name themselves instead of collapsing to "DRAM" |
| **Settings** | `settings_dialog.rs` | 5 tabs: *General / User Interface* (live), *Safety* (stub), *SMBus / I2C* (stub), *Driver Management* (live — shows real ring0 report + elevation), *License Management* (stub) |
| **Graph** (n instances) | `graph_window.rs` | One deferred viewport per sensor identifier in `Shared.graphs`; hand-painted autoscaled polyline (no plotting dependency) |

**Two diagnostic banners** in the Sensors window:
1. `Banner::NotElevated` — when `Shared.elevated == Some(false)`.
2. `Banner::DriverBlocked` — heuristic `driver_appears_blocked()`: CPU has Clock|Power sensors
   *and* they all read ≈0 after warm-up ⇒ WinRing0 is blocked. Shows a `pawnio.eu` hyperlink.

**Theme**: `Palette::of(ColorMode)` with HWiNFO's three modes — `Grey` / `Black` (default) / `Light`.
Fonts: loads `C:\Windows\Fonts\segoeui.ttf` when present, silently falls back otherwise (CI/Linux).

**Icons are hand-painted vector glyphs + text vendor badges** (AMD/NVIDIA/INTEL/CORSAIR/SAMSUNG/MSI)
in `widgets.rs` — deliberately no copied logos, for trademark safety.

---

## 6. Settings & files on disk

- Settings JSON: `%APPDATA%\SensorView\settings.json` (via `dirs::config_dir()`), tolerant of a UTF-8 BOM.
  Saved on exit only when `remember_preferences` is true.
- CSV logs: `Documents\` (falls back to Desktop, then temp) — written with a **UTF-8 BOM** so Excel
  reads `°C` correctly. Header columns are `identifier (name [unit])`, one row per poll tick.
  `CsvLogger::start_in(dir, tree)` exists so tests can use a temp dir.
- Text reports: `SensorView_Report_<stamp>.txt` (Save Report button).

Dev/test env vars honored by `main.rs`: `SENSORVIEW_SOURCE=demo`, `SENSORVIEW_SHOW_SETTINGS`,
`SENSORVIEW_OPEN_GRAPH=<sensor-name-substring>`, `SENSORVIEW_START_LOGGING`.

---

## 7. Build & run

```bash
cd app
cargo test          # poll.rs + logging.rs unit tests
cargo run --release
```

**Windows prerequisites — non-obvious, these have bitten before:**
- Rust MSVC toolchain (`winget install Rustlang.Rustup`). `cargo` lives in `%USERPROFILE%\.cargo\bin`,
  which is **not** on the default PATH in a fresh shell.
- Linking **requires a VS Build Tools environment** — run inside `vcvars64.bat`, otherwise
  `link.exe not found`.
- A **Windows SDK** must be installed or you get `LNK1181: cannot open kernel32.lib`.
  (The VS Installer's `modify` path failed here with exit 5007; `winget install
  Microsoft.WindowsSDK.10.0.18362` worked.)
- Sidecar build: `dotnet publish sidecar -c Release -o sidecar/publish`.
  If NuGet says "No sources found": `dotnet nuget add source https://api.nuget.org/v3/index.json`.
- The release exe carries `requireAdministrator`, so it triggers UAC. Rebuilding while it's running
  gives "Access is denied" — close the app first.

**Linux CI deps:** `libgtk-3-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
libxkbcommon-dev libwayland-dev libssl-dev`.

---

## 8. CI / packaging

`.github/workflows/ci.yml` (push to master, PRs, manual) and `release.yml` (`v*` tags).
Matrix, `fail-fast: false`, `working-directory: app`:

| Runner | Formats |
| --- | --- |
| `windows-latest` | `nsis` |
| `ubuntu-22.04` | `deb,appimage` |
| `macos-latest` | `dmg` |

Steps: checkout → (Linux apt deps) → `dtolnay/rust-toolchain@stable` → `Swatinem/rust-cache@v2`
→ (Windows only) `dotnet publish sidecar` → `cargo test` → `cargo build --release`
→ `cargo install cargo-packager --locked` → `cargo packager --release --formats <…>` → upload artifacts.

**cargo-packager gotcha:** `resources` is a **top-level** field of `[package.metadata.packager]`,
not under `.windows`. It is a glob (`sidecar/publish/*.exe`) so it resolves to nothing on
Linux/macOS runners without erroring.

---

## 9. Current state

**Working:** all four HWiNFO-style windows; LHM sidecar bridge; min/max/avg + history; per-sensor
graph windows; CSV logging; text report export; persistent settings; three color modes; painted
device icons + vendor badges; elevation detection; driver diagnostics.

**Branch stack** (each stacked on the previous, all pushed to `origin` except the last):

```
master
 └ feature/scaffold → feature/ci-packaging → feature/sensor-core → feature/lhm-bridge
   → feature/hwinfo-ui → feature/ui-graphs-logging → feature/portable-build  ← current
```
Placeholder branches also exist: `feature/ui-sensors-table`, `feature/ui-system-summary`,
`feature/tray-settings`, `feature/native-cpu`, `feature/native-gpu`, `feature/native-linux-macos`.

**In progress:** `feature/portable-build` — a GitHub Actions job producing an **extract-and-run
archive** (no installer): Windows zip with `sensorview.exe` + `sensorview-bridge.exe` side by side
(works because `find_sidecar()` checks next-to-exe first), tar.gz for Linux/macOS. The workflow
file is **not yet written**.

**Known issues:**
1. **macOS `.dmg` CI leg fails** at the "Package installers" step. Windows and Linux pass. Exact
   error unread (GitHub's logs API returns 403 unauthenticated; the repo is public so job/step
   *status* is readable but log text is not — needs `gh auth login`).
2. **CPU power reads 0 W and the per-core clocks report no value.** Environmental, not a code bug.
   Two independent blockers are both active on the dev machine — confirmed by registry/CIM query
   and by HWiNFO's own manual (`C:\Program Files\HWiNFO64\HWiNFO Manual.pdf`, §10.5):

   | Blocker | Measured state | Breaks | Fix (user-side, needs elevation + reboot) |
   | --- | --- | --- | --- |
   | Vulnerable-driver blocklist | `VulnerableDriverBlocklistEnable = 1` | WinRing0 → all SMU/MSR power | Install **PawnIO** (signed, blocklist-clean; LHM 0.9.6 supports it) — https://pawnio.eu/ |
   | Virtualization-Based Security | `VirtualizationBasedSecurityStatus = 2` (running) even with Memory Integrity **off** (`HVCI Enabled = 0`) | BCLK reads → every ratio×BCLK clock | Manual §10.5: turning off Memory Integrity is often *not* enough; also disable **Virtual Machine Platform** and **Windows Hypervisor Platform** |

   Kernel-driver installs and security-feature changes are deliberately left to the user; the app's
   job is precise diagnosis, which the `DriverBlocked` banner provides.
   *Ruled out:* orphaned sidecars (a clean relaunch still showed 0).

   **This escalated into a total sensor outage** (fixed in `3ecb6ca`): with BCLK unavailable, LHM
   returned NaN/Infinity for the 8 per-core Clock and 8 per-core Factor sensors, and
   `System.Text.Json` throws rather than serialize non-finite floats — so the sidecar died with
   `0xE0434352` on its first snapshot and the app silently fell back to demo data. The sidecar now
   maps non-finite readings to `null` and survives a bad tick. Result: 151 sensors across 12
   devices read correctly; the 17 blocked ones report "no value" instead of taking everything down.
3. Per-core temps / TDP that LHM genuinely does not expose on Zen 4 remain absent until a native
   sensor engine exists.
4. "All CPU cores at 100 %" was investigated and is **not a bug** — PDH counters confirmed a real
   BOINC/World Community Grid workload pinning the CPU.

**Not built yet:** tray icon + autostart, shared-memory export, SMBus/I2C scanning, and the
pure-Rust native sensor engine (`feature/native-*`). The `SensorSource` trait exists precisely so
those can replace the .NET sidecar one device group at a time without touching the UI or model.

**Note:** `app/README.md`'s backend table and roadmap are **stale** (they predate the bridge and UI
work). This file supersedes them.
