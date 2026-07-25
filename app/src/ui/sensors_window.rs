//! HWiNFO-style "Sensors Status" window.
//!
//! Multi-column flow layout: `Sensor | Current` pairs packed into vertical
//! columns that wrap left→right (groups may split across columns, matching
//! HWiNFO), collapsible group bands, per-type colored icons, warning colors,
//! CPU load in the window title and the uptime / reset / settings toolbar.

use eframe::egui::{self, Align2, FontId, Pos2, RichText, Sense, Stroke, Vec2};

use super::widgets::{self, ROW_H};
use super::{Palette, Shared, WindowFlags};
use crate::model::{Hardware, HardwareType, Sensor, SensorType};

const COL_MIN_W: f32 = 380.0;

enum Item {
    Header { title: String, id: String, hw_type: HardwareType, collapsed: bool },
    Row(Sensor),
}

fn draw_column_header(ui: &mut egui::Ui, col_w: f32, pal: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(col_w, 18.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 0.0, pal.bg_header);
    p.text(Pos2::new(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, "Sensor", FontId::proportional(11.0), pal.text_dim);
    let val_area = (col_w - 150.0).max(120.0);
    let col_w_step = val_area / 4.0;
    p.text(Pos2::new(rect.left() + 150.0 + col_w_step * 1.0 - 4.0, rect.center().y), Align2::RIGHT_CENTER, "Current", FontId::proportional(11.0), pal.text_dim);
    p.text(Pos2::new(rect.left() + 150.0 + col_w_step * 2.0 - 4.0, rect.center().y), Align2::RIGHT_CENTER, "Minimum", FontId::proportional(11.0), pal.text_dim);
    p.text(Pos2::new(rect.left() + 150.0 + col_w_step * 3.0 - 4.0, rect.center().y), Align2::RIGHT_CENTER, "Maximum", FontId::proportional(11.0), pal.text_dim);
    p.text(Pos2::new(rect.left() + 150.0 + col_w_step * 4.0 - 4.0, rect.center().y), Align2::RIGHT_CENTER, "Average", FontId::proportional(11.0), pal.text_dim);
}

fn draw_column(ui: &mut egui::Ui, items: &[Item], col_w: f32, s: &Shared, pal: &Palette) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        draw_column_header(ui, col_w, pal);
        let mut stripe = 0usize;
        for item in items {
            match item {
                Item::Header { title, id, hw_type, collapsed } => {
                    if let Some(new_state) = widgets::group_header(ui, title, *hw_type, *collapsed, col_w, pal) {
                        if let Ok(mut st) = s.settings.write() {
                            if new_state {
                                st.collapsed_groups.insert(id.clone());
                            } else {
                                st.collapsed_groups.remove(id);
                            }
                            st.save();
                        }
                    }
                    stripe = 0;
                }
                Item::Row(sensor) => {
                    draw_row(ui, sensor, col_w, stripe, s, pal);
                    stripe += 1;
                }
            }
        }
    });
}

fn draw_row(ui: &mut egui::Ui, sensor: &Sensor, col_w: f32, stripe: usize, s: &Shared, pal: &Palette) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(col_w, ROW_H), Sense::click());
    let p = ui.painter();
    let bg = if stripe.is_multiple_of(2) { pal.row_even } else { pal.row_odd };
    p.rect_filled(rect, 0.0, bg);

    // Type icon.
    let icon_center = Pos2::new(rect.left() + 10.0, rect.center().y);
    paint_icon(p, icon_center, sensor.sensor_type, pal);

    // Sensor name left-aligned.
    let name_max_w = 140.0;
    p.text(
        Pos2::new(rect.left() + 20.0, rect.center().y),
        Align2::LEFT_CENTER,
        truncate(&sensor.name, (name_max_w / 5.8) as usize),
        FontId::proportional(11.0),
        pal.text,
    );

    // 4 Columns: Current | Minimum | Maximum | Average
    let val_area = (col_w - 150.0).max(120.0);
    let col_w_step = val_area / 4.0;
    let value_color = value_severity_color(sensor, pal);

    let val_cur = widgets::format_value(sensor.value, sensor.sensor_type);
    let val_min = widgets::format_value(sensor.min, sensor.sensor_type);
    let val_max = widgets::format_value(sensor.max, sensor.sensor_type);
    let val_avg = widgets::format_value(sensor.avg, sensor.sensor_type);

    p.text(
        Pos2::new(rect.left() + 150.0 + col_w_step * 1.0 - 4.0, rect.center().y),
        Align2::RIGHT_CENTER,
        &val_cur,
        FontId::monospace(10.0),
        value_color,
    );
    p.text(
        Pos2::new(rect.left() + 150.0 + col_w_step * 2.0 - 4.0, rect.center().y),
        Align2::RIGHT_CENTER,
        &val_min,
        FontId::monospace(10.0),
        pal.text_dim,
    );
    p.text(
        Pos2::new(rect.left() + 150.0 + col_w_step * 3.0 - 4.0, rect.center().y),
        Align2::RIGHT_CENTER,
        &val_max,
        FontId::monospace(10.0),
        pal.text_dim,
    );
    p.text(
        Pos2::new(rect.left() + 150.0 + col_w_step * 4.0 - 4.0, rect.center().y),
        Align2::RIGHT_CENTER,
        &val_avg,
        FontId::monospace(10.0),
        pal.text_dim,
    );

    let graphed = s
        .graphs
        .read()
        .map(|g| g.contains(&sensor.identifier))
        .unwrap_or(false);

    resp.clone().on_hover_ui(|ui| {
        ui.label(RichText::new(&sensor.name).strong());
        ui.label(format!(
            "Current: {}   Min: {}   Max: {}   Avg: {}",
            val_cur, val_min, val_max, val_avg
        ));
        ui.label(RichText::new("Right-click to toggle graph").italics().weak());
    });

    resp.context_menu(|ui| {
        let label = if graphed { "Hide Graph" } else { "Show Graph" };
        if ui.button(label).clicked() {
            toggle_graph(s, &sensor.identifier, graphed);
        }
    });
    if resp.clicked() {
        toggle_graph(s, &sensor.identifier, graphed);
    }
}

pub fn show(ui: &mut egui::Ui, s: &Shared) {
    super::handle_close(ui, &s.windows.sensors);
    let pal = s.palette();

    let frame = s.frame();
    let tree = &frame.tree;

    // Diagnostic banner for the "0 W / 0 MHz" case.
    let warmed_up = s.started.elapsed().as_secs() >= 4;
    let banner = if s.elevated == Some(false) {
        Some(Banner::NotElevated)
    } else if warmed_up && driver_appears_blocked(tree) {
        Some(Banner::DriverBlocked)
    } else {
        None
    };
    if let Some(banner) = banner {
        egui::Panel::top("sensor_warn")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(0x5a, 0x3a, 0x10))
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ui, |ui| match banner {
                Banner::NotElevated => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(
                                "⚠ Not running as Administrator — CPU power, effective clocks, \
                                 Tctl/Tdie, per-core temperatures and fan/voltage sensors will read \
                                 0 or be missing. Relaunch via right-click → Run as administrator \
                                 (the release build does this automatically).",
                            )
                            .color(egui::Color32::from_rgb(0xff, 0xd8, 0x80))
                            .size(11.0),
                        );
                    });
                }
                Banner::DriverBlocked => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(
                                "⚠ CPU clocks/power read 0 — the kernel driver isn't loading. \
                                 Windows' vulnerable-driver blocklist blocks WinRing0. Install \
                                 PawnIO (a signed, blocklist-clean driver the sensor engine can \
                                 use) and relaunch:",
                            )
                            .color(egui::Color32::from_rgb(0xff, 0xd8, 0x80))
                            .size(11.0),
                        );
                        ui.hyperlink_to(
                            RichText::new("pawnio.eu").size(11.0),
                            "https://pawnio.eu/",
                        );
                    });
                }
            });
    }

    // Own CPU load → window title, like HWiNFO's "(0.9%)".
    let cpu_load = find_cpu_load(tree);
    let title = match cpu_load {
        Some(l) => format!("SensorView Sensors Status ({l:.1}%)"),
        None => "SensorView Sensors Status".to_string(),
    };
    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(title));

    // ---- Bottom toolbar --------------------------------------------------
    egui::Panel::bottom("sensors_toolbar")
        .frame(
            egui::Frame::new()
                .fill(pal.bg_header)
                .inner_margin(egui::Margin::symmetric(6, 3)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Uptime:").size(11.0).color(pal.text_dim));
                ui.label(RichText::new(s.uptime_text()).color(pal.text_dim).size(11.0));

                // Logging status text.
                let (logging, rows, path) = s
                    .logger
                    .lock()
                    .map(|l| {
                        l.as_ref()
                            .map(|lg| (true, lg.rows(), lg.path().display().to_string()))
                            .unwrap_or((false, 0, String::new()))
                    })
                    .unwrap_or((false, 0, String::new()));
                if logging {
                    ui.label(
                        RichText::new(format!("● REC {rows} rows"))
                            .color(pal.crit)
                            .size(11.0),
                    )
                    .on_hover_text(path);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(RichText::new("Close").color(pal.accent).size(11.0))
                        .on_hover_text("Close Sensors Window")
                        .clicked()
                    {
                        WindowFlags::close(&s.windows.sensors);
                    }
                    if ui
                        .button(RichText::new("Settings").size(11.0))
                        .on_hover_text("Settings")
                        .clicked()
                    {
                        WindowFlags::open(&s.windows.settings);
                    }
                    // CSV logging toggle.
                    let log_label = if logging { "Stop Logging" } else { "Start Logging" };
                    let log_col = if logging { pal.crit } else { pal.text };
                    if ui.button(RichText::new(log_label).color(log_col).size(11.0)).clicked() {
                        if let Ok(mut slot) = s.logger.lock() {
                            if slot.is_some() {
                                *slot = None; // stop
                            } else if let Ok(l) = crate::logging::CsvLogger::start(tree) {
                                *slot = Some(l);
                            }
                        }
                    }
                    if ui
                        .button(RichText::new("Reset Min/Max").size(11.0))
                        .clicked()
                    {
                        // Sent to the poll thread rather than applied under a
                        // shared lock — the UI never blocks on the poller.
                        s.command(crate::poll::Command::ResetMinMax);
                    }
                });
            });
        });

    // ---- Flow columns ----------------------------------------------------
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(pal.bg))
        .show(ui, |ui| {
            let items = build_items(tree, s);
            flow_columns(ui, &items, s, &pal);
        });
}

enum Banner {
    NotElevated,
    DriverBlocked,
}

/// Heuristic: the driver is (probably) blocked if the CPU exposes clock/power
/// sensors but they're all zero — that's the WinRing0-blocked signature.
fn driver_appears_blocked(tree: &[Hardware]) -> bool {
    let mut saw_driver_sensor = false;
    let mut all_zero = true;
    for hw in tree {
        if hw.hardware_type == HardwareType::Cpu {
            for s in &hw.sensors {
                if matches!(s.sensor_type, SensorType::Clock | SensorType::Power) {
                    saw_driver_sensor = true;
                    if s.value.unwrap_or(0.0).abs() > 0.01 {
                        all_zero = false;
                    }
                }
            }
        }
    }
    saw_driver_sensor && all_zero
}

fn find_cpu_load(tree: &[Hardware]) -> Option<f32> {
    for hw in tree {
        if hw.hardware_type == HardwareType::Cpu {
            for sensor in &hw.sensors {
                if sensor.sensor_type == SensorType::Load
                    && sensor.name.to_lowercase().contains("total")
                {
                    return sensor.value;
                }
            }
        }
    }
    None
}

fn group_title(hw: &Hardware) -> String {
    let prefix = match hw.hardware_type {
        HardwareType::Cpu => "CPU: ",
        HardwareType::GpuNvidia | HardwareType::GpuAti | HardwareType::GpuIntel => "GPU: ",
        HardwareType::Ram => "",
        HardwareType::Storage | HardwareType::Hdd => "Drive: ",
        HardwareType::Network => "Network: ",
        HardwareType::Battery => "Battery: ",
        _ => "",
    };
    format!("{prefix}{}", hw.name)
}

fn build_items(tree: &[Hardware], s: &Shared) -> Vec<Item> {
    let collapsed_set = s
        .settings
        .read()
        .map(|st| st.collapsed_groups.clone())
        .unwrap_or_default();
    let mut items = Vec::new();
    fn add(hw: &Hardware, items: &mut Vec<Item>, collapsed_set: &std::collections::BTreeSet<String>) {
        if hw.sensors.is_empty() && hw.sub_hardware.is_empty() {
            return;
        }
        let collapsed = collapsed_set.contains(&hw.identifier);
        if !hw.sensors.is_empty() {
            items.push(Item::Header {
                title: group_title(hw),
                id: hw.identifier.clone(),
                hw_type: hw.hardware_type,
                collapsed,
            });
            if !collapsed {
                for sensor in &hw.sensors {
                    items.push(Item::Row(sensor.clone()));
                }
            }
        }
        if !collapsed {
            for sub in &hw.sub_hardware {
                add(sub, items, collapsed_set);
            }
        }
    }
    for hw in tree {
        add(hw, &mut items, &collapsed_set);
    }
    items
}

fn flow_columns(ui: &mut egui::Ui, items: &[Item], s: &Shared, pal: &Palette) {
    let avail = ui.available_size();
    let n_cols = ((avail.x / COL_MIN_W).floor() as usize).max(1);
    let col_w = (avail.x / n_cols as f32).floor();
    let rows_per_col = ((avail.y - 4.0) / ROW_H).floor().max(4.0) as usize;

    // Overflow beyond the visible columns scrolls horizontally, like HWiNFO.
    egui::ScrollArea::horizontal().show(ui, |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            let mut idx = 0usize;
            let mut col = 0usize;
            while idx < items.len() {
                let end = (idx + rows_per_col).min(items.len());
                draw_column(ui, &items[idx..end], col_w, s, pal);
                // Column separator.
                let sep_rect = ui.allocate_exact_size(Vec2::new(1.0, avail.y), Sense::hover()).0;
                ui.painter().rect_filled(sep_rect, 0.0, pal.grid);
                idx = end;
                col += 1;
                let _ = col;
            }
        });
    });
}



fn toggle_graph(s: &Shared, identifier: &str, currently_open: bool) {
    if let Ok(mut set) = s.graphs.write() {
        if currently_open {
            set.remove(identifier);
        } else {
            set.insert(identifier.to_string());
        }
    }
}

/// Warning/critical coloring: temps ≥ 80/90 °C, loads ≥ 95 %.
fn value_severity_color(sensor: &Sensor, pal: &Palette) -> egui::Color32 {
    if let Some(v) = sensor.value {
        match sensor.sensor_type {
            SensorType::Temperature if v >= 90.0 => return pal.crit,
            SensorType::Temperature if v >= 80.0 => return pal.warn,
            SensorType::Load if v >= 95.0 => return pal.warn,
            _ => {}
        }
    }
    pal.value
}

fn paint_icon(p: &egui::Painter, c: Pos2, t: SensorType, pal: &Palette) {
    let col = widgets::type_color(t, pal);
    match t {
        SensorType::Voltage | SensorType::Current | SensorType::Power | SensorType::Energy => {
            let pts = vec![
                Pos2::new(c.x + 1.5, c.y - 5.0),
                Pos2::new(c.x - 2.5, c.y + 0.5),
                Pos2::new(c.x - 0.5, c.y + 0.5),
                Pos2::new(c.x - 1.5, c.y + 5.0),
                Pos2::new(c.x + 2.5, c.y - 0.5),
                Pos2::new(c.x + 0.5, c.y - 0.5),
            ];
            p.add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
        }
        SensorType::Clock | SensorType::Frequency | SensorType::TimeSpan => {
            p.circle_stroke(c, 4.0, Stroke::new(1.1, col));
            p.line_segment([c, Pos2::new(c.x, c.y - 2.6)], Stroke::new(0.9, col));
            p.line_segment([c, Pos2::new(c.x + 1.9, c.y + 0.7)], Stroke::new(0.9, col));
        }
        SensorType::Temperature => {
            p.line_segment([Pos2::new(c.x, c.y - 4.5), Pos2::new(c.x, c.y + 1.0)], Stroke::new(1.8, col));
            p.circle_filled(Pos2::new(c.x, c.y + 2.8), 2.2, col);
        }
        SensorType::Fan | SensorType::Flow | SensorType::Control => {
            p.circle_filled(c, 1.2, col);
            for a in [0.0f32, 2.094, 4.189] {
                let dir = Vec2::new(a.cos(), a.sin());
                p.circle_filled(c + dir * 2.8, 1.6, col);
            }
        }
        SensorType::Load | SensorType::Level | SensorType::Factor => {
            let r = egui::Rect::from_center_size(c, Vec2::splat(7.0));
            p.rect_stroke(r, 1.0, Stroke::new(1.1, col), egui::StrokeKind::Inside);
        }
        _ => {
            p.circle_filled(c, 1.8, col);
        }
    }
}

fn truncate(sensor: &str, max_chars: usize) -> String {
    if sensor.chars().count() <= max_chars.max(8) {
        sensor.to_string()
    } else {
        let cut: String = sensor.chars().take(max_chars.max(8).saturating_sub(1)).collect();
        format!("{cut}…")
    }
}
