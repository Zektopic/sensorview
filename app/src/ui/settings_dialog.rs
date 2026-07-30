//! HWiNFO-style Settings dialog: tab strip with a fully functional
//! "General / User Interface" tab persisted via [`crate::settings`].

use eframe::egui::{self, Id, RichText};

use super::widgets::square_check;
use super::{Palette, Shared, WindowFlags};
use crate::settings::ColorMode;

#[derive(Clone, Copy, PartialEq, Default)]
enum Tab {
    #[default]
    General,
    Safety,
    Smbus,
    Driver,
    Remote,
    License,
}

pub fn show(ui: &mut egui::Ui, s: &Shared) {
    super::handle_close(ui, &s.windows.settings);
    let pal = s.palette();

    let tab_id = Id::new("settings_tab");
    let mut tab: Tab = ui.ctx().data_mut(|d| *d.get_temp_mut_or(tab_id, default_tab()));

    // ---- Bottom OK / Cancel ---------------------------------------------
    egui::Panel::bottom("settings_buttons")
        .frame(
            egui::Frame::new()
                .fill(pal.bg)
                .inner_margin(egui::Margin::symmetric(10, 6)),
        )
        .show(ui, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    WindowFlags::close(&s.windows.settings);
                }
                if ui.button("OK").clicked() {
                    if let Ok(st) = s.settings.read() {
                        st.save();
                    }
                    WindowFlags::close(&s.windows.settings);
                }
            });
        });

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(pal.bg)
                .inner_margin(egui::Margin::same(8)),
        )
        .show(ui, |ui| {
            // ---- Tab strip ---------------------------------------------
            ui.horizontal(|ui| {
                for (t, label) in [
                    (Tab::General, "General / User Interface"),
                    (Tab::Safety, "Safety"),
                    (Tab::Smbus, "SMBus / I2C"),
                    (Tab::Driver, "Driver Management"),
                    (Tab::Remote, "Remote Access"),
                    (Tab::License, "License Management"),
                ] {
                    let active = tab == t;
                    let text = RichText::new(label).size(11.0).color(if active {
                        pal.text
                    } else {
                        pal.text_dim
                    });
                    if ui.selectable_label(active, text).clicked() {
                        tab = t;
                        ui.ctx().data_mut(|d| d.insert_temp(tab_id, t));
                    }
                }
            });
            ui.separator();

            match tab {
                Tab::General => general_tab(ui, s, &pal),
                Tab::Safety => stub_tab(ui, &pal, "Safety options (watchdog, polling exclusions) arrive with the native sensor engine."),
                Tab::Smbus => stub_tab(ui, &pal, "SMBus / I2C device scanning arrives with the native sensor engine (SPD, Super-I/O)."),
                Tab::Driver => driver_tab(ui, s, &pal),
                Tab::Remote => remote_tab(ui, s, &pal),
                Tab::License => stub_tab(ui, &pal, "SensorView is open source — no license management needed."),
            }
        });
}

/// Which tab the dialog opens on. `SENSORVIEW_SETTINGS_TAB` is a dev/testing
/// affordance (matching `SENSORVIEW_SHOW_SETTINGS`) so a tab that normally
/// needs a click can be rendered from a script; unset, it is always General.
fn default_tab() -> Tab {
    match std::env::var("SENSORVIEW_SETTINGS_TAB").as_deref() {
        Ok("safety") => Tab::Safety,
        Ok("smbus") => Tab::Smbus,
        Ok("driver") => Tab::Driver,
        Ok("remote") => Tab::Remote,
        Ok("license") => Tab::License,
        _ => Tab::General,
    }
}

fn general_tab(ui: &mut egui::Ui, s: &Shared, pal: &Palette) {
    let Ok(mut st) = s.settings.write() else { return };
    let mut changed = false;

    ui.columns(2, |cols| {
        let c = &mut cols[0];
        changed |= square_check(c, &mut st.show_summary_on_startup, "Show System Summary on Startup", pal);
        changed |= square_check(c, &mut st.show_sensors_on_startup, "Show Sensors on Startup", pal);
        changed |= square_check(c, &mut st.minimize_main_on_startup, "Minimize Main Window on Startup", pal);
        changed |= square_check(c, &mut st.minimize_sensors_on_startup, "Minimize Sensors on Startup", pal);
        changed |= square_check(c, &mut st.minimize_sensors_instead_of_closing, "Minimize Sensors instead of closing", pal);
        changed |= square_check(c, &mut st.show_welcome_screen, "Show Welcome Screen and Progress", pal);
        changed |= square_check(c, &mut st.validate_window_positions, "Validate Window Positions", pal);
        changed |= square_check(c, &mut st.auto_start, "Auto Start", pal);
        changed |= square_check(c, &mut st.automatic_update, "Automatic Update", pal);
        changed |= square_check(c, &mut st.flush_buffers_on_start, "Flush Buffers on Start", pal);
        changed |= square_check(c, &mut st.snapshot_cpu_polling, "Snapshot CPU Polling", pal);
        changed |= square_check(c, &mut st.shared_memory_support, "Shared Memory Support", pal);

        c.add_space(8.0);
        c.label(RichText::new("Sensor polling interval:").size(11.0).color(pal.text_dim));
        let mut secs = st.poll_interval_ms as f32 / 1000.0;
        // The floor is a hardware constraint, not taste: faster than ~250 ms and
        // Super-I/O / SMBus reads start colliding with firmware access.
        if c.add(egui::Slider::new(&mut secs, 0.25..=10.0).suffix(" s").fixed_decimals(2)).changed() {
            st.poll_interval_ms = (secs * 1000.0) as u64;
            // Takes effect on the next tick — no restart, no lock held.
            s.command(crate::poll::Command::SetInterval(
                std::time::Duration::from_millis(st.poll_interval_ms),
            ));
            changed = true;
        }

        c.add_space(8.0);
        c.label(RichText::new("Language:").size(11.0).color(pal.text_dim));
        egui::ComboBox::from_id_salt("lang")
            .selected_text(st.language.clone())
            .show_ui(c, |ui| {
                ui.selectable_value(&mut st.language, "English".to_string(), "English");
            });

        let c = &mut cols[1];
        changed |= square_check(c, &mut st.wake_disabled_gpus, "Wake disabled GPUs", pal);
        changed |= square_check(c, &mut st.poll_sleeping_gpus, "Poll Sleeping GPUs", pal);
        changed |= square_check(c, &mut st.reorder_gpus, "Reorder GPUs", pal);
        changed |= square_check(c, &mut st.prefer_amd_adl, "Prefer AMD ADL", pal);
        changed |= square_check(c, &mut st.presentmon_support, "PresentMon Support", pal);
        changed |= square_check(c, &mut st.remember_preferences, "Remember Preferences", pal);

        c.add_space(10.0);
        c.group(|c| {
            c.label(RichText::new("Color Mode").size(11.0).color(pal.text_dim));
            for (mode, label) in [
                (ColorMode::Grey, "Default (Grey)"),
                (ColorMode::Black, "Default (Black)"),
                (ColorMode::Light, "Disabled (Light)"),
            ] {
                if c.radio(st.color_mode == mode, label).clicked() {
                    st.color_mode = mode;
                    changed = true;
                }
            }
        });

        c.add_space(6.0);
        c.horizontal(|c| {
            if c.button("Backup User Settings").clicked() {
                st.save();
            }
            if c.button("Reset Preferences").clicked() {
                *st = crate::settings::AppSettings::default();
                changed = true;
            }
        });
    });

    if changed {
        st.save();
    }
}

/// "Remote Access" — the embedded dashboard's status and exposure controls.
///
/// The bind address and port are read when the server starts, so changes here
/// are labelled as needing a restart rather than silently doing nothing.
#[cfg(feature = "web")]
fn remote_tab(ui: &mut egui::Ui, s: &Shared, pal: &Palette) {
    ui.add_space(6.0);
    ui.label(RichText::new("Web Dashboard").color(pal.accent).strong());

    match (&s.web.url, &s.web.error) {
        (Some(url), _) => {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Address:").size(11.5).color(pal.text_dim));
                ui.hyperlink_to(RichText::new(url).size(11.5), url);
            });
            if s.web.lan {
                ui.label(
                    RichText::new(
                        "Serving on all interfaces — replace 127.0.0.1 with this machine's \
                         LAN address to open the dashboard from a phone or another PC.",
                    )
                    .size(11.0)
                    .color(pal.text_dim),
                );
            } else {
                ui.label(
                    RichText::new("Loopback only — reachable from this machine.")
                        .size(11.0)
                        .color(pal.text_dim),
                );
            }
        }
        (None, Some(err)) => {
            ui.label(RichText::new(format!("⚠ Not running: {err}")).size(11.5).color(pal.crit));
        }
        (None, None) => {
            ui.label(RichText::new("Disabled.").size(11.5).color(pal.text_dim));
        }
    }

    if let Some(token) = &s.web.token {
        ui.add_space(6.0);
        ui.label(RichText::new("Access token").color(pal.accent).strong());
        ui.label(
            RichText::new(
                "Required because the dashboard is exposed on the network. It is generated \
                 per run and never stored, so restarting invalidates it.",
            )
            .size(11.0)
            .color(pal.text_dim),
        );
        ui.horizontal(|ui| {
            ui.label(RichText::new(token).size(11.5).monospace().color(pal.value));
            if ui.button(RichText::new("Copy").size(11.0)).clicked() {
                ui.ctx().copy_text(token.clone());
            }
        });
    }

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);

    let Ok(mut st) = s.settings.write() else { return };
    let mut changed = false;
    changed |= square_check(ui, &mut st.web_enabled, "Enable web dashboard", pal);
    changed |= square_check(
        ui,
        &mut st.web_lan_access,
        "Allow access from other devices on the network",
        pal,
    );
    if st.web_lan_access {
        ui.label(
            RichText::new(
                "⚠ The dashboard exposes sensor readings, drive serial numbers and raw \
                 SPD / PCI configuration dumps. Anyone with the token and network access \
                 can read them.",
            )
            .size(11.0)
            .color(pal.warn),
        );
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new("Port:").size(11.0).color(pal.text_dim));
        let mut port = st.web_port;
        if ui.add(egui::DragValue::new(&mut port).range(1024..=65535)).changed() {
            st.web_port = port;
            changed = true;
        }
    });
    ui.label(
        RichText::new("Changes to network access and port take effect on the next start.")
            .size(11.0)
            .color(pal.text_dim),
    );

    if changed {
        st.save();
    }
}

/// Without the `web` feature there is no server to configure.
#[cfg(not(feature = "web"))]
fn remote_tab(ui: &mut egui::Ui, _s: &Shared, pal: &Palette) {
    stub_tab(ui, pal, "This build was compiled without the web dashboard.");
}

fn driver_tab(ui: &mut egui::Ui, s: &Shared, pal: &Palette) {
    let frame = s.frame();
    let (source, diag) = (frame.source.clone(), frame.diagnostics.clone());
    // App-token elevation is authoritative (independent of sidecar version).
    #[cfg_attr(target_os = "macos", allow(unused_variables))]
    let elevated = s.elevated;

    ui.add_space(6.0);
    ui.label(RichText::new("Sensor Engine").color(pal.accent).strong());
    ui.label(RichText::new(format!("Active source: {source}")).size(11.5).color(pal.text));
    if !diag.engine_version.is_empty() {
        ui.label(RichText::new(&diag.engine_version).size(11.0).color(pal.text_dim));
    }

    // macOS reads every sensor through IOKit with no kernel driver and no
    // elevation, so the entire WinRing0/PawnIO apparatus below is meaningless
    // there — and a button that silently does nothing is worse than no button.
    #[cfg(target_os = "macos")]
    {
        ui.add_space(6.0);
        ui.label(
            RichText::new("✓ No kernel driver required — sensors are read directly via IOKit.")
                .size(11.0)
                .color(pal.ok_badge),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Temperatures come from the IOHIDEventSystem sensor plane and power from \
                 IOReport. Both are unprivileged, so SensorView never needs to run as root.",
            )
            .size(11.0)
            .color(pal.text_dim),
        );
        driver_report_details(ui, pal, &diag);
        return;
    }

    // Elevation status badge.
    #[allow(unreachable_code)]
    {
    ui.add_space(4.0);
    super::widgets::badge(ui, "Running as Administrator:", elevated, pal);

    // Guidance depends on what's actually wrong.
    ui.add_space(6.0);
    let blocked = diag.driver_report.to_lowercase().contains("blocked")
        || diag.driver_report.to_lowercase().contains("not signed")
        || diag.driver_report.to_lowercase().contains("failed to load");
    if elevated == Some(false) {
        ui.label(
            RichText::new(
                "⚠ Not elevated. CPU package/core power, effective clocks, Tctl/Tdie and \
                 fan/voltage sensors need Administrator rights. The release build elevates \
                 automatically at launch — or right-click SensorView → Run as administrator.",
            )
            .size(11.0)
            .color(pal.warn),
        );
    } else if blocked {
        ui.label(
            RichText::new(
                "⚠ The kernel driver was blocked from loading. On Windows 11 the \
                 vulnerable-driver blocklist (and Memory Integrity / HVCI) can block the \
                 classic WinRing0 driver. Installing PawnIO (a signed, blocklist-clean \
                 driver LibreHardwareMonitor can use) restores full sensor access.",
            )
            .size(11.0)
            .color(pal.warn),
        );
        ui.hyperlink_to("Get PawnIO", "https://pawnio.eu/");
    } else if elevated == Some(true) {
        ui.label(
            RichText::new("✓ Elevated and the kernel driver is available — full sensor access.")
                .size(11.0)
                .color(pal.ok_badge),
        );
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui
            .button(RichText::new("⚡ Install / Reinstall PawnIO Driver").color(pal.accent))
            .clicked()
        {
            let setup_path = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("resources").join("PawnIO_setup.exe")))
                .unwrap_or_else(|| std::path::PathBuf::from("resources/PawnIO_setup.exe"));
            let setup = if setup_path.exists() {
                setup_path
            } else {
                std::path::PathBuf::from("resources/PawnIO_setup.exe")
            };
            if setup.exists() {
                // The path is interpolated into a PowerShell single-quoted
                // string, and this launches elevated (-Verb RunAs). A quote in
                // the install path — `C:\Users\O'Brien\...` is legal — would
                // otherwise close the literal and let the rest be parsed as
                // commands. PowerShell escapes a single quote by doubling it.
                let quoted = setup.display().to_string().replace('\'', "''");
                let _ = std::process::Command::new("powershell")
                    .args([
                        "-Command",
                        &format!("Start-Process -FilePath '{quoted}' -Verb RunAs"),
                    ])
                    .spawn();
            } else {
                let _ = std::process::Command::new("powershell")
                    .args(["-Command", "Start-Process 'https://pawnio.eu/'"])
                    .spawn();
            }
        }
        ui.hyperlink_to("PawnIO Website", "https://pawnio.eu/");
    });

    driver_report_details(ui, pal, &diag);
    }
}

/// The collapsible raw backend report. Shared so the macOS branch above can
/// show it without duplicating the widget.
fn driver_report_details(ui: &mut egui::Ui, pal: &Palette, diag: &crate::source::Diagnostics) {
    if diag.driver_report.is_empty() || diag.driver_report == "(no ring0 section in report)" {
        return;
    }
    let title = if cfg!(target_os = "macos") { "Sensor backend report" } else { "Kernel driver report" };
    ui.add_space(8.0);
    ui.collapsing(RichText::new(title).size(11.0).color(pal.text_dim), |ui| {
        egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
            ui.label(
                RichText::new(&diag.driver_report).size(10.0).monospace().color(pal.text_dim),
            );
        });
    });
}

fn stub_tab(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(text).size(11.5).color(pal.text_dim));
}
