//! HWiNFO-style native UI: main window + deferred viewports (Sensors Status,
//! System Summary, Settings), shared palette/theme, fonts and shared state.

pub mod graph_window;
pub mod hex_window;
pub mod main_window;
pub mod sensors_window;
pub mod settings_dialog;
pub mod summary_window;
pub mod widgets;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use eframe::egui::{self, Color32};

use crate::logging::CsvLogger;
use crate::poll::Command;
use crate::settings::{AppSettings, ColorMode};
use crate::state::{TelemetryFrame, TelemetryStore};
use crate::sysinfo::SystemInfoHandle;

// ---- Which extra windows are open (shared with deferred viewports) ------

#[derive(Default)]
pub struct WindowFlags {
    pub sensors: AtomicBool,
    pub summary: AtomicBool,
    pub settings: AtomicBool,
    pub hex: AtomicBool,
}

impl WindowFlags {
    pub fn open(flag: &AtomicBool) {
        flag.store(true, Ordering::Relaxed);
    }
    pub fn close(flag: &AtomicBool) {
        flag.store(false, Ordering::Relaxed);
    }
    pub fn is_open(flag: &AtomicBool) -> bool {
        flag.load(Ordering::Relaxed)
    }
}

/// Where the LAN dashboard is reachable, for display in the UI.
#[cfg(feature = "web")]
pub struct WebStatus {
    pub url: Option<String>,
    /// Present only when the server is LAN-exposed and therefore token-gated.
    pub token: Option<String>,
    pub error: Option<String>,
    pub lan: bool,
}

/// Everything a viewport callback needs. Cheap to clone (all Arcs).
#[derive(Clone)]
pub struct Shared {
    /// Latest telemetry. Read lock-free — the UI never blocks the poller.
    pub store: Arc<TelemetryStore>,
    /// Mutations are *sent* to the poll thread rather than applied under a
    /// shared lock, so the render loop never contends with polling.
    pub commands: std::sync::mpsc::Sender<Command>,
    pub settings: Arc<RwLock<AppSettings>>,
    pub sysinfo: SystemInfoHandle,
    pub windows: Arc<WindowFlags>,
    /// Sensor identifiers with an open graph window.
    pub graphs: Arc<RwLock<BTreeSet<String>>>,
    /// Active CSV logger (owned/written by the poll thread).
    pub logger: Arc<Mutex<Option<CsvLogger>>>,
    /// Whether this process is elevated (Some) or unknown/N-A (None). Detected
    /// in-process, so it's correct regardless of sidecar version.
    pub elevated: Option<bool>,
    pub started: Instant,
    #[cfg(feature = "web")]
    pub web: Arc<WebStatus>,
}

impl Shared {
    /// The latest telemetry frame. An atomic pointer read — cheap enough to
    /// call once per window per frame, unlike the deep tree clone it replaced.
    pub fn frame(&self) -> Arc<TelemetryFrame> {
        self.store.load()
    }

    /// Send a command to the poll thread. Failure means the poller has already
    /// stopped, which only happens during shutdown.
    pub fn command(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
    }

    pub fn color_mode(&self) -> ColorMode {
        self.settings.read().map(|s| s.color_mode).unwrap_or(ColorMode::Black)
    }

    pub fn palette(&self) -> Palette {
        Palette::of(self.color_mode())
    }

    /// Uptime formatted like HWiNFO's bottom-bar clock (h:mm:ss).
    pub fn uptime_text(&self) -> String {
        let secs = self.started.elapsed().as_secs();
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

// ---- Palette (HWiNFO Color Modes: Grey / Black / Light) -----------------

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub bg_panel: Color32,
    pub bg_header: Color32,
    pub row_even: Color32,
    pub row_odd: Color32,
    pub grid: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub accent: Color32,
    pub value: Color32,
    pub warn: Color32,
    pub crit: Color32,
    pub volt: Color32,
    pub clockc: Color32,
    pub tempc: Color32,
    pub fanc: Color32,
    pub ok_badge: Color32,
}

impl Palette {
    pub fn of(mode: ColorMode) -> Self {
        match mode {
            ColorMode::Black => Self {
                bg: Color32::from_rgb(0x12, 0x12, 0x12),
                bg_panel: Color32::from_rgb(0x1b, 0x1b, 0x1b),
                bg_header: Color32::from_rgb(0x26, 0x26, 0x28),
                row_even: Color32::from_rgb(0x1a, 0x1a, 0x1a),
                row_odd: Color32::from_rgb(0x14, 0x14, 0x14),
                grid: Color32::from_rgb(0x38, 0x38, 0x38),
                text: Color32::from_rgb(0xe6, 0xe6, 0xe6),
                text_dim: Color32::from_rgb(0x9a, 0x9a, 0x9a),
                accent: Color32::from_rgb(0x4f, 0xa3, 0xff),
                value: Color32::from_rgb(0xdc, 0xdc, 0xdc),
                warn: Color32::from_rgb(0xe8, 0xc0, 0x50),
                crit: Color32::from_rgb(0xf1, 0x4c, 0x4c),
                volt: Color32::from_rgb(0xf0, 0xd0, 0x30),
                clockc: Color32::from_rgb(0x60, 0xc8, 0xf0),
                tempc: Color32::from_rgb(0xf0, 0x80, 0x60),
                fanc: Color32::from_rgb(0x80, 0xd0, 0x80),
                ok_badge: Color32::from_rgb(0x30, 0xa0, 0x40),
            },
            ColorMode::Grey => Self {
                bg: Color32::from_rgb(0x2e, 0x2e, 0x30),
                bg_panel: Color32::from_rgb(0x38, 0x38, 0x3a),
                bg_header: Color32::from_rgb(0x44, 0x44, 0x48),
                row_even: Color32::from_rgb(0x36, 0x36, 0x38),
                row_odd: Color32::from_rgb(0x30, 0x30, 0x32),
                grid: Color32::from_rgb(0x55, 0x55, 0x58),
                text: Color32::from_rgb(0xea, 0xea, 0xea),
                text_dim: Color32::from_rgb(0xaa, 0xaa, 0xaa),
                accent: Color32::from_rgb(0x6c, 0xb4, 0xff),
                value: Color32::from_rgb(0xe6, 0xe6, 0xe6),
                warn: Color32::from_rgb(0xe8, 0xc0, 0x50),
                crit: Color32::from_rgb(0xf1, 0x4c, 0x4c),
                volt: Color32::from_rgb(0xf0, 0xd0, 0x30),
                clockc: Color32::from_rgb(0x70, 0xd0, 0xf8),
                tempc: Color32::from_rgb(0xf0, 0x88, 0x68),
                fanc: Color32::from_rgb(0x90, 0xd8, 0x90),
                ok_badge: Color32::from_rgb(0x38, 0xa8, 0x48),
            },
            ColorMode::Light => Self {
                bg: Color32::from_rgb(0xf2, 0xf2, 0xf2),
                bg_panel: Color32::from_rgb(0xfa, 0xfa, 0xfa),
                bg_header: Color32::from_rgb(0xdd, 0xdd, 0xe2),
                row_even: Color32::from_rgb(0xee, 0xee, 0xee),
                row_odd: Color32::from_rgb(0xf6, 0xf6, 0xf6),
                grid: Color32::from_rgb(0xc0, 0xc0, 0xc4),
                text: Color32::from_rgb(0x1a, 0x1a, 0x1a),
                text_dim: Color32::from_rgb(0x60, 0x60, 0x60),
                accent: Color32::from_rgb(0x0a, 0x64, 0xc8),
                value: Color32::from_rgb(0x10, 0x10, 0x10),
                warn: Color32::from_rgb(0xb0, 0x80, 0x00),
                crit: Color32::from_rgb(0xc8, 0x20, 0x20),
                volt: Color32::from_rgb(0xa0, 0x80, 0x00),
                clockc: Color32::from_rgb(0x00, 0x78, 0xa8),
                tempc: Color32::from_rgb(0xc0, 0x40, 0x20),
                fanc: Color32::from_rgb(0x20, 0x80, 0x20),
                ok_badge: Color32::from_rgb(0x20, 0x88, 0x30),
            },
        }
    }
}

// ---- Theme / fonts ------------------------------------------------------

pub fn apply_theme(ctx: &egui::Context, pal: &Palette, light: bool) {
    ctx.set_theme(if light { egui::Theme::Light } else { egui::Theme::Dark });
    let pal = *pal;
    ctx.all_styles_mut(move |style| {
        style.visuals.panel_fill = pal.bg;
        style.visuals.window_fill = pal.bg_panel;
        style.visuals.extreme_bg_color = pal.bg;
        style.visuals.override_text_color = Some(pal.text);
        style.visuals.widgets.noninteractive.bg_stroke.color = pal.grid;
        style.spacing.item_spacing = egui::vec2(4.0, 2.0);
        style.spacing.button_padding = egui::vec2(8.0, 3.0);
    });
}

/// Per-platform system font candidates, best first.
///
/// Only plain `.ttf` files are listed. macOS's `.ttc` collections (Menlo,
/// Helvetica) are deliberately excluded: ab_glyph, which egui parses fonts
/// with, does not handle TrueType collections, so those load as garbage or
/// fail outright. `SFNSMono.ttf` and `Monaco.ttf` are both real single-face
/// files.
#[cfg(target_os = "macos")]
const PROPORTIONAL_FONTS: &[&str] =
    &["/System/Library/Fonts/SFNS.ttf", "/System/Library/Fonts/Geneva.ttf"];
#[cfg(target_os = "macos")]
const MONOSPACE_FONTS: &[&str] =
    &["/System/Library/Fonts/SFNSMono.ttf", "/System/Library/Fonts/Monaco.ttf"];

#[cfg(windows)]
const PROPORTIONAL_FONTS: &[&str] = &[r"C:\Windows\Fonts\segoeui.ttf"];
// Cascadia Mono ships with Windows 11; Consolas is the older fallback.
#[cfg(windows)]
const MONOSPACE_FONTS: &[&str] =
    &[r"C:\Windows\Fonts\CascadiaMono.ttf", r"C:\Windows\Fonts\consola.ttf"];

#[cfg(not(any(windows, target_os = "macos")))]
const PROPORTIONAL_FONTS: &[&str] =
    &["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"];
#[cfg(not(any(windows, target_os = "macos")))]
const MONOSPACE_FONTS: &[&str] =
    &["/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"];

/// Load the platform UI face for proportional text and a real monospace face
/// for the hex dump — both align hex columns far better than egui's bundled
/// face. Silently keeps egui's defaults when nothing loads (CI, minimal
/// container images).
///
/// A readable file is not the same as a usable font, so each candidate is
/// parsed before being installed and a font that fails to parse falls through
/// to the next.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut changed = false;

    for (key, candidates, family) in [
        ("ui", PROPORTIONAL_FONTS, egui::FontFamily::Proportional),
        ("mono", MONOSPACE_FONTS, egui::FontFamily::Monospace),
    ] {
        let Some(bytes) = candidates.iter().find_map(|p| load_font(p)) else {
            continue;
        };
        fonts
            .font_data
            .insert(key.into(), Arc::new(egui::FontData::from_owned(bytes)));
        if let Some(family) = fonts.families.get_mut(&family) {
            family.insert(0, key.into());
        }
        changed = true;
    }

    if changed {
        ctx.set_fonts(fonts);
    }
}

/// Read a font file and confirm it is a *single* sfnt face.
///
/// Existence is not enough: egui parses fonts with ab_glyph, which cannot read
/// TrueType **collections**. Handing it a `.ttc` (Menlo, Helvetica on macOS)
/// yields blank glyphs rather than a clean error, so reject by magic number
/// instead of trusting the extension.
fn load_font(path: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    let magic = bytes.get(..4)?;
    match magic {
        // 0x00010000 (TrueType), "true" (legacy Apple), "OTTO" (CFF outlines).
        [0x00, 0x01, 0x00, 0x00] | b"true" | b"OTTO" => Some(bytes),
        // "ttcf" — a collection. Unsupported; fall through to the next candidate.
        _ => None,
    }
}


// ---- Viewport registration ---------------------------------------------

/// Re-register every open deferred viewport. Must be called each frame from
/// the root `App::ui`.
pub fn show_open_viewports(ctx: &egui::Context, shared: &Shared) {
    if WindowFlags::is_open(&shared.windows.sensors) {
        let s = shared.clone();
        ctx.show_viewport_deferred(
            egui::ViewportId::from_hash_of("sensors"),
            egui::ViewportBuilder::default()
                .with_title("SensorView Sensors Status")
                .with_inner_size([560.0, 760.0])
                .with_min_inner_size([300.0, 300.0]),
            move |ui, _class| sensors_window::show(ui, &s),
        );
    }
    if WindowFlags::is_open(&shared.windows.summary) {
        let s = shared.clone();
        ctx.show_viewport_deferred(
            egui::ViewportId::from_hash_of("summary"),
            egui::ViewportBuilder::default()
                .with_title("SensorView - System Summary")
                .with_inner_size([900.0, 640.0])
                .with_min_inner_size([700.0, 500.0]),
            move |ui, _class| summary_window::show(ui, &s),
        );
    }
    if WindowFlags::is_open(&shared.windows.hex) {
        let s = shared.clone();
        ctx.show_viewport_deferred(
            egui::ViewportId::from_hash_of("hex"),
            egui::ViewportBuilder::default()
                .with_title("Hex Viewer")
                // Wide enough for offset + 16 bytes + ASCII without wrapping.
                .with_inner_size([900.0, 620.0])
                .with_min_inner_size([700.0, 320.0]),
            move |ui, _class| hex_window::show(ui, &s),
        );
    }
    if WindowFlags::is_open(&shared.windows.settings) {
        let s = shared.clone();
        ctx.show_viewport_deferred(
            egui::ViewportId::from_hash_of("settings"),
            egui::ViewportBuilder::default()
                .with_title("SensorView - Settings")
                .with_inner_size([680.0, 430.0])
                .with_resizable(false),
            move |ui, _class| settings_dialog::show(ui, &s),
        );
    }
    // One deferred viewport per open sensor graph.
    let open_graphs: Vec<String> = shared
        .graphs
        .read()
        .map(|g| g.iter().cloned().collect())
        .unwrap_or_default();
    for id in open_graphs {
        let s = shared.clone();
        let vid = egui::ViewportId::from_hash_of(("graph", &id));
        let graph_id = id.clone();
        ctx.show_viewport_deferred(
            vid,
            egui::ViewportBuilder::default()
                .with_title("SensorView - Graph")
                .with_inner_size([420.0, 240.0]),
            move |ui, _class| graph_window::show(ui, &s, &graph_id),
        );
    }
}

/// Standard close-request handling for a deferred viewport: flips its flag.
pub fn handle_close(ui: &egui::Ui, flag: &AtomicBool) {
    if ui.ctx().input(|i| i.viewport().close_requested()) {
        WindowFlags::close(flag);
    }
}

#[cfg(test)]
mod font_tests {
    use super::*;

    /// The whole point of the magic check: on macOS the tempting candidates
    /// (Menlo, Helvetica) are collections and must be rejected, while the ones
    /// actually listed must load.
    #[cfg(target_os = "macos")]
    #[test]
    fn collections_are_rejected_and_listed_faces_load() {
        for ttc in ["/System/Library/Fonts/Menlo.ttc", "/System/Library/Fonts/Helvetica.ttc"] {
            if std::path::Path::new(ttc).exists() {
                assert!(load_font(ttc).is_none(), "{ttc} is a collection and must be rejected");
            }
        }
        // Not asserted: a stripped CI runner image may ship neither, and
        // install_fonts already falls back to egui's bundled face. The load
        // -bearing part of this test is the .ttc rejection above.
        for (kind, candidates) in
            [("proportional", PROPORTIONAL_FONTS), ("monospace", MONOSPACE_FONTS)]
        {
            if !candidates.iter().any(|p| load_font(p).is_some()) {
                eprintln!("SKIP: no usable {kind} system font on this machine");
            }
        }
    }

    #[test]
    fn missing_font_is_none() {
        assert!(load_font("/no/such/font.ttf").is_none());
    }
}
