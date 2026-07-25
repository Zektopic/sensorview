//! Persistent application settings, HWiNFO-style.
//!
//! Backs the Settings dialog's "General / User Interface" tab. Stored as JSON
//! at `%APPDATA%\SensorView\settings.json` (platform equivalent elsewhere).

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Mirrors HWiNFO's Color Mode radio group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorMode {
    Grey,
    Black,
    Light,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    // Startup behavior (left checkbox column in HWiNFO's General tab).
    pub show_summary_on_startup: bool,
    pub show_sensors_on_startup: bool,
    pub minimize_main_on_startup: bool,
    pub minimize_sensors_on_startup: bool,
    pub minimize_sensors_instead_of_closing: bool,
    pub show_welcome_screen: bool,
    pub validate_window_positions: bool,
    pub auto_start: bool,
    pub automatic_update: bool,
    pub flush_buffers_on_start: bool,
    pub snapshot_cpu_polling: bool,
    pub shared_memory_support: bool,
    // Right checkbox column.
    pub wake_disabled_gpus: bool,
    pub poll_sleeping_gpus: bool,
    pub reorder_gpus: bool,
    pub prefer_amd_adl: bool,
    pub presentmon_support: bool,
    pub remember_preferences: bool,
    // Appearance.
    pub color_mode: ColorMode,
    pub language: String,
    // Polling.
    pub poll_interval_ms: u64,
    /// Slow lane: S.M.A.R.T., SPD and PCIe topology. Kept far slower than the
    /// sensor cadence on purpose — see `poll.rs`.
    pub inventory_interval_s: u64,
    // Embedded web dashboard.
    pub web_enabled: bool,
    /// Serve to other machines on the network. Off by default: the dashboard
    /// exposes drive serials and raw SPD/PCI dumps. Turning this on makes the
    /// access token mandatory.
    pub web_lan_access: bool,
    pub web_port: u16,
    // UI state carried across runs.
    pub collapsed_groups: BTreeSet<String>,
    pub font_scale: f32,
    pub sensor_col_width: f32,
    pub val_col_width: f32,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            show_summary_on_startup: true,
            show_sensors_on_startup: false,
            minimize_main_on_startup: false,
            minimize_sensors_on_startup: false,
            minimize_sensors_instead_of_closing: false,
            show_welcome_screen: true,
            validate_window_positions: true,
            auto_start: false,
            automatic_update: true,
            flush_buffers_on_start: true,
            snapshot_cpu_polling: false,
            shared_memory_support: false,
            wake_disabled_gpus: true,
            poll_sleeping_gpus: false,
            reorder_gpus: true,
            prefer_amd_adl: false,
            presentmon_support: true,
            remember_preferences: true,
            color_mode: ColorMode::Black,
            language: "English".to_string(),
            poll_interval_ms: 1000,
            inventory_interval_s: 30,
            web_enabled: true,
            web_lan_access: false,
            web_port: 8080,
            collapsed_groups: BTreeSet::new(),
            font_scale: 1.0,
            sensor_col_width: 220.0,
            val_col_width: 90.0,
        }
    }
}

impl AppSettings {
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("SensorView").join("settings.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            // Tolerate a UTF-8 BOM (hand-edited or PowerShell-written files).
            .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}
