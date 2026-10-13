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
        // The same text `sensorview info` prints, so the report and the CLI
        // can't drift apart as the System Summary grows.
        out.push_str(&crate::cli::render::info_text(i));
        out.push('\n');
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
