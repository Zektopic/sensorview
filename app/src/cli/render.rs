//! Text and JSON rendering for the CLI. Pure functions over a telemetry frame,
//! so they are testable without starting the pipeline.

use crate::format::format_value;
use crate::model::{Hardware, Sensor};
use crate::state::TelemetryFrame;
use crate::sysinfo::SystemInfo;

/// Case-insensitive substring match against a sensor's name.
///
/// `needle` must already be lowercased by the caller — this runs once per
/// sensor per poll in `wait_for`, so it doesn't re-allocate.
pub fn matches(sensor: &Sensor, needle: &str) -> bool {
    sensor.name.to_lowercase().contains(needle)
}

/// First sensor whose name contains `needle` (already lowercased).
///
/// Exact name matches win over substring matches, so `get "GPU Core"` picks the
/// sensor actually called that rather than the first one it is a prefix of.
pub fn find_sensor<'a>(frame: &'a TelemetryFrame, needle: &str) -> Option<&'a Sensor> {
    fn walk<'a>(tree: &'a [Hardware], needle: &str, exact: bool) -> Option<&'a Sensor> {
        for hw in tree {
            for s in &hw.sensors {
                let name = s.name.to_lowercase();
                let hit = if exact { name == needle } else { name.contains(needle) };
                if hit {
                    return Some(s);
                }
            }
            if let Some(found) = walk(&hw.sub_hardware, needle, exact) {
                return Some(found);
            }
        }
        None
    }
    walk(&frame.tree, needle, true).or_else(|| walk(&frame.tree, needle, false))
}

/// One sensor, as a bare value / labelled line / JSON object.
pub fn one_sensor(sensor: &Sensor, json: bool, raw: bool) -> String {
    if json {
        return serde_json::to_string(sensor)
            .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
    }
    if raw {
        // Just the number, for `TEMP=$(sensorview get tdie --raw)`. An absent
        // reading prints nothing rather than an em dash, which would poison
        // any arithmetic downstream.
        return sensor.value.map(|v| v.to_string()).unwrap_or_default();
    }
    format!("{}: {}", sensor.name, format_value(sensor.value, sensor.sensor_type))
}

/// The whole tree as an indented, human-readable listing.
pub fn sensors_text(frame: &TelemetryFrame, filter: Option<&str>) -> String {
    let needle = filter.map(str::to_lowercase);
    let mut out = String::new();

    /// Does this node, or anything beneath it, have a sensor that matches?
    /// Checked before printing a heading — otherwise `--filter temp` emits a
    /// page of empty headings for every node that merely *has* children.
    fn has_match(hw: &Hardware, needle: Option<&String>) -> bool {
        hw.sensors.iter().any(|s| needle.is_none_or(|n| matches(s, n)))
            || hw.sub_hardware.iter().any(|c| has_match(c, needle))
    }

    fn walk(out: &mut String, tree: &[Hardware], needle: Option<&String>, depth: usize) {
        for hw in tree {
            if !has_match(hw, needle) {
                continue;
            }
            let shown: Vec<&Sensor> = hw
                .sensors
                .iter()
                .filter(|s| needle.is_none_or(|n| matches(s, n)))
                .collect();
            let indent = "  ".repeat(depth);
            out.push_str(&format!("{indent}{} [{:?}]\n", hw.name, hw.hardware_type));
            let width = shown.iter().map(|s| s.name.chars().count()).max().unwrap_or(0);
            for s in shown {
                out.push_str(&format!(
                    "{indent}  {:<width$}  {}\n",
                    s.name,
                    format_value(s.value, s.sensor_type),
                    width = width
                ));
            }
            walk(out, &hw.sub_hardware, needle, depth + 1);
        }
    }

    walk(&mut out, &frame.tree, needle.as_ref(), 0);
    if out.is_empty() {
        return match filter {
            Some(f) => format!("(no sensors matching {f:?})"),
            None => "(no sensors)".to_string(),
        };
    }
    out.trim_end().to_string()
}

/// The tree as a flat JSON array — easier to consume with `jq` than the nested
/// frame, which is what `stream --format ndjson` emits.
pub fn sensors_json(frame: &TelemetryFrame, filter: Option<&str>) -> String {
    let needle = filter.map(str::to_lowercase);
    let mut flat: Vec<serde_json::Value> = Vec::new();

    fn walk(
        out: &mut Vec<serde_json::Value>,
        tree: &[Hardware],
        needle: Option<&String>,
        path: &str,
    ) {
        for hw in tree {
            let hw_path = if path.is_empty() {
                hw.name.clone()
            } else {
                format!("{path}/{}", hw.name)
            };
            for s in &hw.sensors {
                if needle.is_some_and(|n| !matches(s, n)) {
                    continue;
                }
                out.push(serde_json::json!({
                    "hardware": hw_path,
                    "identifier": s.identifier,
                    "name": s.name,
                    "type": s.sensor_type,
                    "unit": s.sensor_type.unit(),
                    "value": s.value,
                    "min": s.min,
                    "max": s.max,
                    "avg": s.avg,
                }));
            }
            walk(out, &hw.sub_hardware, needle, &hw_path);
        }
    }

    walk(&mut flat, &frame.tree, needle.as_ref(), "");
    serde_json::to_string_pretty(&flat).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

/// Static system information, formatted like the System Summary window.
pub fn info_text(i: &SystemInfo) -> String {
    // A free fn rather than a closure: a closure capturing `out` would hold a
    // mutable borrow for the whole body, blocking the section headers.
    fn row(out: &mut String, k: &str, v: &str) {
        if !v.is_empty() {
            // Labels are mostly short, but display and memory-bank names are
            // data; keep at least one space between label and value.
            let width = 16.max(k.chars().count() + 1);
            out.push_str(&format!("  {k:<width$}{v}\n"));
        }
    }
    let mut out = String::new();
    macro_rules! row {
        ($k:expr, $v:expr) => {
            row(&mut out, $k, $v)
        };
    }

    let yes_no = |v: Option<bool>| match v {
        Some(true) => "Enabled".to_string(),
        Some(false) => "Disabled".to_string(),
        None => String::new(),
    };

    out.push_str("System\n");
    row!("Computer", &i.computer_name);
    row!("User", &i.user_name);
    row!("OS", &i.os.caption);
    row!("Build", &i.os.build);
    row!("Architecture", &i.os.arch);
    row!("UEFI Boot", &yes_no(i.os.uefi_boot));
    row!("Secure Boot", &yes_no(i.os.secure_boot));
    row!("SIP", &yes_no(i.os.sip_enabled));

    out.push_str("\nCPU\n");
    row!("Name", &i.cpu.name);
    if let Some(c) = i.cpu.cores {
        let threads = i.cpu.threads.unwrap_or(c);
        row!("Cores / Threads", &format!("{c} / {threads}"));
    }
    if let Some(layout) = &i.cpu.socket {
        row!("Layout", layout);
    }
    match (i.cpu.base_clock_mhz, i.cpu.max_clock_mhz) {
        (Some(base), Some(max)) if max > base => row!("Clock", &format!("{base}–{max} MHz")),
        (Some(base), _) => row!("Clock", &format!("{base} MHz")),
        _ => {}
    }
    row!("Codename", &i.cpu.codename);
    row!("CPUID", &i.cpu.cpuid);
    row!("Vendor", &i.cpu.vendor);
    if let Some(kb) = i.cpu.l2_kb {
        row!("L2 Cache", &crate::format::cache_size(kb));
    }
    if let Some(kb) = i.cpu.l3_kb {
        row!("L3 Cache", &crate::format::cache_size(kb));
    }
    let features: Vec<&str> = i.cpu.features.iter().filter(|f| f.1).map(|f| f.0).collect();
    row!("Features", &features.join(" "));

    out.push_str("\nMotherboard\n");
    row!("Product", format!("{} {}", i.board.manufacturer, i.board.product).trim());
    row!("Model", &i.board.model_id);
    row!("Board ID", &i.board.board_id);
    row!("Chipset", &i.board.chipset);
    row!("Firmware", &i.board.bios_version);
    row!("Firmware Date", &i.board.bios_date);

    out.push_str("\nMemory\n");
    if let Some(gb) = i.total_memory_gb {
        let unified = if i.memory_unified { " (unified)" } else { "" };
        row!("Total", &format!("{gb:.0} GB{unified}"));
    }
    for m in &i.memory_modules {
        let speed = m
            .configured_speed_mts
            .or(m.speed_mts)
            .map(|v| format!(" @ {v} MT/s"))
            .unwrap_or_default();
        let maker = [m.manufacturer.as_str(), m.part_number.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        let maker = if maker.is_empty() { String::new() } else { format!(" — {maker}") };
        row!(&m.bank, &format!("{:.0} GB {}{speed}{maker}", m.capacity_gb, m.memory_type));
    }

    if !i.gpus.is_empty() {
        out.push_str("\nGPU\n");
        for g in &i.gpus {
            row!("Name", &g.name);
            if let Some(vram) = g.vram_gb {
                row!("VRAM", &format!("{vram:.0} GB"));
            }
            row!("Driver", &g.driver_version);
        }
    }

    if !i.displays.is_empty() {
        out.push_str("\nDisplays\n");
        for d in &i.displays {
            let mut desc = d.mode_text();
            if let Some(inches) = d.diagonal_in {
                desc.push_str(&format!(", {inches:.1}\""));
            }
            let tags: Vec<&str> =
                [(d.built_in, "built-in"), (d.main, "main")].iter().filter(|t| t.0).map(|t| t.1).collect();
            if !tags.is_empty() {
                desc.push_str(&format!(" [{}]", tags.join(", ")));
            }
            row!(&d.name, &desc);
        }
    }

    if !i.drives.is_empty() {
        out.push_str("\nDrives\n");
        for d in &i.drives {
            let size = d.size_gb.map(|g| format!(" {g:.0} GB")).unwrap_or_default();
            row!("Drive", &format!("{} [{}]{size}", d.model, d.interface));
        }
    }

    if let Some(b) = &i.battery {
        out.push_str("\nBattery\n");
        if let (Some(full), Some(design)) = (b.full_charge_capacity_mah, b.design_capacity_mah) {
            row!("Capacity", &format!("{full} / {design} mAh"));
        }
        if let Some(h) = b.health_pct {
            row!("Health", &format!("{h:.0} %"));
        }
        match (b.cycle_count, b.design_cycle_count) {
            (Some(c), Some(d)) => row!("Cycles", &format!("{c} of {d}")),
            (Some(c), None) => row!("Cycles", &c.to_string()),
            _ => {}
        }
        if let Some(pct) = b.charge_pct {
            row!("Charge", &format!("{pct:.0} %"));
        }
        row!("Power", &b.power_source_text());
        row!("Gauge", &b.gauge);
    }

    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HardwareType, SensorType};

    fn s(name: &str, ty: SensorType, value: Option<f32>) -> Sensor {
        Sensor {
            identifier: format!("/x/{name}"),
            name: name.into(),
            sensor_type: ty,
            index: 0,
            value,
            min: None,
            max: None,
            avg: None,
        }
    }

    fn frame() -> TelemetryFrame {
        TelemetryFrame {
            seq: 2,
            tree: vec![Hardware {
                identifier: "/cpu/0".into(),
                name: "Apple M5".into(),
                hardware_type: HardwareType::Cpu,
                sensors: vec![
                    s("CPU Package Power", SensorType::Power, Some(21.5)),
                    s("PMU tdie6", SensorType::Temperature, Some(38.77)),
                    s("Quiet Sensor", SensorType::Temperature, None),
                ],
                sub_hardware: vec![Hardware {
                    identifier: "/gpu/0".into(),
                    name: "GPU".into(),
                    hardware_type: HardwareType::GpuApple,
                    sensors: vec![s("GPU Core", SensorType::Load, Some(9.0))],
                    sub_hardware: Vec::new(),
                }],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn exact_name_wins_over_substring() {
        let mut f = frame();
        // "GPU Core" is also a substring of "GPU Core Clock".
        f.tree[0].sub_hardware[0]
            .sensors
            .insert(0, s("GPU Core Clock", SensorType::Clock, Some(338.0)));
        let found = find_sensor(&f, "gpu core").expect("match");
        assert_eq!(found.name, "GPU Core", "exact match must beat the substring");
    }

    #[test]
    fn find_recurses_into_sub_hardware() {
        assert_eq!(find_sensor(&frame(), "gpu core").unwrap().name, "GPU Core");
        assert!(find_sensor(&frame(), "nonexistent").is_none());
    }

    /// `--raw` feeds shell arithmetic, so an absent reading must be empty, not
    /// an em dash.
    #[test]
    fn raw_output_is_bare_and_empty_when_unread() {
        let hot = s("t", SensorType::Temperature, Some(38.5));
        assert_eq!(one_sensor(&hot, false, true), "38.5");
        let quiet = s("t", SensorType::Temperature, None);
        assert_eq!(one_sensor(&quiet, false, true), "");
        // The labelled form still shows the em dash.
        assert_eq!(one_sensor(&quiet, false, false), "t: —");
    }

    #[test]
    fn filter_is_case_insensitive_and_hides_empty_nodes() {
        let out = sensors_text(&frame(), Some("POWER"));
        assert!(out.contains("CPU Package Power"), "{out}");
        assert!(!out.contains("PMU tdie6"), "{out}");
        // The GPU node has no matching sensor and no matching descendants.
        assert!(!out.contains("GPU Core"), "{out}");
    }

    #[test]
    fn no_matches_says_so_rather_than_printing_nothing() {
        let out = sensors_text(&frame(), Some("zzz"));
        assert!(out.contains("no sensors matching"), "{out}");
    }

    #[test]
    fn json_is_a_flat_array_with_units() {
        let out = sensors_json(&frame(), Some("package"));
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "CPU Package Power");
        assert_eq!(arr[0]["unit"], "W");
        assert_eq!(arr[0]["value"], 21.5);
        // Nested position is preserved as a path, not lost by flattening.
        assert_eq!(arr[0]["hardware"], "Apple M5");
    }

    #[test]
    fn unread_sensors_serialise_as_null_not_zero() {
        let out = sensors_json(&frame(), Some("quiet"));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v[0]["value"].is_null(), "absent reading must not become 0");
    }
}
