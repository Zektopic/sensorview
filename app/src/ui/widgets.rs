//! Shared HWiNFO-style widgets: painted sensor-type icons, group header bands,
//! summary panels, feature "chips" and square checkboxes.

use eframe::egui::{self, Color32, Pos2, RichText, Stroke, StrokeKind, Vec2};

use super::Palette;
use crate::model::SensorType;

pub const ROW_H: f32 = 17.0;

/// Icon color per sensor type, HWiNFO-ish (yellow bolts, cyan clocks…).
pub fn type_color(t: SensorType, pal: &Palette) -> Color32 {
    match t {
        SensorType::Voltage | SensorType::Current | SensorType::Power | SensorType::Energy => pal.volt,
        SensorType::Clock | SensorType::Frequency | SensorType::TimeSpan => pal.clockc,
        SensorType::Temperature => pal.tempc,
        SensorType::Fan | SensorType::Flow | SensorType::Control => pal.fanc,
        _ => pal.text_dim,
    }
}

/// Parse a hardware vendor from a device name → (badge text, brand color).
/// Trademark-safe: our own colored text badge, no copied logos.
pub fn vendor_badge(name: &str) -> Option<(&'static str, Color32)> {
    let n = name.to_uppercase();
    if n.contains("NVIDIA") || n.contains("GEFORCE") || n.contains("RTX") || n.contains("GTX") {
        Some(("NVIDIA", Color32::from_rgb(0x76, 0xb9, 0x00)))
    } else if n.contains("RADEON") || n.contains("AMD") || n.contains("RYZEN") {
        Some(("AMD", Color32::from_rgb(0xed, 0x1c, 0x24)))
    } else if n.contains("INTEL") || n.contains("CORE I") {
        Some(("INTEL", Color32::from_rgb(0x00, 0x71, 0xc5)))
    } else if n.contains("CORSAIR") {
        Some(("CORSAIR", Color32::from_rgb(0xff, 0xd2, 0x00)))
    } else if n.contains("SAMSUNG") {
        Some(("SAMSUNG", Color32::from_rgb(0x14, 0x28, 0xa0)))
    } else if n.contains("MSI") {
        Some(("MSI", Color32::from_rgb(0xd4, 0x00, 0x00)))
    } else {
        None
    }
}

/// Paint a small colored category glyph for a hardware type (group bands/tree).
pub fn hardware_icon(ui: &egui::Ui, rect: egui::Rect, t: crate::model::HardwareType, pal: &Palette) {
    use crate::model::HardwareType as H;
    let p = ui.painter();
    let c = rect.center();
    let col = match t {
        H::Cpu => pal.accent,
        H::GpuNvidia | H::GpuAti | H::GpuIntel | H::GpuApple => pal.ok_badge,
        H::Ram => pal.clockc,
        H::Storage | H::Hdd => pal.warn,
        H::Network => pal.fanc,
        H::Battery | H::Psu => pal.volt,
        _ => pal.text_dim,
    };
    match t {
        H::Cpu | H::Mainboard | H::SuperIO | H::EmbeddedController => {
            // Chip: square with pins.
            let r = egui::Rect::from_center_size(c, Vec2::splat(8.0));
            p.rect_stroke(r, 1.0, Stroke::new(1.2, col), StrokeKind::Inside);
            let inner = egui::Rect::from_center_size(c, Vec2::splat(3.5));
            p.rect_filled(inner, 0.0, col);
        }
        H::GpuNvidia | H::GpuAti | H::GpuIntel | H::GpuApple => {
            // Card: rectangle + fan circle.
            let r = egui::Rect::from_min_size(Pos2::new(c.x - 5.0, c.y - 3.5), Vec2::new(10.0, 7.0));
            p.rect_stroke(r, 1.0, Stroke::new(1.2, col), StrokeKind::Inside);
            p.circle_stroke(Pos2::new(c.x + 1.5, c.y), 1.8, Stroke::new(1.0, col));
        }
        H::Ram => {
            // Memory stick.
            let r = egui::Rect::from_min_size(Pos2::new(c.x - 5.0, c.y - 3.0), Vec2::new(10.0, 6.0));
            p.rect_stroke(r, 0.0, Stroke::new(1.2, col), StrokeKind::Inside);
            for dx in [-2.5, 0.0, 2.5] {
                p.line_segment(
                    [Pos2::new(c.x + dx, c.y + 3.0), Pos2::new(c.x + dx, c.y + 5.0)],
                    Stroke::new(1.0, col),
                );
            }
        }
        H::Storage | H::Hdd => {
            p.circle_stroke(c, 4.5, Stroke::new(1.2, col));
            p.circle_filled(c, 1.2, col);
        }
        H::Network => {
            p.circle_filled(Pos2::new(c.x - 3.0, c.y + 3.0), 1.5, col);
            p.circle_filled(Pos2::new(c.x, c.y - 3.0), 1.5, col);
            p.circle_filled(Pos2::new(c.x + 3.0, c.y + 3.0), 1.5, col);
        }
        _ => {
            p.circle_filled(c, 3.0, col);
        }
    }
}

/// Paint a small colored dot marker for a sensor type at the cursor (used in
/// the graph header where a full icon would be overkill).
pub fn sensor_icon_at_cursor(ui: &mut egui::Ui, t: SensorType, pal: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, type_color(t, pal));
}

/// Allocate a 15×15 slot and paint the category icon inline (device tree rows).
pub fn hardware_icon_inline(ui: &mut egui::Ui, t: crate::model::HardwareType, pal: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(15.0), egui::Sense::hover());
    hardware_icon(ui, rect, t, pal);
}

/// Collapsible group header band ("CPU [#0]: AMD Ryzen 7 7700"). Returns the
/// new collapsed state (None = unchanged).
pub fn group_header(
    ui: &mut egui::Ui,
    title: &str,
    hw_type: crate::model::HardwareType,
    collapsed: bool,
    width: f32,
    pal: &Palette,
) -> Option<bool> {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, ROW_H + 1.0), egui::Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, 0.0, pal.bg_header);
    p.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, pal.grid),
    );
    let chev = if collapsed { "▸" } else { "▾" };
    p.text(
        Pos2::new(rect.left() + 4.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        chev,
        egui::FontId::proportional(9.0),
        pal.text_dim,
    );
    // Category icon.
    let icon_rect = egui::Rect::from_center_size(
        Pos2::new(rect.left() + 20.0, rect.center().y),
        Vec2::splat(12.0),
    );
    hardware_icon(ui, icon_rect, hw_type, pal);

    // Vendor badge (if recognizable), then title.
    let mut x = rect.left() + 30.0;
    if let Some((vendor, color)) = vendor_badge(title) {
        let galley = ui.painter().layout_no_wrap(
            vendor.to_string(),
            egui::FontId::proportional(8.5),
            Color32::WHITE,
        );
        let bw = galley.size().x + 6.0;
        let brect = egui::Rect::from_min_size(
            Pos2::new(x, rect.center().y - 6.0),
            Vec2::new(bw, 12.0),
        );
        ui.painter().rect_filled(brect, 2.0, color);
        ui.painter().galley(Pos2::new(x + 3.0, rect.center().y - galley.size().y / 2.0), galley, Color32::WHITE);
        x += bw + 4.0;
    }
    ui.painter().text(
        Pos2::new(x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(11.0),
        pal.text,
    );
    if resp.clicked() {
        Some(!collapsed)
    } else {
        None
    }
}

/// Boxed section with a header strip — the System Summary panel look.
pub fn panel<R>(
    ui: &mut egui::Ui,
    title: &str,
    pal: &Palette,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::Frame::new()
        .fill(pal.bg_panel)
        .stroke(Stroke::new(1.0, pal.grid))
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            ui.label(RichText::new(title).color(pal.accent).size(11.5).strong());
            ui.separator();
            add(ui)
        })
        .inner
}

/// Small feature "chip" (the green/dark ISA boxes in the Summary CPU panel).
/// The label never wraps — chips flow as whole units in a wrapped row.
pub fn chip(ui: &mut egui::Ui, label: &str, on: bool, pal: &Palette) {
    let (bg, fg) = if on {
        (pal.ok_badge, Color32::WHITE)
    } else {
        (pal.bg_header, pal.text_dim)
    };
    egui::Frame::new()
        .fill(bg)
        .corner_radius(2)
        .inner_margin(egui::Margin::symmetric(4, 1))
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(RichText::new(label).color(fg).size(9.5))
                    .wrap_mode(egui::TextWrapMode::Extend),
            );
        });
}

/// Square `[x]` checkbox like HWiNFO's settings dialog.
pub fn square_check(ui: &mut egui::Ui, value: &mut bool, label: &str, pal: &Palette) -> bool {
    let resp = ui
        .horizontal(|ui| {
            let (rect, r) = ui.allocate_exact_size(Vec2::splat(13.0), egui::Sense::click());
            let p = ui.painter();
            p.rect_stroke(rect, 0.0, Stroke::new(1.0, pal.text_dim), StrokeKind::Inside);
            if *value {
                p.line_segment(
                    [rect.left_top() + Vec2::splat(2.5), rect.right_bottom() - Vec2::splat(2.5)],
                    Stroke::new(1.6, pal.text),
                );
                p.line_segment(
                    [
                        Pos2::new(rect.right() - 2.5, rect.top() + 2.5),
                        Pos2::new(rect.left() + 2.5, rect.bottom() - 2.5),
                    ],
                    Stroke::new(1.6, pal.text),
                );
            }
            let lr = ui.label(RichText::new(label).size(11.5).color(pal.text));
            r.union(lr.interact(egui::Sense::click()))
        })
        .inner;
    if resp.clicked() {
        *value = !*value;
        true
    } else {
        false
    }
}

/// `label: value` row for info grids (Feature pane, Summary fields).
pub fn info_row(ui: &mut egui::Ui, label: &str, value: &str, pal: &Palette) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(pal.text_dim).size(11.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(if value.is_empty() { "—" } else { value }).color(pal.text).size(11.0));
        });
    });
}

/// Colored status badge (UEFI Boot / Secure Boot / HVCI rows).
pub fn badge(ui: &mut egui::Ui, label: &str, ok: Option<bool>, pal: &Palette) {
    let (bg, text) = match ok {
        Some(true) => (pal.ok_badge, "Enabled"),
        Some(false) => (pal.bg_header, "Disabled"),
        None => (pal.bg_header, "—"),
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(pal.text_dim).size(11.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            egui::Frame::new()
                .fill(bg)
                .corner_radius(2)
                .inner_margin(egui::Margin::symmetric(6, 1))
                .show(ui, |ui| {
                    ui.label(RichText::new(text).color(Color32::WHITE).size(10.0));
                });
        });
    });
}

// Formatting moved to `crate::format` so the report, CLI and TUI can use it
// without linking the GUI toolkit. Re-exported here because every UI call site
// already says `widgets::format_value`.
pub use crate::format::format_value;
