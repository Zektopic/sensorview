//! SensorView — a native, HWiNFO-style hardware monitor with a LAN dashboard.
//!
//! # Thread topology
//!
//! ```text
//!   ┌─ Thread 1  poll::spawn ───────────────────────────────────────────┐
//!   │  fast lane ~1 s: SensorSource → Monitor (min/max/avg)             │
//!   │  publishes an immutable TelemetryFrame — the ONLY writer          │
//!   └───────────┬──────────────────────────────────────┬────────────────┘
//!               │ ArcSwap (atomic ptr)                 │ broadcast channel
//!               ▼                                      ▼
//!   ┌─ Thread 2  GUI (this thread) ──┐   ┌─ Thread 3  web::spawn ───────┐
//!   │  eframe/egui, lock-free reads  │   │  tokio + axum, /ws/telemetry │
//!   └────────────────────────────────┘   └──────────────────────────────┘
//!
//!   ┌─ Thread 1b  inventory::spawn_collector ~30 s ─────────────────────┐
//!   │  S.M.A.R.T. / SPD / PCIe topology → ArcSwap, read by Thread 1     │
//!   └───────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Why it can't deadlock
//!
//! * **One writer.** Only Thread 1 mutates telemetry. Threads 2 and 3 read
//!   through [`state::TelemetryStore::load`], an atomic pointer read that never
//!   blocks and cannot be poisoned.
//! * **No shared lock on the hot path.** The GUI does not lock the poller to
//!   read; UI-initiated mutations (reset min/max, change interval) are *sent*
//!   as [`poll::Command`]s over an `mpsc` channel.
//! * **No guard across `.await`.** Thread 3 touches only the `ArcSwap` and the
//!   broadcast channel; `web/` denies `clippy::await_holding_lock`.
//! * **No lock-ordering cycle.** The one remaining lock (the store's history
//!   ring) is a leaf: never acquired while holding another.
//! * **Backpressure is bounded.** `broadcast::Sender::send` never blocks, so a
//!   stalled browser cannot slow the hardware loop or the UI.
//! * **Shutdown is ordered.** GUI exit → stop Thread 3 (releases the port) →
//!   stop Thread 1 (releases the sensor driver) → stop Thread 1b.

// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod inventory;
mod logging;
mod model;
mod poll;
mod report;
mod settings;
mod source;
mod state;
mod sysinfo;
mod ui;
#[cfg(feature = "web")]
mod web;

#[cfg(feature = "web")]
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use eframe::egui;

use settings::AppSettings;
use state::TelemetryStore;
use ui::{main_window, Shared, WindowFlags};

/// How many ticks a WebSocket client may fall behind before it is told it
/// lagged and resyncs. Small on purpose: for a live dashboard, the newest frame
/// is the only interesting one.
const BROADCAST_CAPACITY: usize = 16;

fn main() -> eframe::Result {
    let app_settings = AppSettings::load();

    // ---- Shared state ----------------------------------------------------
    let store = Arc::new(TelemetryStore::new(BROADCAST_CAPACITY));
    let sysinfo = sysinfo::spawn_query();
    // Shared with the UI, which toggles logging; the poll thread does the writing.
    let logger: poll::LoggerSlot = Arc::new(Mutex::new(None));

    let poll_config = poll::PollConfig {
        fast: Duration::from_millis(app_settings.poll_interval_ms),
        // Floor of 5 s: S.M.A.R.T. and SPD reads are expensive and keep drives
        // awake, so no setting may turn the slow lane into a second fast one.
        slow: Duration::from_secs(app_settings.inventory_interval_s.max(5)),
    };

    // ---- Thread 1b: slow lane (S.M.A.R.T. / SPD / topology) --------------
    let collector = inventory::spawn_collector(inventory::default_inventory(), poll_config.slow);

    // ---- Thread 1: hardware poller --------------------------------------
    let poller = poll::spawn(
        store.clone(),
        source::default_source(),
        collector,
        logger.clone(),
        poll_config,
    );

    // ---- Thread 3: web dashboard ----------------------------------------
    // Started before the GUI so a bind failure (port in use) is known by the
    // time the first frame is drawn, and can be shown rather than guessed at.
    #[cfg(feature = "web")]
    let web = web::spawn(
        store.clone(),
        sysinfo.clone(),
        web::WebConfig {
            enabled: app_settings.web_enabled,
            bind: if app_settings.web_lan_access {
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            } else {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            },
            port: app_settings.web_port,
        }
        .with_env_overrides(),
    );

    // Announce the dashboard the way any server does. Release builds have no
    // console (`windows_subsystem = "windows"`), so this is for development and
    // for anyone launching from a terminal; the UI shows the same thing.
    #[cfg(feature = "web")]
    match (&web.url(), &web.error) {
        (Some(url), _) => eprintln!("SensorView dashboard: {url}"),
        (None, Some(err)) => eprintln!("SensorView dashboard unavailable: {err}"),
        (None, None) => {}
    }

    let shared = Shared {
        store: store.clone(),
        commands: poller.sender(),
        settings: Arc::new(RwLock::new(app_settings.clone())),
        sysinfo,
        windows: Arc::new(WindowFlags::default()),
        graphs: Arc::new(RwLock::new(std::collections::BTreeSet::new())),
        logger,
        elevated: sysinfo::is_elevated(),
        started: Instant::now(),
        #[cfg(feature = "web")]
        web: Arc::new(ui::WebStatus {
            url: web.url(),
            token: web.token.clone(),
            error: web.error.clone(),
            lan: app_settings.web_lan_access,
        }),
    };

    // Startup windows per settings (HWiNFO's "Show … on Startup").
    shared
        .windows
        .summary
        .store(app_settings.show_summary_on_startup, Ordering::Relaxed);
    shared
        .windows
        .sensors
        .store(app_settings.show_sensors_on_startup, Ordering::Relaxed);
    // Dev/testing affordances (env-gated, inert in normal use). They exist so
    // the windows that only open on user interaction — Settings, Graph — can be
    // smoke-tested without driving the mouse.
    if std::env::var("SENSORVIEW_SHOW_SETTINGS").is_ok() {
        shared.windows.settings.store(true, Ordering::Relaxed);
    }
    if std::env::var("SENSORVIEW_SHOW_HEX").is_ok() {
        shared.windows.hex.store(true, Ordering::Relaxed);
    }
    let open_graph = std::env::var("SENSORVIEW_OPEN_GRAPH").ok();
    let start_logging = std::env::var("SENSORVIEW_START_LOGGING").is_ok();
    if open_graph.is_some() || start_logging {
        // Both need real sensors, which only exist once the poller has ticked.
        let frame = wait_for_first_frame(&store, Duration::from_secs(20));
        if let Some(needle) = &open_graph {
            if let Some(id) = first_sensor_matching(&frame.tree, needle) {
                shared.windows.sensors.store(true, Ordering::Relaxed);
                if let Ok(mut g) = shared.graphs.write() {
                    g.insert(id);
                }
            } else {
                eprintln!("SENSORVIEW_OPEN_GRAPH: no sensor name contains {needle:?}");
            }
        }
        if start_logging {
            match logging::CsvLogger::start(&frame.tree) {
                Ok(l) => {
                    *shared.logger.lock().expect("fresh logger slot") = Some(l);
                    shared.windows.sensors.store(true, Ordering::Relaxed);
                }
                Err(e) => eprintln!("SENSORVIEW_START_LOGGING: {e}"),
            }
        }
    }

    if std::env::var("SENSORVIEW_HEADLESS").is_ok() {
        #[cfg(feature = "web")]
        if let Some(url) = web.url() {
            println!("SensorView running in headless mode.");
            println!("Dashboard URL: {url}");
        } else {
            println!("SensorView running in headless mode.");
        }
        std::thread::park();
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SensorView")
            .with_inner_size([760.0, 560.0])
            .with_min_inner_size([620.0, 420.0])
            .with_icon(load_icon()),
        ..Default::default()
    };

    // ---- Thread 2: the GUI, on the main thread (winit requires it) -------
    eframe::run_native(
        "SensorView",
        options,
        Box::new(move |cc| {
            ui::install_fonts(&cc.egui_ctx);
            // The poller wakes the UI after each publish, so egui repaints on
            // new data rather than spinning at the display refresh rate.
            let ctx = cc.egui_ctx.clone();
            poller.on_tick(move || ctx.request_repaint());
            Ok(Box::new(SensorViewApp {
                shared,
                main_state: main_window::MainWindowState::default(),
                poller,
                #[cfg(feature = "web")]
                web,
            }))
        }),
    )
}

struct SensorViewApp {
    shared: Shared,
    main_state: main_window::MainWindowState,
    /// Owning the handles here means `on_exit` can shut the threads down in a
    /// defined order, and `Drop` is a backstop if the process exits another way.
    poller: poll::PollHandle,
    #[cfg(feature = "web")]
    web: web::WebHandle,
}

impl eframe::App for SensorViewApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Theme follows the settings' Color Mode (switchable live).
        let pal = self.shared.palette();
        let light = self.shared.color_mode() == settings::ColorMode::Light;
        ui::apply_theme(ui.ctx(), &pal, light);

        main_window::show(ui, &self.shared, &mut self.main_state);
        ui::show_open_viewports(ui.ctx(), &self.shared);
    }

    fn on_exit(&mut self) {
        if let Ok(st) = self.shared.settings.read() {
            if st.remember_preferences {
                st.save();
            }
        }
        // Ordered shutdown: release the port first so a quick restart can
        // rebind, then the sensor driver (which the sidecar holds open).
        #[cfg(feature = "web")]
        self.web.stop();
        self.poller.stop();
    }
}

/// Block until the poller publishes its first frame, or `timeout` elapses.
/// Only used by the env-gated dev hooks, which need real sensor identifiers.
fn wait_for_first_frame(
    store: &Arc<TelemetryStore>,
    timeout: Duration,
) -> Arc<state::TelemetryFrame> {
    let deadline = Instant::now() + timeout;
    loop {
        let frame = store.load();
        if frame.seq > 0 || Instant::now() >= deadline {
            return frame;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// First sensor identifier whose name contains `needle` (case-insensitive).
fn first_sensor_matching(tree: &[model::Hardware], needle: &str) -> Option<String> {
    let needle = needle.to_lowercase();
    fn walk(tree: &[model::Hardware], needle: &str) -> Option<String> {
        for hw in tree {
            for s in &hw.sensors {
                if s.name.to_lowercase().contains(needle) {
                    return Some(s.identifier.clone());
                }
            }
            if let Some(found) = walk(&hw.sub_hardware, needle) {
                return Some(found);
            }
        }
        None
    }
    walk(tree, &needle)
}

/// Window icon (32×32 PNG baked into the binary).
fn load_icon() -> egui::IconData {
    let bytes = include_bytes!("../assets/32x32.png");
    let img = image::load_from_memory(bytes)
        .expect("embedded icon is valid PNG")
        .into_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData { rgba: img.into_raw(), width, height }
}

#[cfg(test)]
mod packaging_assets {
    //! Guards the icon set that `cargo packager` turns into the macOS `.icns`.
    //!
    //! Every icon listed under `[package.metadata.packager] icons` is opened and
    //! passed to `IconType::from_pixel_size_and_density(w, h, density)`; a `None`
    //! aborts the whole macOS job with "No matching IconType". Density is 2 only
    //! when the filename contains `@2x`.
    //!
    //! That bit us with a plain 1024×1024 `icon.png`: 1024 px exists **only** as
    //! `512@2x` (OSType `ic10`), so at density 1 it has no type. The failure is
    //! invisible on Windows and Linux, which is why it survived several releases
    //! — hence a check that runs on every platform.

    use std::path::{Path, PathBuf};

    /// Pixel sizes ICNS accepts at 1× — OSTypes icp4/icp5/ih32/icp6/ic07/ic08/ic09.
    const SIZES_1X: &[u32] = &[16, 32, 48, 64, 128, 256, 512];
    /// Pixel sizes ICNS accepts at 2× — ic11 (16@2x), ic12 (32@2x), ic13
    /// (128@2x), ic14 (256@2x), ic10 (512@2x). Note there is no 128 px @2x.
    const SIZES_2X: &[u32] = &[32, 64, 256, 512, 1024];

    fn assets_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("assets")
    }

    /// Width/height straight out of the PNG IHDR — no image decode needed.
    fn png_size(path: &Path) -> (u32, u32) {
        let b = std::fs::read(path).expect("read png");
        assert_eq!(&b[1..4], b"PNG", "{} is not a PNG", path.display());
        let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
        let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
        (w, h)
    }

    /// Largest frame in an ICO — what `image::open` hands the packager.
    fn ico_largest(path: &Path) -> u32 {
        let b = std::fs::read(path).expect("read ico");
        let count = u16::from_le_bytes([b[4], b[5]]) as usize;
        (0..count)
            .map(|i| match b[6 + i * 16] {
                0 => 256, // 0 encodes 256 in the ICO directory
                w => w as u32,
            })
            .max()
            .expect("ico has at least one frame")
    }

    fn accepted(size: u32, retina: bool) -> bool {
        if retina { SIZES_2X } else { SIZES_1X }.contains(&size)
    }

    #[test]
    fn icon_assets_map_to_valid_icns_types() {
        let dir = assets_dir();
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("assets dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("png") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let (w, h) = png_size(&path);
            assert_eq!(w, h, "{name} is {w}×{h}; icons must be square");

            let retina = name.contains("@2x");
            assert!(
                accepted(w, retina),
                "{name} is {w}px at {}× — no ICNS type matches, so `cargo packager` \
                 will fail the macOS build with \"No matching IconType\". Valid \
                 sizes: 1× {SIZES_1X:?}, 2× {SIZES_2X:?}.",
                if retina { 2 } else { 1 }
            );
            checked += 1;
        }
        assert!(checked >= 4, "expected the full icon family, found {checked} PNGs");
    }

    #[test]
    fn ico_largest_frame_is_a_valid_icns_size() {
        // The .ico is in the same `icons` list, so it goes through the identical
        // conversion — a 1024px frame would fail exactly like the PNG did.
        let size = ico_largest(&assets_dir().join("icon.ico"));
        assert!(
            accepted(size, false),
            "icon.ico's largest frame is {size}px, which has no 1× ICNS type"
        );
    }

    #[test]
    fn the_1024_master_is_marked_retina() {
        // The specific regression: 1024px is legal only as 512@2x.
        let path = assets_dir().join("icon@2x.png");
        assert!(path.is_file(), "the 1024px master must keep its @2x name");
        assert_eq!(png_size(&path), (1024, 1024));
        assert!(accepted(1024, true), "1024px is valid at 2×");
        assert!(!accepted(1024, false), "1024px must NOT be accepted at 1×");
    }
}
