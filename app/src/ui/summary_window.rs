//! HWiNFO-style "System Summary" window: CPU / Motherboard / Memory / GPU /
//! OS / Drives panel grid with the ISA features chip-grid and an Operating
//! Point table fed by live sensors.

use eframe::egui::{self, RichText};

use super::widgets::{chip, info_row, panel};
use super::{Palette, Shared};
use crate::model::{Hardware, HardwareType, SensorType};

pub fn show(ui: &mut egui::Ui, s: &Shared) {
    super::handle_close(ui, &s.windows.summary);
    let pal = s.palette();
    let info = s.sysinfo.read().ok().and_then(|i| i.clone());
    let frame = s.frame();
    let tree = &frame.tree;

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(pal.bg)
                .inner_margin(egui::Margin::same(8)),
        )
        .show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                let Some(i) = info else {
                    ui.label(RichText::new("Enumerating system…").color(pal.text_dim));
                    return;
                };

                ui.columns(3, |cols| {
                    // ---- CPU ------------------------------------------------
                    cpu_panel(&mut cols[0], &i, tree, &pal);
                    // ---- Motherboard + Memory + Battery --------------------
                    board_memory_panels(&mut cols[1], &i, &pal);
                    battery_panel(&mut cols[1], &i, &pal);
                    // ---- GPU + Displays + OS + Drives ----------------------
                    gpu_os_drives_panels(&mut cols[2], &i, tree, &pal);
                });
            });
        });
}

fn cpu_panel(ui: &mut egui::Ui, i: &crate::sysinfo::SystemInfo, tree: &[Hardware], pal: &Palette) {
    panel(ui, "CPU", pal, |ui| {
        // Vendor text badge (no trademarked logos).
        let vendor = if i.cpu.name.to_uppercase().contains("AMD") {
            "AMD"
        } else if i.cpu.name.to_uppercase().contains("INTEL") {
            "INTEL"
        } else if i.cpu.name.starts_with("Apple") {
            "APPLE"
        } else {
            "CPU"
        };
        ui.horizontal(|ui| {
            egui::Frame::new()
                .fill(pal.bg_header)
                .corner_radius(3)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.label(RichText::new(vendor).color(pal.accent).size(15.0).strong());
                });
            ui.vertical(|ui| {
                ui.label(RichText::new(&i.cpu.name).color(pal.text).size(12.0).strong());
                ui.label(
                    RichText::new(i.cpu.socket.as_deref().unwrap_or("—"))
                        .color(pal.text_dim)
                        .size(10.5),
                );
            });
        });
        ui.add_space(4.0);

        let cores = i
            .cpu
            .cores
            .map(|c| format!("{c} / {}", i.cpu.threads.unwrap_or(c)))
            .unwrap_or_default();
        info_row(ui, "Cores / Threads:", &cores, pal);
        info_row(ui, "L2 Cache:", &i.cpu.l2_kb.map(crate::format::cache_size).unwrap_or_default(), pal);
        info_row(ui, "L3 Cache:", &i.cpu.l3_kb.map(crate::format::cache_size).unwrap_or_default(), pal);
        info_row(ui, "Codename:", &i.cpu.codename, pal);
        info_row(ui, "CPUID:", &i.cpu.cpuid, pal);
        info_row(
            ui,
            "Package Power:",
            &cpu_sensor(tree, SensorType::Power, "package")
                .map(|v| format!("{v:.1} W"))
                .unwrap_or_default(),
            pal,
        );
        // Windows/Linux name it "Package"; the Mac backend's headline is the
        // hottest CPU die probe.
        info_row(
            ui,
            "Temperature:",
            &cpu_sensor(tree, SensorType::Temperature, "package")
                .or_else(|| cpu_sensor(tree, SensorType::Temperature, "cpu die (hottest)"))
                .map(|v| format!("{v:.1} °C"))
                .unwrap_or_default(),
            pal,
        );

        ui.add_space(4.0);
        ui.label(RichText::new("Features").color(pal.text_dim).size(10.5));
        // Fixed rows of 5 — deterministic wrap regardless of column width.
        for row in i.cpu.features.chunks(5) {
            ui.horizontal(|ui| {
                for (name, on) in row {
                    chip(ui, name, *on, pal);
                }
            });
        }

        ui.add_space(6.0);
        ui.label(RichText::new("Operating Point").color(pal.text_dim).size(10.5));
        operating_point_table(ui, i, tree, pal);
    });
}

/// Min/Base/Boost/Avg clock table from WMI base clock + live core clocks/VIDs.
fn operating_point_table(ui: &mut egui::Ui, i: &crate::sysinfo::SystemInfo, tree: &[Hardware], pal: &Palette) {
    let clocks = collect_cpu(tree, SensorType::Clock, "core");
    let vids = collect_cpu(tree, SensorType::Voltage, "vid");
    let cur_avg = mean(&clocks);
    let cur_max = clocks.iter().copied().fold(f32::NAN, f32::max);
    let vid = mean(&vids);

    egui::Grid::new("op_table")
        .num_columns(3)
        .spacing([12.0, 2.0])
        .show(ui, |ui| {
            let head = |ui: &mut egui::Ui, t: &str| {
                ui.label(RichText::new(t).color(pal.text_dim).size(10.5).strong());
            };
            head(ui, "");
            head(ui, "Clock");
            head(ui, "VID");
            ui.end_row();

            let row = |ui: &mut egui::Ui, name: &str, clock: Option<f32>, vid: Option<f32>, pal: &Palette| {
                ui.label(RichText::new(name).color(pal.text).size(10.5));
                ui.label(
                    RichText::new(clock.map(|c| format!("{c:.1} MHz")).unwrap_or("—".into()))
                        .color(pal.clockc)
                        .size(10.5)
                        .monospace(),
                );
                ui.label(
                    RichText::new(vid.map(|v| format!("{v:.4} V")).unwrap_or("—".into()))
                        .color(pal.volt)
                        .size(10.5)
                        .monospace(),
                );
                ui.end_row();
            };
            row(ui, "Base Clock", i.cpu.base_clock_mhz.map(|c| c as f32), None, pal);
            row(ui, "Max Clock", Some(cur_max).filter(|v| v.is_finite()), None, pal);
            row(ui, "Avg. Active Clock", cur_avg, vid, pal);
        });
}

fn board_memory_panels(ui: &mut egui::Ui, i: &crate::sysinfo::SystemInfo, pal: &Palette) {
    panel(ui, "Motherboard", pal, |ui| {
        ui.label(
            RichText::new(format!("{} {}", i.board.manufacturer, i.board.product))
                .color(pal.text)
                .size(12.0)
                .strong(),
        );
        ui.add_space(2.0);
        if !i.board.model_id.is_empty() {
            info_row(ui, "Model:", &i.board.model_id, pal);
        }
        if !i.board.board_id.is_empty() {
            info_row(ui, "Board ID:", &i.board.board_id, pal);
        }
        // Empty on Windows/Linux until PCI enumeration lands (native engine);
        // the Apple Silicon SoC is its own chipset and is named.
        info_row(ui, "Chipset:", &i.board.chipset, pal);
        info_row(ui, "BIOS Version:", &i.board.bios_version, pal);
        info_row(ui, "BIOS Date:", &i.board.bios_date, pal);
    });

    ui.add_space(6.0);

    panel(ui, "Memory", pal, |ui| {
        info_row(
            ui,
            "Size:",
            &i.total_memory_gb.map(|g| format!("{g:.0} GB")).unwrap_or_default(),
            pal,
        );
        let mem_type = i
            .memory_modules
            .first()
            .map(|m| crate::sysinfo::memory_type_label(&m.memory_type))
            .unwrap_or_default();
        info_row(ui, "Type:", &mem_type, pal);
        let clock = i
            .memory_modules
            .first()
            .and_then(|m| m.configured_speed_mts.or(m.speed_mts))
            .map(|v| format!("{v} MT/s"))
            .unwrap_or_default();
        info_row(ui, "Clock:", &clock, pal);
        // Unified memory is a wide on-package bus, not a DIMM channel count —
        // inferring "Single-Channel" from the one synthetic module would be
        // plainly wrong.
        let mode = if i.memory_unified {
            "Unified"
        } else {
            match i.memory_modules.len() {
                2 => "Dual-Channel",
                4 => "Quad-Channel",
                1 => "Single-Channel",
                _ => "",
            }
        };
        info_row(ui, "Mode:", mode, pal);
        info_row(ui, "Timings:", "", pal); // needs SPD — native engine

        ui.add_space(4.0);
        ui.label(RichText::new("Memory Modules").color(pal.text_dim).size(10.5));
        for m in &i.memory_modules {
            egui::Frame::new()
                .fill(pal.bg_header)
                .corner_radius(2)
                .inner_margin(egui::Margin::same(4))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!("{}: {} {}", m.bank, m.manufacturer, m.part_number))
                            .color(pal.text)
                            .size(10.5),
                    );
                    ui.label(
                        RichText::new(format!(
                            "{:.0} GB {}{}  {}",
                            m.capacity_gb,
                            m.memory_type,
                            // Unknown speed is left out, not shown as "@ 0".
                            m.configured_speed_mts
                                .or(m.speed_mts)
                                .map(|v| format!(" @ {v} MT/s"))
                                .unwrap_or_default(),
                            m.voltage_mv
                                .map(|v| format!("{:.2} V", v as f32 / 1000.0))
                                .unwrap_or_default(),
                        ))
                        .color(pal.text_dim)
                        .size(10.0),
                    );
                });
            ui.add_space(2.0);
        }
    });
}

fn gpu_os_drives_panels(ui: &mut egui::Ui, i: &crate::sysinfo::SystemInfo, tree: &[Hardware], pal: &Palette) {
    panel(ui, "GPU", pal, |ui| {
        for (gi, g) in i.gpus.iter().enumerate() {
            let vendor = if g.name.to_uppercase().contains("NVIDIA") {
                "NVIDIA"
            } else if g.name.to_uppercase().contains("AMD") || g.name.to_uppercase().contains("RADEON") {
                "RADEON"
            } else if g.name.to_uppercase().contains("INTEL") {
                "INTEL"
            } else if g.name.starts_with("Apple") {
                "APPLE"
            } else {
                "GPU"
            };
            ui.horizontal(|ui| {
                egui::Frame::new()
                    .fill(pal.bg_header)
                    .corner_radius(3)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.label(RichText::new(vendor).color(pal.ok_badge).size(12.0).strong());
                    });
                ui.vertical(|ui| {
                    ui.label(RichText::new(&g.name).color(pal.text).size(11.5).strong());
                    // NOTE: WMI AdapterRAM is a u32 capped at 4 GB — showing it
                    // would be wrong for modern cards, so Windows VRAM waits
                    // for the native GPU engine (NVML/ADL). An Intel Mac's
                    // registry reports the real figure.
                    let vram = if cfg!(windows) { None } else { g.vram_gb };
                    let detail = match (vram, g.driver_version.is_empty()) {
                        (Some(gb), true) => format!("{gb:.0} GB VRAM"),
                        (Some(gb), false) => format!("{gb:.0} GB VRAM · Driver {}", g.driver_version),
                        (None, _) => format!("Driver {}", g.driver_version),
                    };
                    ui.label(RichText::new(detail).color(pal.text_dim).size(10.0));
                });
            });
            if gi + 1 < i.gpus.len() {
                ui.add_space(3.0);
            }
        }
        ui.add_space(4.0);
        // Live GPU clocks from sensors.
        let (core, mem) = gpu_live_clocks(tree);
        info_row(
            ui,
            "GPU Clock:",
            &core.map(|v| format!("{v:.1} MHz")).unwrap_or_default(),
            pal,
        );
        info_row(
            ui,
            "Memory Clock:",
            &mem.map(|v| format!("{v:.1} MHz")).unwrap_or_default(),
            pal,
        );
        info_row(ui, "PCIe Link:", "", pal); // needs native engine
    });

    ui.add_space(6.0);

    if !i.displays.is_empty() {
        panel(ui, "Displays", pal, |ui| {
            for d in &i.displays {
                let mut title = d.name.clone();
                if let Some(inches) = d.diagonal_in {
                    title.push_str(&format!("  {inches:.1}\""));
                }
                ui.label(RichText::new(title).color(pal.text).size(11.0).strong());
                let mut detail = d.mode_text();
                if d.built_in {
                    detail.push_str("  · built-in");
                }
                if d.main {
                    detail.push_str("  · main");
                }
                ui.label(RichText::new(detail).color(pal.text_dim).size(10.0));
                ui.add_space(2.0);
            }
        });
        ui.add_space(6.0);
    }

    panel(ui, "Operating System", pal, |ui| {
        ui.label(
            RichText::new(format!("{} ({})", i.os.caption, i.os.arch))
                .color(pal.text)
                .size(11.0),
        );
        info_row(ui, "Build:", &i.os.build, pal);
        super::widgets::badge(ui, "UEFI Boot:", i.os.uefi_boot, pal);
        super::widgets::badge(ui, "Secure Boot:", i.os.secure_boot, pal);
        // macOS only; a "—" row on platforms without SIP would be noise.
        if i.os.sip_enabled.is_some() {
            super::widgets::badge(ui, "SIP:", i.os.sip_enabled, pal);
        }
    });

    ui.add_space(6.0);

    panel(ui, "Drives", pal, |ui| {
        for d in &i.drives {
            ui.label(
                RichText::new(format!(
                    "• {} [{}] {}",
                    d.model,
                    d.interface,
                    d.size_gb.map(|g| format!("{g:.0} GB")).unwrap_or_default()
                ))
                .color(pal.text)
                .size(10.5),
            );
        }
    });
}

fn battery_panel(ui: &mut egui::Ui, i: &crate::sysinfo::SystemInfo, pal: &Palette) {
    let Some(b) = &i.battery else { return };
    ui.add_space(6.0);
    panel(ui, "Battery", pal, |ui| {
        let capacity = match (b.full_charge_capacity_mah, b.design_capacity_mah) {
            (Some(full), Some(design)) => format!("{full} / {design} mAh"),
            _ => String::new(),
        };
        info_row(ui, "Full / Design:", &capacity, pal);
        info_row(ui, "Health:", &b.health_pct.map(|h| format!("{h:.0} %")).unwrap_or_default(), pal);
        let cycles = match (b.cycle_count, b.design_cycle_count) {
            (Some(c), Some(d)) => format!("{c} of {d}"),
            (Some(c), None) => c.to_string(),
            _ => String::new(),
        };
        info_row(ui, "Cycles:", &cycles, pal);
        info_row(ui, "Charge:", &b.charge_pct.map(|c| format!("{c:.0} %")).unwrap_or_default(), pal);
        info_row(ui, "Power:", &b.power_source_text(), pal);
    });
}

// ---- live-sensor helpers ------------------------------------------------

/// First CPU sensor of a type whose name contains `needle` (case-insensitive).
fn cpu_sensor(tree: &[Hardware], t: SensorType, needle: &str) -> Option<f32> {
    for hw in tree {
        if hw.hardware_type == HardwareType::Cpu {
            for s in &hw.sensors {
                if s.sensor_type == t && s.name.to_lowercase().contains(needle) {
                    return s.value;
                }
            }
        }
    }
    None
}

fn collect_cpu(tree: &[Hardware], t: SensorType, name_contains: &str) -> Vec<f32> {
    let mut out = Vec::new();
    for hw in tree {
        if hw.hardware_type == HardwareType::Cpu {
            for s in &hw.sensors {
                if s.sensor_type == t && s.name.to_lowercase().contains(name_contains) {
                    if let Some(v) = s.value {
                        out.push(v);
                    }
                }
            }
        }
    }
    out
}

fn gpu_live_clocks(tree: &[Hardware]) -> (Option<f32>, Option<f32>) {
    let mut core = None;
    let mut mem = None;
    for hw in tree {
        if matches!(
            hw.hardware_type,
            HardwareType::GpuNvidia
                | HardwareType::GpuAti
                | HardwareType::GpuIntel
                | HardwareType::GpuApple
        ) {
            for s in &hw.sensors {
                if s.sensor_type == SensorType::Clock {
                    let n = s.name.to_lowercase();
                    if n.contains("core") && core.is_none() {
                        core = s.value;
                    } else if n.contains("memory") && mem.is_none() {
                        mem = s.value;
                    }
                }
            }
        }
    }
    (core, mem)
}

fn mean(v: &[f32]) -> Option<f32> {
    if v.is_empty() {
        None
    } else {
        Some(v.iter().sum::<f32>() / v.len() as f32)
    }
}
