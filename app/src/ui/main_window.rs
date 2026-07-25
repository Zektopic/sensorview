//! HWiNFO-style main window: big-icon toolbar (Summary / Save Report /
//! Sensors / Memory / About), device tree on the left, "Feature" detail pane
//! on the right, machine-name status bar.

use eframe::egui::{self, Align2, FontId, RichText};

use super::widgets::{badge, info_row};
use super::{Palette, Shared, WindowFlags};

#[derive(Default)]
pub struct MainWindowState {
    pub selected: Selection,
    pub show_about: bool,
    pub last_report: Option<String>,
}

#[derive(Default, Clone, PartialEq)]
pub enum Selection {
    #[default]
    Computer,
    Cpu,
    Motherboard,
    Memory,
    Video,
    Drives,
    Network,
}

pub fn show(ui: &mut egui::Ui, s: &Shared, state: &mut MainWindowState) {
    let pal = s.palette();

    // ---- Toolbar ---------------------------------------------------------
    egui::Panel::top("main_toolbar")
        .frame(
            egui::Frame::new()
                .fill(pal.bg_header)
                .inner_margin(egui::Margin::symmetric(8, 6)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if tool_button(ui, ToolIcon::Summary, "Summary", &pal) {
                    WindowFlags::open(&s.windows.summary);
                }
                if tool_button(ui, ToolIcon::Save, "Save Report", &pal) {
                    let frame = s.frame();
                    let info = s.sysinfo.read().ok().and_then(|i| i.clone());
                    match crate::report::write_report(&frame.tree, info.as_ref()) {
                        Ok(p) => state.last_report = Some(format!("Report saved: {}", p.display())),
                        Err(e) => state.last_report = Some(format!("Report failed: {e}")),
                    }
                }
                if tool_button(ui, ToolIcon::Sensors, "Sensors", &pal) {
                    WindowFlags::open(&s.windows.sensors);
                }
                if tool_button(ui, ToolIcon::Memory, "Memory", &pal) {
                    state.selected = Selection::Memory;
                }
                if tool_button(ui, ToolIcon::Hex, "Hex", &pal) {
                    WindowFlags::open(&s.windows.hex);
                }
                if tool_button(ui, ToolIcon::About, "About", &pal) {
                    state.show_about = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(RichText::new("Settings").size(11.0)).clicked() {
                        WindowFlags::open(&s.windows.settings);
                    }
                });
            });
        });

    // ---- Status bar ------------------------------------------------------
    let info = s.sysinfo.read().ok().and_then(|i| i.clone());
    egui::Panel::bottom("main_status")
        .frame(
            egui::Frame::new()
                .fill(pal.bg_header)
                .inner_margin(egui::Margin::symmetric(8, 3)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let name = info.as_ref().map(|i| i.computer_name.clone()).unwrap_or_default();
                ui.label(RichText::new(name).color(pal.text_dim).size(11.0));
                if let Some(msg) = &state.last_report {
                    ui.separator();
                    ui.label(RichText::new(msg).color(pal.text_dim).size(11.0));
                }
                ui.separator();
                ui.label(
                    RichText::new(format!("{} sensors", s.frame().sensor_count()))
                        .color(pal.text_dim)
                        .size(11.0),
                );
                // Make the dashboard discoverable without digging through
                // Settings — it's the whole point of the LAN tier.
                // A failed bind (port already in use) must not be silent — the
                // dashboard would simply never appear.
                #[cfg(feature = "web")]
                if let Some(err) = &s.web.error {
                    ui.separator();
                    ui.label(RichText::new("🌐 Dashboard unavailable").color(pal.warn).size(11.0))
                        .on_hover_text(format!(
                            "{err}

Change the port in Settings → Remote Access,                              or set SENSORVIEW_WEB_PORT."
                        ));
                }
                #[cfg(feature = "web")]
                if let Some(url) = &s.web.url {
                    ui.separator();
                    ui.hyperlink_to(RichText::new("🌐 Dashboard").size(11.0), url)
                        .on_hover_text(url);
                    let clients = s.store.subscriber_count();
                    if clients > 0 {
                        ui.label(
                            RichText::new(format!("({clients} connected)"))
                                .color(pal.ok_badge)
                                .size(11.0),
                        );
                    }
                }
            });
        });

    // ---- Tree ------------------------------------------------------------
    egui::Panel::left("main_tree")
        .frame(
            egui::Frame::new()
                .fill(pal.bg_panel)
                .inner_margin(egui::Margin::same(4)),
        )
        .show(ui, |ui| {
            ui.take_available_space();
            ui.set_min_width(230.0);
            let computer = info
                .as_ref()
                .map(|i| i.computer_name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "Computer".into());
            use crate::model::HardwareType as H;
            tree_node(ui, &computer, Selection::Computer, None, state, &pal);
            ui.indent("tree_indent", |ui| {
                tree_node(ui, "Central Processor(s)", Selection::Cpu, Some(H::Cpu), state, &pal);
                tree_node(ui, "Motherboard", Selection::Motherboard, Some(H::Mainboard), state, &pal);
                tree_node(ui, "Memory", Selection::Memory, Some(H::Ram), state, &pal);
                tree_node(ui, "Video Adapter", Selection::Video, Some(H::GpuNvidia), state, &pal);
                tree_node(ui, "Drives", Selection::Drives, Some(H::Storage), state, &pal);
                tree_node(ui, "Network", Selection::Network, Some(H::Network), state, &pal);
            });
        });

    // ---- Feature pane ----------------------------------------------------
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(pal.bg)
                .inner_margin(egui::Margin::same(8)),
        )
        .show(ui, |ui| {
            ui.label(RichText::new("Feature").color(pal.text_dim).size(10.5));
            ui.separator();
            feature_pane(ui, s, state, info.as_ref(), &pal);
        });

    // ---- About dialog ----------------------------------------------------
    if state.show_about {
        egui::Window::new("About SensorView")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label(RichText::new("SensorView").color(pal.accent).size(16.0).strong());
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.label("A native HWiNFO-style hardware monitor written in Rust.");
                ui.label("Sensor engine: LibreHardwareMonitor bridge (migrating to pure Rust).");
                ui.add_space(6.0);
                if ui.button("Close").clicked() {
                    state.show_about = false;
                }
            });
    }
}

#[derive(Copy, Clone)]
enum ToolIcon {
    Summary,
    Save,
    Sensors,
    Memory,
    Hex,
    About,
}

fn tool_button(ui: &mut egui::Ui, icon: ToolIcon, label: &str, pal: &Palette) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(60.0, 36.0), egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, pal.row_odd);
    }
    let p = ui.painter();
    let c = egui::pos2(rect.center().x, rect.top() + 11.0);
    match icon {
        ToolIcon::Summary => {
            let r = egui::Rect::from_center_size(c, egui::vec2(12.0, 9.0));
            p.rect_stroke(r, 1.0, egui::Stroke::new(1.2, pal.accent), egui::StrokeKind::Inside);
        }
        ToolIcon::Save => {
            let r = egui::Rect::from_center_size(c, egui::vec2(10.0, 10.0));
            p.rect_stroke(r, 1.0, egui::Stroke::new(1.2, pal.ok_badge), egui::StrokeKind::Inside);
        }
        ToolIcon::Sensors => {
            p.circle_stroke(c, 4.5, egui::Stroke::new(1.2, pal.tempc));
            p.circle_filled(c, 1.5, pal.tempc);
        }
        ToolIcon::Memory => {
            let r = egui::Rect::from_center_size(c, egui::vec2(12.0, 7.0));
            p.rect_stroke(r, 0.0, egui::Stroke::new(1.2, pal.clockc), egui::StrokeKind::Inside);
        }
        ToolIcon::Hex => {
            p.text(c, Align2::CENTER_CENTER, "0x", FontId::monospace(10.0), pal.volt);
        }
        ToolIcon::About => {
            p.circle_stroke(c, 5.0, egui::Stroke::new(1.2, pal.accent));
            p.text(c, Align2::CENTER_CENTER, "i", FontId::proportional(9.0), pal.accent);
        }
    }
    p.text(
        egui::pos2(rect.center().x, rect.bottom() - 6.0),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(10.5),
        pal.text,
    );
    resp.clicked()
}

fn tree_node(
    ui: &mut egui::Ui,
    label: &str,
    sel: Selection,
    icon: Option<crate::model::HardwareType>,
    state: &mut MainWindowState,
    pal: &Palette,
) {
    let selected = state.selected == sel;
    let resp = ui
        .horizontal(|ui| {
            ui.add_space(2.0);
            match icon {
                Some(t) => super::widgets::hardware_icon_inline(ui, t, pal),
                None => paint_computer_icon(ui, pal), // root = the machine
            }
            let text = RichText::new(label)
                .size(11.5)
                .strong()
                .color(if selected { pal.accent } else { pal.text });
            ui.selectable_label(selected, text)
        })
        .inner;
    if resp.clicked() {
        state.selected = sel;
    }
}

/// Small monitor glyph for the root computer node.
fn paint_computer_icon(ui: &mut egui::Ui, pal: &Palette) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(15.0, 15.0), egui::Sense::hover());
    let p = ui.painter();
    let screen = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.center().y - 1.0),
        egui::vec2(11.0, 8.0),
    );
    p.rect_stroke(screen, 1.0, egui::Stroke::new(1.2, pal.accent), egui::StrokeKind::Inside);
    p.line_segment(
        [
            egui::pos2(rect.center().x - 2.5, rect.bottom() - 2.0),
            egui::pos2(rect.center().x + 2.5, rect.bottom() - 2.0),
        ],
        egui::Stroke::new(1.4, pal.accent),
    );
}

fn feature_pane(
    ui: &mut egui::Ui,
    s: &Shared,
    state: &MainWindowState,
    info: Option<&crate::sysinfo::SystemInfo>,
    pal: &Palette,
) {
    let Some(i) = info else {
        ui.label(RichText::new("Enumerating system…").color(pal.text_dim));
        return;
    };
    match state.selected {
        Selection::Computer => {
            ui.label(RichText::new("Current Computer").color(pal.accent).strong().size(12.0));
            info_row(ui, "Computer Name:", &i.computer_name, pal);
            info_row(ui, "Current User Name:", &i.user_name, pal);
            ui.add_space(8.0);
            ui.label(RichText::new("Operating System").color(pal.accent).strong().size(12.0));
            info_row(ui, "Operating System:", &i.os.caption, pal);
            info_row(ui, "Build:", &i.os.build, pal);
            info_row(ui, "Architecture:", &i.os.arch, pal);
            badge(ui, "UEFI Boot:", i.os.uefi_boot, pal);
            badge(ui, "Secure Boot:", i.os.secure_boot, pal);
        }
        Selection::Cpu => {
            ui.label(RichText::new("Central Processor(s)").color(pal.accent).strong().size(12.0));
            info_row(ui, "Processor:", &i.cpu.name, pal);
            info_row(ui, "Cores:", &opt_u32(i.cpu.cores), pal);
            info_row(ui, "Threads:", &opt_u32(i.cpu.threads), pal);
            info_row(ui, "Max Clock:", &i.cpu.max_clock_mhz.map(|c| format!("{c} MHz")).unwrap_or_default(), pal);
            info_row(ui, "L2 Cache:", &i.cpu.l2_kb.map(|k| format!("{k} KB")).unwrap_or_default(), pal);
            info_row(ui, "L3 Cache:", &i.cpu.l3_kb.map(|k| format!("{k} KB")).unwrap_or_default(), pal);
            info_row(ui, "Socket:", i.cpu.socket.as_deref().unwrap_or(""), pal);
        }
        Selection::Motherboard => {
            ui.label(RichText::new("Motherboard").color(pal.accent).strong().size(12.0));
            info_row(ui, "Manufacturer:", &i.board.manufacturer, pal);
            info_row(ui, "Model:", &i.board.product, pal);
            info_row(ui, "BIOS Version:", &i.board.bios_version, pal);
            info_row(ui, "BIOS Date:", &i.board.bios_date, pal);
        }
        Selection::Memory => {
            ui.label(RichText::new("Memory").color(pal.accent).strong().size(12.0));
            info_row(
                ui,
                "Total Size:",
                &i.total_memory_gb.map(|g| format!("{g:.0} GB")).unwrap_or_default(),
                pal,
            );
            for m in &i.memory_modules {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("• {}", m.bank)).color(pal.text).size(11.0));
                info_row(ui, "  Part Number:", &m.part_number, pal);
                info_row(ui, "  Size:", &format!("{:.0} GB {}", m.capacity_gb, m.memory_type), pal);
                info_row(
                    ui,
                    "  Speed:",
                    &m.configured_speed_mts
                        .or(m.speed_mts)
                        .map(|v| format!("{v} MT/s"))
                        .unwrap_or_default(),
                    pal,
                );
            }
        }
        Selection::Video => {
            ui.label(RichText::new("Video Adapter").color(pal.accent).strong().size(12.0));
            for g in &i.gpus {
                info_row(ui, "GPU:", &g.name, pal);
                info_row(
                    ui,
                    "  Driver:",
                    &g.driver_version,
                    pal,
                );
            }
        }
        Selection::Drives => {
            ui.label(RichText::new("Drives").color(pal.accent).strong().size(12.0));
            for d in &i.drives {
                info_row(
                    ui,
                    &format!("{} ({}):", d.model, d.interface),
                    &d.size_gb.map(|g| format!("{g:.0} GB")).unwrap_or_default(),
                    pal,
                );
            }
        }
        Selection::Network => {
            ui.label(RichText::new("Network").color(pal.accent).strong().size(12.0));
            let frame = s.frame();
            for hw in &frame.tree {
                if hw.hardware_type == crate::model::HardwareType::Network {
                    info_row(ui, "Adapter:", &hw.name, pal);
                }
            }
        }
    }
}

fn opt_u32(v: Option<u32>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}
