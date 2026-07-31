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
                // Shown as a share of the machine, matching the CPU column
                // header — the same number twice in two different units read
                // as a contradiction.
                let cpu = if snapshot.has_cpu() {
                    format!("{:.0} %", snapshot.total_cpu_pct / snapshot.cpu_count.max(1) as f32)
                } else {
                    // Not zero: the baseline simply doesn't exist yet.
                    "—".to_string()
                };
                ui.label(
                    RichText::new(format!(
                        "{} processes · CPU {cpu} of {} logical CPUs",
                        snapshot.rows.len(),
                        snapshot.cpu_count
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

            // Every heat column is scaled to the busiest process on show, which
            // makes the shading a *ranking* rather than a measurement — the
            // exact figure is in the cell, and the tint is only there to draw
            // the eye to it.
            //
            // CPU gets a floor as well. Scaling it against a full machine
            // instead looks principled but renders the column colourless in
            // practice: on 16 threads a busy process is ~6 % of the whole, so
            // every cell would be the palest tint. The floor stops the other
            // extreme — on a genuinely idle machine the top process is not
            // painted scarlet for using 0.4 %.
            let cpus = snapshot.cpu_count.max(1) as f32;
            let peak_cpu = rows
                .iter()
                .filter_map(|r| r.cpu_pct)
                .fold(0.0f32, f32::max)
                .max(2.0 * cpus);
            let peak_mem = rows.iter().map(|r| r.mem_bytes).max().unwrap_or(0) as f32;
            let peak_disk = rows
                .iter()
                .filter_map(|r| r.disk_bps)
                .fold(0.0f64, f64::max) as f32;

            let row_h = 20.0;
            // Virtualised: `body.rows` only builds what is on screen, which
            // matters at ~700 processes.
            TableBuilder::new(ui)
                .striped(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::remainder())    // name
                .column(Column::exact(72.0))    // cpu
                .column(Column::exact(84.0))    // memory
                .column(Column::exact(84.0))    // disk
                .column(Column::exact(58.0))    // pid
                .column(Column::exact(90.0))    // user
                .header(34.0, |mut header| {
                    // Windows heads each measured column with the machine-wide
                    // total, so the columns double as a system summary.
                    let total_cpu = snapshot
                        .has_cpu()
                        .then(|| format!("{:.0}%", snapshot.total_cpu_pct / cpus));
                    let total_disk = (peak_disk > 0.0).then(|| {
                        let sum: f64 = rows.iter().filter_map(|r| r.disk_bps).sum();
                        procs::format_rate(Some(sum))
                    });
                    for (label, total, sort) in [
                        ("Name", None, Some(Sort::Name)),
                        ("CPU", total_cpu, Some(Sort::Cpu)),
                        ("Memory", None, Some(Sort::Memory)),
                        ("Disk", total_disk, Some(Sort::Disk)),
                        ("PID", None, Some(Sort::Pid)),
                        ("User", None, None),
                    ] {
                        header.col(|ui| {
                            sort_header(ui, pal, state, label, total.as_deref(), sort);
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_h, rows.len(), |mut row| {
                        let r = &rows[row.index()];
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
                        row.col(|ui| {
                            // An unknown reading is an em dash, never 0.0 —
                            // "we can't see" and "idle" are different facts.
                            match r.cpu_pct {
                                None => {
                                    heat_cell(ui, pal, "—", None);
                                }
                                // Shown as a share of the whole machine, which
                                // is what a task manager's CPU column means to
                                // a reader. `cpu_pct` itself is per-thread
                                // summed (1600 % on 16 threads); the raw figure
                                // is in the tooltip so nothing is lost.
                                Some(v) => {
                                    let share = v / cpus;
                                    heat_cell(ui, pal, &format!("{share:.1}%"), Some(v / peak_cpu))
                                    .on_hover_text(format!(
                                        "{v:.1}% summed across {} logical CPUs",
                                        snapshot.cpu_count
                                    ));
                                }
                            }
                        });
                        row.col(|ui| {
                            let t = (peak_mem > 0.0).then(|| r.mem_bytes as f32 / peak_mem);
                            heat_cell(ui, pal, &procs::format_bytes(r.mem_bytes), t);
                        });
                        row.col(|ui| {
                            let t = r
                                .disk_bps
                                .filter(|_| peak_disk > 0.0)
                                .map(|v| v as f32 / peak_disk);
                            heat_cell(ui, pal, &procs::format_rate(r.disk_bps), t);
                        });
                        row.col(|ui| mono(ui, pal, &r.pid.to_string()));
                        row.col(|ui| {
                            ui.label(
                                RichText::new(r.user.as_deref().unwrap_or("—"))
                                    .size(10.5)
                                    .color(pal.text_dim),
                            );
                        });
                    });
                });
        });
}

/// A table cell shaded by how big its value is, the way Windows tints its
/// CPU/Memory/Disk columns. `intensity` is 0..=1, or `None` for "no reading",
/// which is left unshaded rather than shaded as if it were zero.
fn heat_cell(
    ui: &mut egui::Ui,
    pal: &Palette,
    text: &str,
    intensity: Option<f32>,
) -> egui::Response {
    let t = intensity.unwrap_or(0.0).clamp(0.0, 1.0);
    // Below this the tint is indistinguishable from the stripe and only makes
    // the table look dirty, so idle rows stay clean.
    if t > 0.02 {
        let rect = ui.max_rect().expand2(egui::vec2(3.0, 0.0));
        ui.painter().rect_filled(rect, 1.0, heat_color(t, pal));
    }
    let color = if intensity.is_none() {
        pal.text_dim
    } else if t > 0.55 {
        // Dark enough underneath that the normal value colour stops reading.
        pal.text
    } else {
        pal.value
    };
    ui.label(RichText::new(text).size(10.5).monospace().color(color))
}

/// Amber → red as the value climbs, with the tint strengthening too, so both
/// hue and weight carry the signal.
fn heat_color(t: f32, pal: &Palette) -> Color32 {
    let base = pal.warn.lerp_to_gamma(pal.crit, t);
    base.gamma_multiply(0.16 + 0.5 * t)
}

/// A column heading, optionally carrying the machine-wide total above it the
/// way Windows does ("CPU" with "23%" over it).
fn sort_header(
    ui: &mut egui::Ui,
    pal: &Palette,
    state: &mut State,
    label: &str,
    total: Option<&str>,
    sort: Option<Sort>,
) {
    // Vertical so the total can sit above the label; without this the header
    // is a single baseline and there is nowhere to put it. The blank line
    // keeps every heading the same height when a column has no total.
    ui.vertical(|ui| {
        ui.add_space(1.0);
        ui.label(
            RichText::new(total.unwrap_or(" "))
                .size(11.5)
                .color(if total.is_some() { pal.text } else { pal.text_dim }),
        );
        sort_label(ui, pal, state, label, sort);
    });
}

/// The clickable label half of a column heading.
fn sort_label(
    ui: &mut egui::Ui,
    pal: &Palette,
    state: &mut State,
    label: &str,
    sort: Option<Sort>,
) {
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
    /// The device's own name, kept separately from `title` so repeated roles
    /// can be re-titled once the whole tree is known. See [`disambiguate`].
    device: String,
    subtitle: String,
    /// Sensor identifier whose history drives the sparkline and graph.
    identifier: String,
    value: Option<f32>,
    sensor_type: SensorType,
    /// Kept rather than a resolved `Color32` so `build_categories` stays
    /// palette-free and therefore testable without a UI context.
    hardware_type: HardwareType,
    /// Sensors shown in the stats grid for this category.
    detail: Vec<(String, String)>,
}

/// Windows gives each device class its own hue and uses it for everything that
/// device owns — sidebar sparkline, big graph, fill. That single choice is most
/// of what makes its Task Manager readable at a glance, so it is reproduced
/// here rather than painting every graph in one accent colour.
///
/// These are our own approximations of that scheme, not sampled assets.
fn category_color(t: HardwareType, pal: &Palette) -> Color32 {
    match t {
        HardwareType::Cpu => Color32::from_rgb(0x1f, 0x7f, 0xc1),           // blue
        HardwareType::Ram => Color32::from_rgb(0x8b, 0x5f, 0xbf),           // purple
        HardwareType::Storage | HardwareType::Hdd => Color32::from_rgb(0x2e, 0x9e, 0x5b), // green
        HardwareType::Network => Color32::from_rgb(0xc4, 0x51, 0x7a),       // rose
        HardwareType::GpuApple
        | HardwareType::GpuAti
        | HardwareType::GpuIntel
        | HardwareType::GpuNvidia => Color32::from_rgb(0x1e, 0x9c, 0xa8),   // teal
        _ => pal.accent,
    }
}

/// A utilisation graph is pinned to 0-100 % the way Windows pins its CPU chart,
/// so a quiet machine reads as a low line rather than an auto-scaled one that
/// looks busy. Everything else keeps auto-scaling, because there is no
/// meaningful fixed ceiling for a clock or a temperature.
fn fixed_scale(t: SensorType) -> Option<(f32, f32)> {
    matches!(t, SensorType::Load).then_some((0.0, 100.0))
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

    let color = category_color(cat.hardware_type, pal);

    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(pal.bg).inner_margin(egui::Margin::same(12)))
        .show(ui, |ui| {
            // Title left, device name right — Windows' header line.
            ui.horizontal(|ui| {
                ui.label(RichText::new(&cat.title).size(19.0).color(pal.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(&cat.subtitle).size(11.5).color(pal.text_dim));
                });
            });
            ui.add_space(6.0);

            let graph_h = (ui.available_height() - 22.0 * cat.detail.len().min(6) as f32 - 56.0)
                .clamp(140.0, 360.0);
            let (rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), graph_h), egui::Sense::hover());
            paint_graph(ui, rect, &history, color, pal, cat.sensor_type);

            ui.add_space(10.0);

            // The headline reading, big and in the device's colour — the number
            // Windows puts under the chart before the smaller stats.
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(crate::format::format_value(cat.value, cat.sensor_type))
                        .size(26.0)
                        .color(color),
                );
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new(&cat.title).size(11.0).color(pal.text_dim));
                });
            });

            ui.add_space(8.0);

            // Two columns of stats, as Windows lays out its detail block.
            egui::Grid::new("taskmgr_detail")
                .num_columns(4)
                .spacing([22.0, 4.0])
                .show(ui, |ui| {
                    for pair in cat.detail.chunks(2) {
                        for (k, v) in pair {
                            ui.label(RichText::new(k).size(11.0).color(pal.text_dim));
                            ui.label(RichText::new(v).size(11.0).monospace().color(pal.value));
                        }
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
    // Taller than a list row: Windows' sidebar entries are cards carrying a
    // live thumbnail of the same graph, not one-line labels.
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 56.0), egui::Sense::click());
    let p = ui.painter_at(rect);
    let color = category_color(cat.hardware_type, pal);

    if selected {
        p.rect_filled(rect, 3.0, pal.bg_header);
        // The selection bar takes the device's colour, so the sidebar says
        // which device is selected twice over — position and hue.
        p.rect_filled(
            egui::Rect::from_min_max(
                Pos2::new(rect.left(), rect.top() + 2.0),
                Pos2::new(rect.left() + 3.0, rect.bottom() - 2.0),
            ),
            1.5,
            color,
        );
    } else if resp.hovered() {
        p.rect_filled(rect, 3.0, pal.row_odd);
    }

    let text_x = rect.left() + 10.0;
    // Device names are as long as their vendors made them ("OpenVPN Data
    // Channel Offload"), so the title is elided to the space left of the
    // thumbnail. Painting it unclipped ran it under the graph.
    let title_w = (rect.right() - 86.0) - text_x;
    let galley = {
        let mut job = egui::text::LayoutJob::simple_singleline(
            cat.title.clone(),
            FontId::proportional(12.0),
            if selected { pal.text } else { pal.text_dim },
        );
        job.wrap.max_width = title_w.max(24.0);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        p.layout_job(job)
    };
    p.galley(
        Pos2::new(text_x, rect.top() + 7.0),
        galley,
        if selected { pal.text } else { pal.text_dim },
    );
    p.text(
        Pos2::new(text_x, rect.bottom() - 8.0),
        Align2::LEFT_BOTTOM,
        crate::format::format_value(cat.value, cat.sensor_type),
        FontId::proportional(12.5),
        color,
    );

    // Thumbnail graph on the right, filled like the big one.
    let thumb = egui::Rect::from_min_max(
        Pos2::new(rect.right() - 80.0, rect.top() + 8.0),
        Pos2::new(rect.right() - 8.0, rect.bottom() - 8.0),
    );
    p.rect_filled(thumb, 1.0, pal.bg.gamma_multiply(0.6));
    paint_sparkline(
        &p,
        thumb,
        &s.store.history(&cat.identifier),
        color,
        fixed_scale(cat.sensor_type),
    );
    p.rect_stroke(
        thumb,
        1.0,
        Stroke::new(1.0, pal.grid.gamma_multiply(0.7)),
        egui::StrokeKind::Inside,
    );

    // Two NICs can share a visible prefix once elided ("Local Area Connec…"),
    // so the untruncated name has to be reachable without selecting the row.
    let resp = resp.on_hover_text(&cat.subtitle);

    resp.clicked()
}

fn paint_sparkline(
    p: &egui::Painter,
    rect: egui::Rect,
    history: &[f32],
    line: Color32,
    fixed: Option<(f32, f32)>,
) {
    if history.len() < 2 {
        return;
    }
    let (lo, hi) = value_range(history, fixed);
    let n = history.len();
    let pts: Vec<Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = rect.left() + (i as f32 / (n - 1) as f32) * rect.width();
            let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
            Pos2::new(x, rect.bottom() - t * rect.height())
        })
        .collect();
    fill_under(p, rect, &pts, line);
    p.add(egui::Shape::line(pts, Stroke::new(1.2, line)));
}

/// Work out the value range a series should be drawn against.
///
/// Split out from painting so both the graph and its axis labels agree, and so
/// the flat-series case has one home instead of being re-derived per caller.
fn value_range(history: &[f32], fixed: Option<(f32, f32)>) -> (f32, f32) {
    if let Some(range) = fixed {
        return range;
    }
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for &v in history {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if !lo.is_finite() || !hi.is_finite() {
        return (0.0, 1.0);
    }
    // A flat series would divide by zero; give it a band so it draws mid-height.
    if (hi - lo).abs() < f32::EPSILON {
        return (lo - 1.0, hi + 1.0);
    }
    let pad = (hi - lo) * 0.08;
    (lo - pad, hi + pad)
}

/// The big graph, drawn the way Windows draws it: a fixed grid, the series
/// filled down to the baseline in the device's own colour, and the axis
/// described in words at the corners rather than with numbered ticks.
fn paint_graph(
    ui: &egui::Ui,
    rect: egui::Rect,
    history: &[f32],
    line: Color32,
    pal: &Palette,
    sensor_type: SensorType,
) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, pal.row_odd);

    // The grid is drawn whether or not there is data, so the panel has the
    // same shape while the first samples are still arriving.
    let cells = 10.0;
    let grid = pal.grid.gamma_multiply(0.45);
    for i in 1..cells as i32 {
        let t = i as f32 / cells;
        let x = rect.left() + t * rect.width();
        let y = rect.top() + t * rect.height();
        p.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.0, grid),
        );
        p.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(1.0, grid),
        );
    }
    p.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, pal.grid),
        egui::StrokeKind::Inside,
    );

    let fixed = fixed_scale(sensor_type);
    let (lo, hi) = value_range(history, fixed);

    // Corner labels, as Windows captions its charts.
    let caption = match sensor_type {
        SensorType::Load => "% Utilisation",
        SensorType::Temperature => "Temperature",
        SensorType::Clock => "Clock",
        SensorType::Throughput => "Throughput",
        SensorType::Power => "Power",
        _ => "Value",
    };
    p.text(
        Pos2::new(rect.left() + 6.0, rect.top() + 4.0),
        Align2::LEFT_TOP,
        caption,
        FontId::proportional(10.5),
        pal.text_dim,
    );
    p.text(
        Pos2::new(rect.right() - 6.0, rect.top() + 4.0),
        Align2::RIGHT_TOP,
        crate::format::format_value(Some(hi), sensor_type),
        FontId::proportional(10.5),
        pal.text_dim,
    );
    p.text(
        Pos2::new(rect.left() + 6.0, rect.bottom() - 4.0),
        Align2::LEFT_BOTTOM,
        "60 seconds",
        FontId::proportional(10.5),
        pal.text_dim,
    );
    p.text(
        Pos2::new(rect.right() - 6.0, rect.bottom() - 4.0),
        Align2::RIGHT_BOTTOM,
        crate::format::format_value(Some(lo), sensor_type),
        FontId::proportional(10.5),
        pal.text_dim,
    );

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

    let n = history.len();
    let pts: Vec<Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = rect.left() + (i as f32 / (n - 1) as f32) * rect.width();
            let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
            Pos2::new(x, rect.bottom() - t * rect.height())
        })
        .collect();

    fill_under(&p, rect, &pts, line);
    p.add(egui::Shape::line(pts, Stroke::new(1.6, line)));
}

/// Fill the area between a series and the baseline.
///
/// Emitted as a quad per sample pair rather than one polygon: egui's polygon
/// fill wants a convex path, and a utilisation trace is anything but.
fn fill_under(p: &egui::Painter, rect: egui::Rect, pts: &[Pos2], color: Color32) {
    if pts.len() < 2 {
        return;
    }
    let fill = color.gamma_multiply(0.28);
    let mut mesh = egui::Mesh::default();
    for pair in pts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let base = mesh.vertices.len() as u32;
        for pos in [
            a,
            b,
            Pos2::new(b.x, rect.bottom()),
            Pos2::new(a.x, rect.bottom()),
        ] {
            mesh.colored_vertex(pos, fill);
        }
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    }
    p.add(egui::Shape::mesh(mesh));
}

/// Reduce the hardware tree to sidebar categories.
///
/// Each hardware node contributes one category, headlined by the sensor a
/// person would actually look at first — utilisation where there is one,
/// otherwise temperature, otherwise whatever it has.
fn build_categories(frame: &TelemetryFrame) -> Vec<Category> {
    let mut out = Vec::new();
    walk(&frame.tree, &mut out);
    disambiguate(&mut out);
    out
}

fn walk(tree: &[Hardware], out: &mut Vec<Category>) {
    for hw in tree {
        if let Some(cat) = category_for(hw) {
            out.push(cat);
        }
        walk(&hw.sub_hardware, out);
    }
}

/// Sidebar titles name the *role*, not the device: on Apple Silicon the CPU
/// and GPU nodes are both called "Apple M5", so device names alone produce two
/// identical rows. The device name becomes the subtitle instead.
fn role_title(t: HardwareType) -> &'static str {
    match t {
        HardwareType::Cpu => "CPU",
        HardwareType::Ram => "Memory",
        HardwareType::Storage | HardwareType::Hdd => "Disk",
        HardwareType::Network => "Network",
        _ => "GPU",
    }
}

/// Make repeated roles tell themselves apart.
///
/// A bare role is only unambiguous when the machine has one of them. This box
/// has two RAM-class nodes ("Total Memory" and "Virtual Memory") and six NICs,
/// which as bare roles rendered as two cards both labelled "Memory" and six
/// labelled "Network" — the sidebar could not say which was which.
///
/// Run as a post-pass because the rule depends on the final tally, which no
/// single node knows while the tree is still being walked.
///
/// Where the devices have distinct names, those names *are* the disambiguator
/// and read far better than an index — "Wi-Fi 2" beats "Network 5", and this is
/// what Windows shows. Numbering is the fallback for genuinely identical
/// devices, such as two disks that report the same model string.
fn disambiguate(cats: &mut [Category]) {
    let mut counts: std::collections::HashMap<&'static str, usize> =
        std::collections::HashMap::new();
    for c in cats.iter() {
        *counts.entry(role_title(c.hardware_type)).or_default() += 1;
    }

    let mut seen: std::collections::HashMap<&'static str, usize> =
        std::collections::HashMap::new();
    for i in 0..cats.len() {
        let role = role_title(cats[i].hardware_type);
        if counts.get(role).copied().unwrap_or(0) < 2 {
            continue;
        }
        // Distinct among the *others* sharing this role, not globally.
        let unique = !cats.iter().enumerate().any(|(j, other)| {
            j != i
                && role_title(other.hardware_type) == role
                && other.device == cats[i].device
        });
        let n = seen.entry(role).or_default();
        cats[i].title = if unique {
            cats[i].device.clone()
        } else {
            format!("{role} {n}")
        };
        *n += 1;
    }
}

fn category_for(hw: &Hardware) -> Option<Category> {
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
        title: role_title(hw.hardware_type).to_string(),
        device: hw.name.clone(),
        subtitle: format!("{} · {} sensors", hw.name, hw.sensors.len()),
        identifier: headline.identifier.clone(),
        value: headline.value,
        sensor_type: headline.sensor_type,
        hardware_type: hw.hardware_type,
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
                disk_read_bytes: 0, disk_write_bytes: 0, disk_bps: None, run_time_s: 0,
            },
            crate::procs::ProcessRow {
                pid: 2, parent: None, name: "busy".into(), cmd: String::new(),
                user: None, cpu_pct: Some(90.0), mem_bytes: 1, virt_bytes: 1,
                disk_read_bytes: 0, disk_write_bytes: 0, disk_bps: None, run_time_s: 0,
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

    /// Two RAM-class nodes are real on Windows — physical and virtual memory —
    /// and both rendered as a bare "Memory" card, which is the one thing the
    /// sidebar must never do. Distinct device names disambiguate them.
    #[test]
    fn repeated_roles_with_distinct_names_use_those_names() {
        let ram = |id: &str, name: &str| Hardware {
            identifier: id.into(),
            name: name.into(),
            hardware_type: HardwareType::Ram,
            sensors: vec![sensor("Load", id, SensorType::Load, Some(50.0))],
            sub_hardware: Vec::new(),
        };
        let f = TelemetryFrame {
            tree: vec![ram("/ram", "Total Memory"), ram("/vram", "Virtual Memory")],
            ..Default::default()
        };
        let titles: Vec<String> = build_categories(&f).iter().map(|c| c.title.clone()).collect();
        assert_eq!(titles, vec!["Total Memory", "Virtual Memory"]);
    }

    /// A lone instance keeps the plain role name — Windows shows "Memory", not
    /// "Memory 0", when there is only one.
    #[test]
    fn a_single_instance_keeps_the_bare_role_name() {
        let f = TelemetryFrame {
            tree: vec![Hardware {
                identifier: "/ram".into(),
                name: "Total Memory".into(),
                hardware_type: HardwareType::Ram,
                sensors: vec![sensor("Load", "/r", SensorType::Load, Some(50.0))],
                sub_hardware: Vec::new(),
            }],
            ..Default::default()
        };
        assert_eq!(build_categories(&f)[0].title, "Memory");
    }

    /// Several disks reporting the *same* model string have no name to tell
    /// them apart, so they fall back to numbering.
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
        let (lo, hi) = value_range(&[5.0f32; 10], None);
        let y = ((5.0 - lo) / (hi - lo)) * 100.0;
        assert!(y.is_finite(), "flat series produced a non-finite coordinate");
        assert!(lo < hi, "a flat series must still get a non-empty band");
    }

    /// An empty series must not yield infinities from the min/max fold.
    #[test]
    fn empty_history_yields_a_usable_range() {
        let (lo, hi) = value_range(&[], None);
        assert!(lo.is_finite() && hi.is_finite() && lo < hi, "got {lo}..{hi}");
    }

    /// Utilisation charts are pinned to 0-100 so an idle machine reads as idle,
    /// rather than being auto-scaled until noise fills the panel.
    #[test]
    fn load_graphs_are_pinned_to_full_scale() {
        assert_eq!(fixed_scale(SensorType::Load), Some((0.0, 100.0)));
        // A quiet CPU stays near the floor instead of being stretched.
        let (lo, hi) = value_range(&[1.0, 2.0, 1.5], fixed_scale(SensorType::Load));
        assert_eq!((lo, hi), (0.0, 100.0));
        // Everything else keeps auto-scaling — there is no fixed ceiling for a
        // clock or a temperature.
        assert_eq!(fixed_scale(SensorType::Clock), None);
        let (lo, hi) = value_range(&[3000.0, 4000.0], fixed_scale(SensorType::Clock));
        assert!(lo < 3000.0 && hi > 4000.0, "auto-scale should pad: {lo}..{hi}");
    }

    /// Each device class must get its own hue, or the colour carries no
    /// information — that distinctness is the whole point of the scheme.
    #[test]
    fn device_classes_get_distinct_colors() {
        let pal = Palette::of(crate::settings::ColorMode::Black);
        let types = [
            HardwareType::Cpu,
            HardwareType::Ram,
            HardwareType::Storage,
            HardwareType::Network,
            HardwareType::GpuNvidia,
        ];
        let colors: Vec<Color32> = types.iter().map(|t| category_color(*t, &pal)).collect();
        for (i, a) in colors.iter().enumerate() {
            for (j, b) in colors.iter().enumerate() {
                assert!(i == j || a != b, "{:?} and {:?} share a colour", types[i], types[j]);
            }
        }
        // Every GPU vendor is one device class and shares the GPU hue.
        assert_eq!(
            category_color(HardwareType::GpuAti, &pal),
            category_color(HardwareType::GpuNvidia, &pal)
        );
    }

    /// The heat tint must grow with the value, and stay clear of the row
    /// stripe at the bottom end so an idle table doesn't look grubby.
    #[test]
    fn heat_intensity_increases_with_value() {
        let pal = Palette::of(crate::settings::ColorMode::Black);
        let low = heat_color(0.1, &pal);
        let high = heat_color(1.0, &pal);
        assert!(
            high.a() > low.a(),
            "a busier cell must be tinted more strongly: {low:?} vs {high:?}"
        );
        // Amber at the bottom, red at the top: hue carries signal as well.
        // Compared as a red:green *ratio* rather than raw channels, because
        // these are premultiplied — the alpha ramp above scales every channel,
        // so absolute values say nothing about hue on their own.
        let ratio = |c: Color32| f32::from(c.r()) / f32::from(c.g()).max(1.0);
        assert!(
            ratio(high) > ratio(low),
            "tint should shift towards red as load climbs: {low:?} -> {high:?}"
        );
    }
}
