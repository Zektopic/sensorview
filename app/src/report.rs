//! Text report export — the Main window's "Save Report" action.
//! Mirrors the idea of OpenHardwareMonitor's GUI/ReportForm.cs.

use std::path::PathBuf;

use crate::model::Hardware;
use crate::sysinfo::SystemInfo;
use crate::format::format_value;

pub fn write_report(tree: &[Hardware], info: Option<&SystemInfo>) -> Result<PathBuf, String> {
    let mut out = String::new();
    out.push_str("SensorView Report\n");
    out.push_str(&format!("Version: {}\n", env!("CARGO_PKG_VERSION")));
    out.push_str(&format!("Generated: {:?}\n", std::time::SystemTime::now()));
    out.push_str("\n================ SYSTEM ================\n");
    if let Some(i) = info {
        out.push_str(&format!("Computer:  {}\n", i.computer_name));
        out.push_str(&format!("User:      {}\n", i.user_name));
        out.push_str(&format!("CPU:       {}\n", i.cpu.name));
        out.push_str(&format!(
            "Board:     {} {}\n",
            i.board.manufacturer, i.board.product
        ));
        out.push_str(&format!(
            "BIOS:      {} ({})\n",
            i.board.bios_version, i.board.bios_date
        ));
        out.push_str(&format!(
            "OS:        {} build {} ({})\n",
            i.os.caption, i.os.build, i.os.arch
        ));
        for m in &i.memory_modules {
            out.push_str(&format!(
                "DIMM:      {} {} {:.0} GB @ {} MT/s\n",
                m.bank,
                m.part_number,
                m.capacity_gb,
                m.configured_speed_mts.or(m.speed_mts).unwrap_or(0)
            ));
        }
        for g in &i.gpus {
            out.push_str(&format!("GPU:       {}\n", g.name));
        }
        for d in &i.drives {
            out.push_str(&format!(
                "Drive:     {} [{}] {:.0} GB\n",
                d.model,
                d.interface,
                d.size_gb.unwrap_or(0.0)
            ));
        }
    } else {
        out.push_str("(system info not yet available)\n");
    }

    out.push_str("\n================ SENSORS ================\n");
    fn dump(hw: &Hardware, depth: usize, out: &mut String) {
        out.push_str(&format!("{}[{}]\n", "  ".repeat(depth), hw.name));
        for s in &hw.sensors {
            out.push_str(&format!(
                "{}{:<40} {:>14}  (min {} / max {} / avg {})\n",
                "  ".repeat(depth + 1),
                s.name,
                format_value(s.value, s.sensor_type),
                format_value(s.min, s.sensor_type),
                format_value(s.max, s.sensor_type),
                format_value(s.avg, s.sensor_type),
            ));
        }
        for sub in &hw.sub_hardware {
            dump(sub, depth + 1, out);
        }
    }
    for hw in tree {
        dump(hw, 0, &mut out);
    }

    let dir = dirs::desktop_dir()
        .or_else(dirs::document_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("SensorView_Report_{stamp}.txt"));
    std::fs::write(&path, out).map_err(|e| e.to_string())?;
    Ok(path)
}
