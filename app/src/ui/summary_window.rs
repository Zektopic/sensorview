//! HWiNFO-style "System Summary" window: CPU / Motherboard / Memory / GPU /
//! OS / Drives panel grid with the ISA features chip-grid and an Operating
//! Point table fed by live sensors.

use eframe::egui::{self, Color32, RichText};

use super::widgets::{chip, info_row, panel};
use super::{Palette, Shared};
use crate::model::storage::{HealthStatus, StorageHealth, StorageProtocol};
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
                    // ---- Motherboard + Memory ------------------------------
                    board_memory_panels(&mut cols[1], &i, &pal);
                    // ---- GPU + OS + Drives ---------------------------------
                    gpu_os_drives_panels(&mut cols[2], &i, tree, frame.storage(), &pal);
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
        info_row(
            ui,
            "L2 Cache:",
            &i.cpu.l2_kb.map(|k| format!("{} KB", k)).unwrap_or_default(),
            pal,
        );
        info_row(
            ui,
            "L3 Cache:",
            &i.cpu.l3_kb.map(|k| format!("{} MB", k / 1024)).unwrap_or_default(),
            pal,
        );
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
        info_row(ui, "Chipset:", "", pal); // needs PCI enum — native engine
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
            .map(|m| format!("{} SDRAM", m.memory_type))
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
        let unified = i
            .memory_modules
            .first()
            .is_some_and(|m| m.memory_type.contains("on-package"));
        let mode = if unified {
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
                            "{:.0} GB {} @ {} MT/s  {}",
                            m.capacity_gb,
                            m.memory_type,
                            m.configured_speed_mts.or(m.speed_mts).unwrap_or(0),
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

fn gpu_os_drives_panels(
    ui: &mut egui::Ui,
    i: &crate::sysinfo::SystemInfo,
    tree: &[Hardware],
    frame_storage: &[StorageHealth],
    pal: &Palette,
) {
    panel(ui, "GPU", pal, |ui| {
        for (gi, g) in i.gpus.iter().enumerate() {
            let vendor = if g.name.to_uppercase().contains("NVIDIA") {
                "NVIDIA"
            } else if g.name.to_uppercase().contains("AMD") || g.name.to_uppercase().contains("RADEON") {
                "RADEON"
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
                    // would be wrong for modern cards. VRAM comes with the
                    // native GPU engine (NVML/ADL).
                    ui.label(
                        RichText::new(format!("Driver {}", g.driver_version))
                            .color(pal.text_dim)
                            .size(10.0),
                    );
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

    panel(ui, "Operating System", pal, |ui| {
        ui.label(
            RichText::new(format!("{} ({})", i.os.caption, i.os.arch))
                .color(pal.text)
                .size(11.0),
        );
        info_row(ui, "Build:", &i.os.build, pal);
        super::widgets::badge(ui, "UEFI Boot:", i.os.uefi_boot, pal);
        super::widgets::badge(ui, "Secure Boot:", i.os.secure_boot, pal);
    });

    ui.add_space(6.0);

    panel(ui, "Drives", pal, |ui| {
        // Health, where a backend could read it. Falls back to the plain
        // WMI/inventory listing for drives S.M.A.R.T. could not be read from,
        // so a machine with one unreadable disk still lists all of them.
        let health = frame_storage;
        for d in &i.drives {
            let matched = health.iter().find(|h| {
                // Model strings differ in padding and case between WMI and the
                // drive's own identity response, so compare loosely.
                let a = h.model.to_lowercase();
                let b = d.model.to_lowercase();
                !a.is_empty() && (a.contains(b.trim()) || b.contains(a.trim()))
            });
            match matched {
                Some(h) => drive_health_block(ui, h, pal),
                None => {
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
            }
        }

        // Drives the sensor backend saw but WMI did not enumerate.
        for h in health {
            let known = i.drives.iter().any(|d| {
                let a = h.model.to_lowercase();
                let b = d.model.to_lowercase();
                !a.is_empty() && (a.contains(b.trim()) || b.contains(a.trim()))
            });
            if !known {
                drive_health_block(ui, h, pal);
            }
        }

        if i.drives.is_empty() && health.is_empty() {
            ui.label(
                RichText::new("No drives enumerated — S.M.A.R.T. needs administrator rights")
                    .color(pal.text_dim)
                    .size(10.5),
            );
        }
    });
}

/// One drive: identity, the health headline, and its S.M.A.R.T. table.
fn drive_health_block(ui: &mut egui::Ui, h: &StorageHealth, pal: &Palette) {
    let (label, color) = match h.status {
        HealthStatus::Good => ("Good", pal.ok_badge),
        HealthStatus::Caution => ("Caution", pal.warn),
        HealthStatus::Bad => ("Bad", pal.crit),
        // Never "Good" by default: not being able to read a drive's health is
        // a different fact from the drive being healthy.
        HealthStatus::Unknown => ("Unknown", pal.text_dim),
    };

    egui::Frame::new()
        .fill(pal.bg_header)
        .corner_radius(3)
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&h.model).color(pal.text).size(11.5).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::Frame::new()
                        .fill(color)
                        .corner_radius(2)
                        .inner_margin(egui::Margin::symmetric(6, 1))
                        .show(ui, |ui| {
                            ui.label(RichText::new(label).color(Color32::WHITE).size(10.0).strong());
                        });
                });
            });

            let proto = match h.protocol {
                StorageProtocol::Nvme => "NVMe",
                StorageProtocol::Ata => "SATA / ATA",
                StorageProtocol::Scsi => "SCSI",
            };
            info_row(ui, "Interface:", proto, pal);
            info_row(
                ui,
                "Capacity:",
                &h.capacity_bytes
                    .map(|b| format!("{:.0} GB", b as f64 / 1e9))
                    .unwrap_or_default(),
                pal,
            );
            info_row(ui, "Firmware:", &h.firmware, pal);
            info_row(ui, "Serial:", &h.serial, pal);
            info_row(
                ui,
                "Temperature:",
                &h.temperature_c.map(|t| format!("{t:.0} °C")).unwrap_or_default(),
                pal,
            );
            info_row(
                ui,
                "Power-On Hours:",
                &h.power_on_hours
                    .map(|v| format!("{v} h  ({:.1} y)", v as f64 / 8760.0))
                    .unwrap_or_default(),
                pal,
            );
            info_row(
                ui,
                "Power Cycles:",
                &h.power_cycles.map(|v| v.to_string()).unwrap_or_default(),
                pal,
            );
            info_row(
                ui,
                "Health / Life:",
                &h.life_remaining_pct
                    .map(|v| format!("{v:.0} %"))
                    .unwrap_or_default(),
                pal,
            );
            info_row(ui, "Host Writes:", &tb(h.total_bytes_written), pal);
            info_row(ui, "Host Reads:", &tb(h.total_bytes_read), pal);

            for w in &h.warnings {
                ui.label(RichText::new(format!("⚠ {w}")).color(pal.warn).size(10.0));
            }

            if !h.attributes.is_empty() {
                // Collapsed by default: the attribute table is long, and the
                // headline above is what most people came for.
                egui::CollapsingHeader::new(
                    RichText::new(format!("S.M.A.R.T. attributes ({})", h.attributes.len()))
                        .size(10.5)
                        .color(pal.text_dim),
                )
                .id_salt(&h.identifier)
                .show(ui, |ui| smart_table(ui, h, pal));
            }
        });
    ui.add_space(4.0);
}

/// The attribute table, in the column order every S.M.A.R.T. tool uses.
fn smart_table(ui: &mut egui::Ui, h: &StorageHealth, pal: &Palette) {
    egui::Grid::new(format!("smart_{}", h.identifier))
        .num_columns(6)
        .spacing([10.0, 2.0])
        .striped(true)
        .show(ui, |ui| {
            for (t, w) in [
                ("ID", 26.0),
                ("Attribute", 150.0),
                ("Cur", 26.0),
                ("Wst", 26.0),
                ("Thr", 26.0),
                ("Raw", 60.0),
            ] {
                ui.scope(|ui| {
                    ui.set_min_width(w);
                    ui.label(RichText::new(t).size(9.5).strong().color(pal.text_dim));
                });
            }
            ui.end_row();

            for a in &h.attributes {
                // A failing attribute is the single most important thing on
                // this screen, so the whole row takes the critical colour.
                let c = if a.failing() { pal.crit } else { pal.value };
                ui.label(RichText::new(format!("{:02X}", a.id)).size(9.5).monospace().color(pal.text_dim));
                ui.label(RichText::new(&a.name).size(9.5).color(if a.failing() { pal.crit } else { pal.text }));
                ui.label(RichText::new(a.current.to_string()).size(9.5).monospace().color(c));
                ui.label(RichText::new(a.worst.to_string()).size(9.5).monospace().color(c));
                ui.label(
                    RichText::new(if a.threshold == 0 { "—".into() } else { a.threshold.to_string() })
                        .size(9.5)
                        .monospace()
                        .color(pal.text_dim),
                );
                ui.label(RichText::new(a.raw.to_string()).size(9.5).monospace().color(c));
                ui.end_row();
            }
        });
}

/// Bytes as TB/GB. `None` stays an em dash — an unread counter is not zero.
fn tb(v: Option<u128>) -> String {
    match v {
        None => String::new(),
        Some(b) if b >= 1_000_000_000_000 => format!("{:.2} TB", b as f64 / 1e12),
        Some(b) => format!("{:.0} GB", b as f64 / 1e9),
    }
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
