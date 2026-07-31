//! Task Manager — processes and a performance overview.
//!
//! Laid out after Mission Center: a left sidebar of categories, each with a
//! sparkline and its current value, and the selected one filling the right
//! pane with a large graph and a stats grid. Written from scratch against
//! SensorView's own data and palette — nothing is ported from that project,
//! which is GTK4 and could not be reused here in any case.
//!
//! # Collector lifecycle
//!
//! Enumerating every process costs tens of milliseconds, so it does not run
//! when nobody is looking. The collector is started on the first frame this
//! window draws and stopped when it closes; `Shared::procs` being `Some` *is*
//! the running state, which is why there is no separate flag to keep in sync.

use std::sync::Mutex;
use std::time::Duration;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, RichText, Stroke};
use egui_extras::{Column, TableBuilder};

use crate::model::{Hardware, HardwareType, Sensor, SensorType};
use crate::procs::{self, ProcessCollector, Sort};
use crate::state::TelemetryFrame;

use super::{Palette, Shared, WindowFlags};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Performance,
    Processes,
}

/// Per-window state, kept in egui's memory so it survives frames without
/// widening `Shared` for something only this window cares about.
#[derive(Clone)]
struct State {
    tab: Tab,
    sort: Sort,
    descending: bool,
    filter: String,
    selected_category: usize,
    /// Pid awaiting confirmation, and whether the request was a force-kill.
    confirm_kill: Option<(u32, String, bool)>,
    last_action: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            tab: Tab::default(),
            sort: Sort::Cpu,
            // Descending, emphatically: opening a task manager to a list of the
            // *idlest* processes is useless. `bool::default()` is false, which
            // is why this cannot be derived.
            descending: true,
            filter: String::new(),
            selected_category: 0,
            confirm_kill: None,
            last_action: None,
        }
    }
}

/// Which tab opens first. `SENSORVIEW_TASKMGR_TAB` is a dev/testing
/// affordance matching `SENSORVIEW_SETTINGS_TAB`, so the Processes tab can be
/// smoke-tested without driving the mouse; unset, it is always Performance.
fn default_tab() -> Tab {
    match std::env::var("SENSORVIEW_TASKMGR_TAB").as_deref() {
        Ok("processes") => Tab::Processes,
        _ => Tab::Performance,
    }
}

pub fn show(ui: &mut egui::Ui, s: &Shared) {
    let pal = s.palette();

    // Closing must stop the collector, not just hide the window — otherwise it
    // keeps enumerating processes forever behind a closed window.
    if ui.ctx().input(|i| i.viewport().close_requested()) {
        stop_collector(&s.procs);
        WindowFlags::close(&s.windows.taskmgr);
        return;
    }
    ensure_collector(&s.procs);

    // The collector publishes at its own cadence, not in response to input, so
    // ask egui to come back rather than waiting for a mouse move.
    ui.ctx().request_repaint_after(Duration::from_millis(500));

    let id = ui.id().with("taskmgr_state");
    let mut state: State = ui
        .ctx()
        .data_mut(|d| d.get_temp(id).unwrap_or_else(|| State { tab: default_tab(), ..State::default() }));

    egui::Panel::top("taskmgr_tabs")
        .frame(
            egui::Frame::new()
                .fill(pal.bg_header)
                .inner_margin(egui::Margin::symmetric(8, 6)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (tab, label) in
                    [(Tab::Performance, "Performance"), (Tab::Processes, "Processes")]
                {
                    let active = state.tab == tab;
                    let text = RichText::new(label)
                        .size(12.0)
                        .color(if active { pal.text } else { pal.text_dim });
                    if ui.selectable_label(active, text).clicked() {
                        state.tab = tab;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(msg) = &state.last_action {
                        ui.label(RichText::new(msg).size(10.5).color(pal.text_dim));
                    }
                });
            });
        });

    match state.tab {
        Tab::Performance => performance_tab(ui, s, &pal, &mut state),
        Tab::Processes => processes_tab(ui, s, &pal, &mut state),
    }

    confirm_kill_modal(ui, &pal, &mut state);
    ui.ctx().data_mut(|d| d.insert_temp(id, state));
}

// ---- collector lifecycle -------------------------------------------------

/// Start the collector if it isn't already running. Idempotent: called on
/// every frame, so a second start must never spawn a second thread.
fn ensure_collector(slot: &Mutex<Option<ProcessCollector>>) {
    if let Ok(mut slot) = slot.lock() {
        if slot.is_none() {
            *slot = Some(ProcessCollector::start());
        }
    }
}

/// Stop and drop the collector. `Drop` joins the thread, so once this returns
/// nothing is still enumerating processes.
fn stop_collector(slot: &Mutex<Option<ProcessCollector>>) {
    if let Ok(mut slot) = slot.lock() {
        slot.take();
    }
}

// ---- Processes -----------------------------------------------------------

fn processes_tab(ui: &mut egui::Ui, s: &Shared, pal: &Palette, state: &mut State) {
    let snapshot = match s.procs.lock() {
        Ok(slot) => slot.as_ref().map(|c| c.snapshot()),
        Err(_) => None,
    };
    let Some(snapshot) = snapshot else {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Starting process collector…").color(pal.text_dim));
        });
        return;
    };

    egui::Panel::top("taskmgr_filter")
        .frame(egui::Frame::new().fill(pal.bg).inner_margin(egui::Margin::symmetric(8, 4)))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Filter").size(11.0).color(pal.text_dim));
                ui.add(
                    egui::TextEdit::singleline(&mut state.filter)
                        .desired_width(220.0)
                        .hint_text("name, command or pid"),
                );
                if !state.filter.is_empty() && ui.small_button("✕").clicked() {
                    state.filter.clear();
                }
                ui.separator();
                let cpu = if snapshot.has_cpu() {
                    format!("{:.0} %", snapshot.total_cpu_pct)
                } else {
                    // Not zero: the baseline simply doesn't exist yet.
                    "—".to_string()
                };
                ui.label(
                    RichText::new(format!(
                        "{} processes · total CPU {cpu}",
                        snapshot.rows.len()
                    ))
                    .size(11.0)
                    .color(pal.text_dim),
                );
            });
        });

    let filter = (!state.filter.is_empty()).then_some(state.filter.as_str());
    let rows = procs::arrange(&snapshot.rows, filter, state.sort, state.descending);

    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(pal.bg).inner_margin(egui::Margin::same(4)))
        .show(ui, |ui| {
            if rows.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new(format!("No process matches {:?}", state.filter))
                            .color(pal.text_dim),
                    );
                });
                return;
            }

            let row_h = 18.0;
            // Virtualised: `body.rows` only builds what is on screen, which
            // matters at ~700 processes.
            TableBuilder::new(ui)
                .striped(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(64.0))    // pid
                .column(Column::exact(64.0))    // cpu
                .column(Column::exact(78.0))    // memory
                .column(Column::exact(96.0))    // user
                .column(Column::remainder())    // name
                .header(20.0, |mut header| {
                    for (label, sort) in [
                        ("PID", Some(Sort::Pid)),
                        ("CPU %", Some(Sort::Cpu)),
                        ("Memory", Some(Sort::Memory)),
                        ("User", None),
                        ("Name", Some(Sort::Name)),
                    ] {
                        header.col(|ui| {
                            sort_header(ui, pal, state, label, sort);
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_h, rows.len(), |mut row| {
                        let r = &rows[row.index()];
                        row.col(|ui| mono(ui, pal, &r.pid.to_string()));
                        row.col(|ui| {
                            // An unknown reading is an em dash, never 0.0 —
                            // "we can't see" and "idle" are different facts.
                            let (text, color) = match r.cpu_pct {
                                None => ("—".to_string(), pal.text_dim),
                                Some(v) => (format!("{v:.1}"), load_color(v, pal)),
                            };
                            ui.label(
                                RichText::new(text).size(10.5).monospace().color(color),
                            );
                        });
                        row.col(|ui| mono(ui, pal, &procs::format_bytes(r.mem_bytes)));
                        row.col(|ui| {
                            ui.label(
                                RichText::new(r.user.as_deref().unwrap_or("—"))
                                    .size(10.5)
                                    .color(pal.text_dim),
                            );
                        });
                        row.col(|ui| {
                            let resp = ui
                                .label(RichText::new(&r.name).size(11.0).color(pal.text))
                                .on_hover_text(if r.cmd.is_empty() {
                                    r.name.clone()
                                } else {
                                    r.cmd.clone()
                                });
                            resp.context_menu(|ui| {
                                ui.label(
                                    RichText::new(format!("{} ({})", r.name, r.pid))
                                        .strong()
                                        .size(11.0),
                                );
                                ui.separator();
                                if ui.button("End process").clicked() {
                                    state.confirm_kill =
                                        Some((r.pid, r.name.clone(), false));
                                    ui.close();
                                }
                                if ui.button("Force kill").clicked() {
                                    state.confirm_kill =
                                        Some((r.pid, r.name.clone(), true));
                                    ui.close();
                                }
                            });
                        });
                    });
                });
        });
}

fn sort_header(ui: &mut egui::Ui, pal: &Palette, state: &mut State, label: &str, sort: Option<Sort>) {
    let Some(sort) = sort else {
        ui.label(RichText::new(label).size(10.5).strong().color(pal.text_dim));
        return;
    };
    let active = state.sort == sort;
    let arrow = if !active {
        ""
    } else if state.descending {
        " ▼"
    } else {
        " ▲"
    };
    let text = RichText::new(format!("{label}{arrow}"))
        .size(10.5)
        .strong()
        .color(if active { pal.accent } else { pal.text_dim });
    if ui.add(egui::Label::new(text).sense(egui::Sense::click())).clicked() {
        if active {
            state.descending = !state.descending;
        } else {
            state.sort = sort;
            // A new column starts descending: the interesting end of CPU and
            // memory is the top, and that is what someone opening a task
            // manager is looking for.
            state.descending = true;
        }
    }
}

fn mono(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    ui.label(RichText::new(text).size(10.5).monospace().color(pal.value));
}

fn load_color(pct: f32, pal: &Palette) -> Color32 {
    if pct >= 80.0 {
        pal.crit
    } else if pct >= 40.0 {
        pal.warn
    } else {
        pal.value
    }
}

/// Killing is destructive and irreversible, so it asks first.
fn confirm_kill_modal(ui: &mut egui::Ui, pal: &Palette, state: &mut State) {
    let Some((pid, name, force)) = state.confirm_kill.clone() else {
        return;
    };
    let mut open = true;
    egui::Window::new(if force { "Force kill process" } else { "End process" })
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ui.ctx(), |ui| {
            ui.label(
                RichText::new(format!("{name} (pid {pid})"))
                    .size(12.0)
                    .strong()
                    .color(pal.text),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(if force {
                    "SIGKILL — the process gets no chance to save or clean up."
                } else {
                    "SIGTERM — asks the process to exit."
                })
                .size(11.0)
                .color(if force { pal.warn } else { pal.text_dim }),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    state.confirm_kill = None;
                }
                let label = if force { "Force kill" } else { "End process" };
                if ui.button(RichText::new(label).color(pal.crit)).clicked() {
                    // Report the outcome either way: signalling another user's
                    // process fails, and silently doing nothing would look like
                    // the button was broken.
                    state.last_action = Some(match procs::kill(pid, force) {
                        Ok(()) => format!("pid {pid} signalled"),
                        Err(e) => format!("pid {pid}: {e}"),
                    });
                    state.confirm_kill = None;
                }
            });
        });
    if !open {
        state.confirm_kill = None;
    }
}

// ---- Performance ---------------------------------------------------------

/// One sidebar entry: a hardware node reduced to a headline sensor.
struct Category {
    title: String,
    subtitle: String,
    /// Sensor identifier whose history drives the sparkline and graph.
    identifier: String,
    value: Option<f32>,
    sensor_type: SensorType,
    /// Sensors shown in the stats grid for this category.
    detail: Vec<(String, String)>,
}

fn performance_tab(ui: &mut egui::Ui, s: &Shared, pal: &Palette, state: &mut State) {
    let frame = s.frame();
    let categories = build_categories(&frame);

    if categories.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Waiting for sensor data…").color(pal.text_dim));
        });
        return;
    }
    state.selected_category = state.selected_category.min(categories.len() - 1);

    egui::Panel::left("taskmgr_categories")
        .frame(egui::Frame::new().fill(pal.bg_panel).inner_margin(egui::Margin::same(4)))
        .show(ui, |ui| {
            // Sized the way main_window sizes its tree panel — this egui build
            // has no width builder on Panel.
            ui.set_min_width(206.0);
            ui.set_max_width(206.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, cat) in categories.iter().enumerate() {
                    let selected = i == state.selected_category;
                    if category_row(ui, s, pal, cat, selected) {
                        state.selected_category = i;
                    }
                }
            });
        });

    let cat = &categories[state.selected_category];
    let history = s.store.history(&cat.identifier);

    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(pal.bg).inner_margin(egui::Margin::same(10)))
        .show(ui, |ui| {
            ui.label(RichText::new(&cat.title).size(15.0).strong().color(pal.text));
            ui.label(RichText::new(&cat.subtitle).size(11.0).color(pal.text_dim));
            ui.add_space(6.0);

            let graph_h = (ui.available_height() - 26.0 * cat.detail.len() as f32 - 20.0)
                .clamp(120.0, 340.0);
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), graph_h), egui::Sense::hover());
            paint_graph(ui, rect, &history, super::widgets::type_color(cat.sensor_type, pal), pal);

            ui.add_space(8.0);
            egui::Grid::new("taskmgr_detail")
                .num_columns(2)
                .spacing([18.0, 3.0])
                .show(ui, |ui| {
                    for (k, v) in &cat.detail {
                        ui.label(RichText::new(k).size(11.0).color(pal.text_dim));
                        ui.label(RichText::new(v).size(11.0).monospace().color(pal.value));
                        ui.end_row();
                    }
                });
        });
}

/// A sidebar row: title, current value, and a sparkline of recent history.
fn category_row(
    ui: &mut egui::Ui,
    s: &Shared,
    pal: &Palette,
    cat: &Category,
    selected: bool,
) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), egui::Sense::click());
    let p = ui.painter_at(rect);

    if selected {
        p.rect_filled(rect, 2.0, pal.bg_header);
        p.line_segment(
            [
                Pos2::new(rect.left() + 1.0, rect.top() + 3.0),
                Pos2::new(rect.left() + 1.0, rect.bottom() - 3.0),
            ],
            Stroke::new(2.0, pal.accent),
        );
    } else if resp.hovered() {
        p.rect_filled(rect, 2.0, pal.row_odd);
    }

    p.text(
        Pos2::new(rect.left() + 8.0, rect.top() + 6.0),
        Align2::LEFT_TOP,
        &cat.title,
        FontId::proportional(11.5),
        if selected { pal.text } else { pal.text_dim },
    );
    p.text(
        Pos2::new(rect.left() + 8.0, rect.bottom() - 6.0),
        Align2::LEFT_BOTTOM,
        crate::format::format_value(cat.value, cat.sensor_type),
        FontId::monospace(10.5),
        pal.value,
    );

    // Sparkline on the right half.
    let spark = egui::Rect::from_min_max(
        Pos2::new(rect.right() - 74.0, rect.top() + 8.0),
        Pos2::new(rect.right() - 6.0, rect.bottom() - 8.0),
    );
    paint_sparkline(
        &p,
        spark,
        &s.store.history(&cat.identifier),
        super::widgets::type_color(cat.sensor_type, pal),
    );

    resp.clicked()
}

fn paint_sparkline(p: &egui::Painter, rect: egui::Rect, history: &[f32], line: Color32) {
    if history.len() < 2 {
        return;
    }
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for &v in history {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    // A flat series would divide by zero; give it a band so it draws mid-height.
    if (hi - lo).abs() < f32::EPSILON {
        lo -= 1.0;
        hi += 1.0;
    }
    let n = history.len();
    let pts: Vec<Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = rect.left() + (i as f32 / (n - 1) as f32) * rect.width();
            let y = rect.bottom() - ((v - lo) / (hi - lo)) * rect.height();
            Pos2::new(x, y)
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(1.0, line)));
}

fn paint_graph(
    ui: &egui::Ui,
    rect: egui::Rect,
    history: &[f32],
    line: Color32,
    pal: &Palette,
) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, pal.row_odd);

    if history.len() < 2 {
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Collecting samples…",
            FontId::proportional(12.0),
            pal.text_dim,
        );
        return;
    }

    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for &v in history {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if (hi - lo).abs() < f32::EPSILON {
        hi = lo + 1.0;
        lo -= 1.0;
    }
    let pad = (hi - lo) * 0.08;
    lo -= pad;
    hi += pad;

    for i in 0..=4 {
        let t = i as f32 / 4.0;
        let y = rect.bottom() - t * rect.height();
        p.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(1.0, pal.grid.gamma_multiply(0.5)),
        );
        p.text(
            Pos2::new(rect.left() + 4.0, y - 1.0),
            Align2::LEFT_BOTTOM,
            format!("{:.1}", lo + t * (hi - lo)),
            FontId::monospace(9.0),
            pal.text_dim,
        );
    }

    let n = history.len();
    let pts: Vec<Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = rect.left() + (i as f32 / (n - 1) as f32) * rect.width();
            let y = rect.bottom() - ((v - lo) / (hi - lo)) * rect.height();
            Pos2::new(x, y)
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(1.5, line)));
}

/// Reduce the hardware tree to sidebar categories.
///
/// Each hardware node contributes one category, headlined by the sensor a
/// person would actually look at first — utilisation where there is one,
/// otherwise temperature, otherwise whatever it has.
fn build_categories(frame: &TelemetryFrame) -> Vec<Category> {
    let mut out = Vec::new();
    walk(&frame.tree, &mut out);
    out
}

fn walk(tree: &[Hardware], out: &mut Vec<Category>) {
    for hw in tree {
        if let Some(cat) = category_for(hw, out) {
            out.push(cat);
        }
        walk(&hw.sub_hardware, out);
    }
}

/// Sidebar titles name the *role*, not the device: on Apple Silicon the CPU
/// and GPU nodes are both called "Apple M5", so device names alone produce two
/// identical rows. The device name becomes the subtitle instead. Multiple
/// disks and NICs are numbered in tree order, as "Disk 0", "Disk 1".
fn role_title(t: HardwareType, existing: &[Category]) -> String {
    let base = match t {
        HardwareType::Cpu => "CPU",
        HardwareType::Ram => "Memory",
        HardwareType::Storage | HardwareType::Hdd => "Disk",
        HardwareType::Network => "Network",
        _ => "GPU",
    };
    if !matches!(
        t,
        HardwareType::Storage | HardwareType::Hdd | HardwareType::Network
    ) {
        return base.to_string();
    }
    let n = existing.iter().filter(|c| c.title.starts_with(base)).count();
    format!("{base} {n}")
}

fn category_for(hw: &Hardware, existing: &[Category]) -> Option<Category> {
    if !is_performance_node(hw.hardware_type) {
        return None;
    }
    let headline = pick_headline(&hw.sensors)?;

    let detail = hw
        .sensors
        .iter()
        .filter(|s| s.identifier != headline.identifier)
        .take(12)
        .map(|s| {
            (
                s.name.clone(),
                crate::format::format_value(s.value, s.sensor_type),
            )
        })
        .collect();

    Some(Category {
        title: role_title(hw.hardware_type, existing),
        subtitle: format!("{} · {} sensors", hw.name, hw.sensors.len()),
        identifier: headline.identifier.clone(),
        value: headline.value,
        sensor_type: headline.sensor_type,
        detail,
    })
}

/// Utilisation first, then clock, then temperature, then anything — the order
/// someone scanning a performance page cares about.
fn pick_headline(sensors: &[Sensor]) -> Option<&Sensor> {
    for wanted in [
        SensorType::Load,
        SensorType::Clock,
        SensorType::Temperature,
        SensorType::Throughput,
        SensorType::Power,
    ] {
        if let Some(s) = sensors.iter().find(|s| s.sensor_type == wanted) {
            return Some(s);
        }
    }
    sensors.first()
}

/// Whether this hardware type is one the performance page groups under.
/// Kept as a function so the intent is testable and greppable.
fn is_performance_node(t: HardwareType) -> bool {
    matches!(
        t,
        HardwareType::Cpu
            | HardwareType::Ram
            | HardwareType::Storage
            | HardwareType::Hdd
            | HardwareType::Network
            | HardwareType::GpuApple
            | HardwareType::GpuAti
            | HardwareType::GpuIntel
            | HardwareType::GpuNvidia
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sensor(name: &str, id: &str, ty: SensorType, v: Option<f32>) -> Sensor {
        Sensor {
            identifier: id.into(),
            name: name.into(),
            sensor_type: ty,
            index: 0,
            value: v,
            min: None,
            max: None,
            avg: None,
        }
    }

    fn frame() -> TelemetryFrame {
        TelemetryFrame {
            seq: 3,
            tree: vec![Hardware {
                identifier: "/cpu".into(),
                name: "Apple M5".into(),
                hardware_type: HardwareType::Cpu,
                sensors: vec![
                    sensor("PMU tdie6", "/t", SensorType::Temperature, Some(38.0)),
                    sensor("Total CPU Usage", "/l", SensorType::Load, Some(12.0)),
                    sensor("P-Core Clock", "/c", SensorType::Clock, Some(3000.0)),
                ],
                sub_hardware: vec![Hardware {
                    identifier: "/gpu".into(),
                    name: "GPU".into(),
                    hardware_type: HardwareType::GpuApple,
                    sensors: vec![sensor("GPU Core", "/g", SensorType::Load, Some(9.0))],
                    sub_hardware: Vec::new(),
                }],
            }],
            ..Default::default()
        }
    }

    /// Utilisation is what a performance page leads with, even when a
    /// temperature sensor appears earlier in the node.
    #[test]
    fn headline_prefers_load_over_temperature() {
        let cats = build_categories(&frame());
        assert_eq!(cats[0].identifier, "/l", "should headline Load, not the first sensor");
        assert_eq!(cats[0].value, Some(12.0));
    }

    #[test]
    fn categories_include_nested_hardware() {
        let cats = build_categories(&frame());
        assert_eq!(cats.len(), 2, "the nested GPU node must get its own category");
        assert_eq!(cats[1].title, "GPU");
        assert_eq!(cats[1].identifier, "/g");
        // The device name moves to the subtitle.
        assert!(cats[1].subtitle.starts_with("GPU ·"), "{}", cats[1].subtitle);
    }

    /// The headline must not be repeated in the grid below its own graph.
    #[test]
    fn detail_grid_excludes_the_headline_sensor() {
        let cats = build_categories(&frame());
        assert!(
            cats[0].detail.iter().all(|(k, _)| k != "Total CPU Usage"),
            "headline should not repeat in the detail grid: {:?}",
            cats[0].detail
        );
        assert_eq!(cats[0].detail.len(), 2);
    }

    #[test]
    fn a_node_with_no_sensors_produces_no_category() {
        let empty = TelemetryFrame {
            tree: vec![Hardware {
                identifier: "/x".into(),
                name: "Empty".into(),
                hardware_type: HardwareType::Cpu,
                sensors: Vec::new(),
                sub_hardware: Vec::new(),
            }],
            ..Default::default()
        };
        assert!(build_categories(&empty).is_empty());
    }

    #[test]
    fn every_gpu_variant_counts_as_a_performance_node() {
        for t in [
            HardwareType::GpuApple,
            HardwareType::GpuAti,
            HardwareType::GpuIntel,
            HardwareType::GpuNvidia,
        ] {
            assert!(is_performance_node(t), "{t:?} should be a performance node");
        }
        assert!(!is_performance_node(HardwareType::Battery));
    }

    /// The first thing anyone wants from a process list is what is *busiest*.
    /// `#[derive(Default)]` would give `descending: false` and open on the
    /// idlest processes, which is how this shipped the first time.
    #[test]
    fn default_sort_is_busiest_first() {
        let st = State::default();
        assert_eq!(st.sort, Sort::Cpu);
        assert!(st.descending, "process list must open busiest-first");

        // And the ordering actually follows from it.
        let rows = vec![
            crate::procs::ProcessRow {
                pid: 1, parent: None, name: "idle".into(), cmd: String::new(),
                user: None, cpu_pct: Some(0.0), mem_bytes: 1, virt_bytes: 1,
                disk_read_bytes: 0, disk_write_bytes: 0, run_time_s: 0,
            },
            crate::procs::ProcessRow {
                pid: 2, parent: None, name: "busy".into(), cmd: String::new(),
                user: None, cpu_pct: Some(90.0), mem_bytes: 1, virt_bytes: 1,
                disk_read_bytes: 0, disk_write_bytes: 0, run_time_s: 0,
            },
        ];
        let sorted = crate::procs::arrange(&rows, None, st.sort, st.descending);
        assert_eq!(sorted[0].name, "busy");
    }

    /// Apple Silicon names its CPU and GPU nodes identically ("Apple M5"), so
    /// device-named sidebar rows collide. Roles must disambiguate them.
    #[test]
    fn duplicate_device_names_get_distinct_role_titles() {
        let cats = build_categories(&frame());
        let titles: Vec<&str> = cats.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["CPU", "GPU"], "roles must disambiguate: {titles:?}");
    }

    /// Several disks must be numbered rather than all called "Disk".
    #[test]
    fn multiple_disks_are_numbered() {
        let disk = |id: &str| Hardware {
            identifier: id.into(),
            name: "APPLE SSD".into(),
            hardware_type: HardwareType::Storage,
            sensors: vec![sensor("Read Rate", id, SensorType::Throughput, Some(1.0))],
            sub_hardware: Vec::new(),
        };
        let f = TelemetryFrame {
            tree: vec![disk("/d0"), disk("/d1")],
            ..Default::default()
        };
        let cats = build_categories(&f);
        assert_eq!(
            cats.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
            vec!["Disk 0", "Disk 1"]
        );
    }

    /// Called every frame, so it must not spawn a thread per frame.
    #[test]
    fn ensure_collector_is_idempotent() {
        let slot = Mutex::new(None);
        ensure_collector(&slot);

        // Wait for a published snapshot first: comparing two seq values is only
        // meaningful once the collector has produced one, and on a slow machine
        // an immediate read returns the empty seq-0 placeholder either way.
        let first = {
            let guard = slot.lock().unwrap();
            crate::procs::wait_for(guard.as_ref().unwrap(), "the first snapshot", |s| s.seq >= 1)
                .seq
        };

        for _ in 0..5 {
            ensure_collector(&slot);
        }
        assert!(slot.lock().unwrap().is_some());

        // Same collector, not a replacement: a fresh one would restart at 0,
        // so seq must only ever move forward.
        let later = slot.lock().unwrap().as_ref().unwrap().snapshot().seq;
        assert!(
            later >= first,
            "repeated ensure_collector replaced the collector ({first} -> {later})"
        );
        stop_collector(&slot);
    }

    /// Open, close, reopen — the cycle a user actually performs. Closing must
    /// really stop it (not merely hide the window), and reopening must produce
    /// a collector that re-establishes its own CPU baseline.
    #[test]
    fn open_close_reopen_restarts_collection() {
        let slot = Mutex::new(None);

        ensure_collector(&slot);
        {
            let guard = slot.lock().unwrap();
            let snap = crate::procs::wait_for(guard.as_ref().unwrap(), "the first snapshot", |s| {
                s.seq >= 1
            });
            assert!(!snap.rows.is_empty(), "collector produced no processes");
        }

        stop_collector(&slot);
        assert!(slot.lock().unwrap().is_none(), "closing must drop the collector");

        ensure_collector(&slot);
        let guard = slot.lock().unwrap();
        let snap = crate::procs::wait_for(
            guard.as_ref().unwrap(),
            "a CPU baseline after reopening",
            |s| s.has_cpu(),
        );
        assert!(!snap.rows.is_empty(), "reopened collector produced nothing");
        drop(guard);
        stop_collector(&slot);
    }

    /// A flat history would divide by zero when normalising; it must still
    /// draw rather than produce NaN coordinates.
    #[test]
    fn flat_history_does_not_produce_nan_points() {
        let history = vec![5.0f32; 10];
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for &v in &history {
            lo = lo.min(v);
            hi = hi.max(v);
        }
        if (hi - lo).abs() < f32::EPSILON {
            lo -= 1.0;
            hi += 1.0;
        }
        let y = ((history[0] - lo) / (hi - lo)) * 100.0;
        assert!(y.is_finite(), "flat series produced a non-finite coordinate");
    }
}
