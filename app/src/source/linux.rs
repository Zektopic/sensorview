//! Linux native hardware sensor polling implementation via `/sys/class/hwmon`, `/proc/stat`, and `/proc/meminfo`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::model::{Hardware, HardwareType, Sensor, SensorType};
use crate::source::{Diagnostics, SensorSource};

/// Tracks CPU ticks for calculating total CPU load percentage between snapshot polls.
#[derive(Debug, Default, Clone, Copy)]
struct CpuTick {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
}

impl CpuTick {
    fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq + self.steal
    }

    fn idle_total(&self) -> u64 {
        self.idle + self.iowait
    }
}

/// Pure-Rust Linux sensor source reading `/sys/class/hwmon`, `/proc/stat`, `/proc/meminfo`, and cpufreq.
pub struct LinuxSysfsSource {
    prev_cpu_tick: Option<CpuTick>,
    hwmon_base: PathBuf,
}

impl LinuxSysfsSource {
    pub fn new() -> Self {
        Self {
            prev_cpu_tick: None,
            hwmon_base: PathBuf::from("/sys/class/hwmon"),
        }
    }

    #[allow(dead_code)]
    pub fn with_hwmon_path(path: PathBuf) -> Self {
        Self {
            prev_cpu_tick: None,
            hwmon_base: path,
        }
    }

    /// Read `/proc/stat` total CPU usage percentage.
    fn read_cpu_load(&mut self) -> Option<f32> {
        let content = fs::read_to_string("/proc/stat").ok()?;
        let line = content.lines().next()?;
        if !line.starts_with("cpu ") {
            return None;
        }

        let parts: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|s| s.parse().ok())
            .collect();

        if parts.len() < 7 {
            return None;
        }

        let tick = CpuTick {
            user: parts[0],
            nice: parts[1],
            system: parts[2],
            idle: parts[3],
            iowait: parts.get(4).copied().unwrap_or(0),
            irq: parts.get(5).copied().unwrap_or(0),
            softirq: parts.get(6).copied().unwrap_or(0),
            steal: parts.get(7).copied().unwrap_or(0),
        };

        let load = if let Some(prev) = self.prev_cpu_tick {
            let total_diff = tick.total().saturating_sub(prev.total());
            let idle_diff = tick.idle_total().saturating_sub(prev.idle_total());
            if total_diff > 0 {
                let active = total_diff.saturating_sub(idle_diff);
                Some((active as f32 / total_diff as f32) * 100.0)
            } else {
                Some(0.0)
            }
        } else {
            None
        };

        self.prev_cpu_tick = Some(tick);
        load
    }

    /// Read `/proc/meminfo` RAM load % and used memory GB.
    fn read_ram_stats() -> (Option<f32>, Option<f32>, Option<f32>) {
        let Ok(content) = fs::read_to_string("/proc/meminfo") else {
            return (None, None, None);
        };

        let mut total_kb: Option<f64> = None;
        let mut avail_kb: Option<f64> = None;

        for line in content.lines() {
            if line.starts_with("MemTotal:") {
                total_kb = line.split_whitespace().nth(1).and_then(|s| s.parse().ok());
            } else if line.starts_with("MemAvailable:") {
                avail_kb = line.split_whitespace().nth(1).and_then(|s| s.parse().ok());
            }
        }

        if let (Some(total), Some(avail)) = (total_kb, avail_kb) {
            let used = (total - avail).max(0.0);
            let load_pct = ((used / total) * 100.0) as f32;
            let used_gb = (used / (1024.0 * 1024.0)) as f32;
            let total_gb = (total / (1024.0 * 1024.0)) as f32;
            (Some(load_pct), Some(used_gb), Some(total_gb))
        } else {
            (None, None, None)
        }
    }

    /// Scan `/sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq` for CPU core clock frequencies.
    fn read_cpu_clocks() -> Vec<Sensor> {
        let mut sensors = Vec::new();
        let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") else {
            return sensors;
        };

        let mut cpus: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with("cpu") && name[3..].chars().all(|c| c.is_ascii_digit())
            })
            .collect();

        cpus.sort_by_key(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name[3..].parse::<u32>().unwrap_or(0)
        });

        for (idx, entry) in cpus.iter().enumerate() {
            let freq_file = entry.path().join("cpufreq/scaling_cur_freq");
            if let Ok(text) = fs::read_to_string(&freq_file) {
                if let Ok(khz) = text.trim().parse::<f32>() {
                    let mhz = khz / 1000.0;
                    sensors.push(Sensor {
                        identifier: format!("/sysfs/cpu/0/clock/{idx}"),
                        name: format!("Core #{idx} Clock"),
                        sensor_type: SensorType::Clock,
                        index: idx as u32,
                        value: Some(mhz),
                        min: None,
                        max: None,
                        avg: None,
                    });
                }
            }
        }

        sensors
    }

    /// Read hardware nodes from `/sys/class/hwmon/hwmon*`.
    fn scan_hwmon(&mut self) -> Vec<Hardware> {
        let Ok(entries) = fs::read_dir(&self.hwmon_base) else {
            return Vec::new();
        };

        let mut hardware_nodes = Vec::new();

        let mut dirs: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        dirs.sort_by_key(|e| e.file_name());

        for (hw_idx, entry) in dirs.iter().enumerate() {
            let dir_path = entry.path();
            if !dir_path.is_dir() {
                continue;
            }

            let name_file = dir_path.join("name");
            let name = fs::read_to_string(&name_file)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| entry.file_name().to_string_lossy().to_string());

            let hw_type = classify_hardware(&name);
            let sensors = scan_hwmon_dir(&dir_path, &name, hw_idx);

            if !sensors.is_empty() || hw_type == HardwareType::Cpu {
                hardware_nodes.push(Hardware {
                    identifier: format!("/sysfs/hwmon/{hw_idx}"),
                    name: format!("{} ({name})", format_hw_name(&name, hw_type)),
                    hardware_type: hw_type,
                    sensors,
                    sub_hardware: Vec::new(),
                });
            }
        }

        hardware_nodes
    }
}

impl SensorSource for LinuxSysfsSource {
    fn name(&self) -> &'static str {
        "Linux sysfs"
    }

    fn snapshot(&mut self) -> Vec<Hardware> {
        let mut nodes = self.scan_hwmon();

        // 1. CPU Node enhancement (CPU Load % and Core Clocks)
        let cpu_load = self.read_cpu_load();
        let cpu_clocks = Self::read_cpu_clocks();

        let cpu_node = nodes.iter_mut().find(|n| n.hardware_type == HardwareType::Cpu);

        if let Some(cpu) = cpu_node {
            if let Some(load) = cpu_load {
                cpu.sensors.insert(
                    0,
                    Sensor {
                        identifier: "/sysfs/cpu/0/load/0".into(),
                        name: "CPU Total".into(),
                        sensor_type: SensorType::Load,
                        index: 0,
                        value: Some(load),
                        min: None,
                        max: None,
                        avg: None,
                    },
                );
            }
            cpu.sensors.extend(cpu_clocks);
        } else {
            // If no hwmon CPU node (e.g. k10temp) existed, create a primary CPU node
            let mut sensors = Vec::new();
            if let Some(load) = cpu_load {
                sensors.push(Sensor {
                    identifier: "/sysfs/cpu/0/load/0".into(),
                    name: "CPU Total".into(),
                    sensor_type: SensorType::Load,
                    index: 0,
                    value: Some(load),
                    min: None,
                    max: None,
                    avg: None,
                });
            }
            sensors.extend(cpu_clocks);

            if !sensors.is_empty() {
                nodes.push(Hardware {
                    identifier: "/sysfs/cpu/0".into(),
                    name: "Processor".into(),
                    hardware_type: HardwareType::Cpu,
                    sensors,
                    sub_hardware: Vec::new(),
                });
            }
        }

        // 2. RAM Node (Load %, Used GB, Total GB)
        let (ram_load, ram_used_gb, ram_total_gb) = Self::read_ram_stats();
        if ram_load.is_some() || ram_used_gb.is_some() {
            let mut ram_sensors = Vec::new();
            if let Some(load) = ram_load {
                ram_sensors.push(Sensor {
                    identifier: "/sysfs/ram/0/load/0".into(),
                    name: "Memory Used".into(),
                    sensor_type: SensorType::Load,
                    index: 0,
                    value: Some(load),
                    min: None,
                    max: None,
                    avg: None,
                });
            }
            if let Some(used) = ram_used_gb {
                ram_sensors.push(Sensor {
                    identifier: "/sysfs/ram/0/data/0".into(),
                    name: "Memory Used".into(),
                    sensor_type: SensorType::Data,
                    index: 0,
                    value: Some(used),
                    min: None,
                    max: None,
                    avg: None,
                });
            }
            if let Some(total) = ram_total_gb {
                ram_sensors.push(Sensor {
                    identifier: "/sysfs/ram/0/data/1".into(),
                    name: "Memory Total".into(),
                    sensor_type: SensorType::Data,
                    index: 1,
                    value: Some(total),
                    min: None,
                    max: None,
                    avg: None,
                });
            }

            nodes.push(Hardware {
                identifier: "/sysfs/ram/0".into(),
                name: "Generic Memory".into(),
                hardware_type: HardwareType::Ram,
                sensors: ram_sensors,
                sub_hardware: Vec::new(),
            });
        }

        nodes
    }

    fn diagnostics(&self) -> Diagnostics {
        Diagnostics {
            engine_version: "Linux sysfs 0.1.0".into(),
            driver_report: format!("hwmon path: {}", self.hwmon_base.display()),
        }
    }
}

/// Classify hwmon chip name into HardwareType.
fn classify_hardware(name: &str) -> HardwareType {
    match name.to_lowercase().as_str() {
        "k10temp" | "coretemp" | "zenpower" | "cpu" | "intel_rapl" => HardwareType::Cpu,
        "amdgpu" | "radeon" => HardwareType::GpuAti,
        "nouveau" | "nvidia" => HardwareType::GpuNvidia,
        "i915" | "xe" => HardwareType::GpuIntel,
        "nvme" | "drivetemp" => HardwareType::Hdd,
        "spd5118" | "ee1004" => HardwareType::Ram,
        "nct6775" | "it87" | "w83627ehf" | "w83627dhg" => HardwareType::SuperIO,
        _ => HardwareType::Mainboard,
    }
}

fn format_hw_name(_name: &str, hw_type: HardwareType) -> &'static str {
    match hw_type {
        HardwareType::Cpu => "Processor",
        HardwareType::GpuAti => "AMD Radeon GPU",
        HardwareType::GpuNvidia => "NVIDIA GPU",
        HardwareType::GpuIntel => "Intel HD/Arc GPU",
        HardwareType::Hdd => "Storage Drive",
        HardwareType::Ram => "System Memory",
        HardwareType::SuperIO => "Super I/O Chip",
        _ => "System Device",
    }
}

/// Read all sensor files inside a `/sys/class/hwmon/hwmonX` directory.
fn scan_hwmon_dir(dir: &Path, _hw_name: &str, hw_idx: usize) -> Vec<Sensor> {
    let mut sensors = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return sensors;
    };

    let mut map: HashMap<String, String> = HashMap::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let fname = entry.file_name().to_string_lossy().to_string();
        if let Ok(content) = fs::read_to_string(entry.path()) {
            map.insert(fname, content.trim().to_string());
        }
    }

    // Process temperature: temp*_input
    for i in 1..=32 {
        let key = format!("temp{i}_input");
        if let Some(val_str) = map.get(&key) {
            if let Ok(mdeg) = val_str.parse::<f32>() {
                let deg = mdeg / 1000.0;
                let label = map
                    .get(&format!("temp{i}_label"))
                    .cloned()
                    .unwrap_or_else(|| format!("Temperature #{i}"));

                sensors.push(Sensor {
                    identifier: format!("/sysfs/hwmon/{hw_idx}/temp/{i}"),
                    name: label,
                    sensor_type: SensorType::Temperature,
                    index: i as u32,
                    value: Some(deg),
                    min: None,
                    max: None,
                    avg: None,
                });
            }
        }
    }

    // Process fan speed: fan*_input
    for i in 1..=32 {
        let key = format!("fan{i}_input");
        if let Some(val_str) = map.get(&key) {
            if let Ok(rpm) = val_str.parse::<f32>() {
                let label = map
                    .get(&format!("fan{i}_label"))
                    .cloned()
                    .unwrap_or_else(|| format!("Fan #{i}"));

                sensors.push(Sensor {
                    identifier: format!("/sysfs/hwmon/{hw_idx}/fan/{i}"),
                    name: label,
                    sensor_type: SensorType::Fan,
                    index: i as u32,
                    value: Some(rpm),
                    min: None,
                    max: None,
                    avg: None,
                });
            }
        }
    }

    // Process voltage: in*_input
    for i in 0..=32 {
        let key = format!("in{i}_input");
        if let Some(val_str) = map.get(&key) {
            if let Ok(mvolt) = val_str.parse::<f32>() {
                let volt = mvolt / 1000.0;
                let label = map
                    .get(&format!("in{i}_label"))
                    .cloned()
                    .unwrap_or_else(|| format!("Voltage #{i}"));

                sensors.push(Sensor {
                    identifier: format!("/sysfs/hwmon/{hw_idx}/in/{i}"),
                    name: label,
                    sensor_type: SensorType::Voltage,
                    index: i as u32,
                    value: Some(volt),
                    min: None,
                    max: None,
                    avg: None,
                });
            }
        }
    }

    // Process power: power*_input / power*_average
    for i in 1..=32 {
        let key = format!("power{i}_input");
        let key_avg = format!("power{i}_average");
        let val_str = map.get(&key).or_else(|| map.get(&key_avg));

        if let Some(val_str) = val_str {
            if let Ok(uwatt) = val_str.parse::<f32>() {
                let watt = uwatt / 1_000_000.0;
                let label = map
                    .get(&format!("power{i}_label"))
                    .cloned()
                    .unwrap_or_else(|| format!("Power #{i}"));

                sensors.push(Sensor {
                    identifier: format!("/sysfs/hwmon/{hw_idx}/power/{i}"),
                    name: label,
                    sensor_type: SensorType::Power,
                    index: i as u32,
                    value: Some(watt),
                    min: None,
                    max: None,
                    avg: None,
                });
            }
        }
    }

    // Process frequency: freq*_input
    for i in 1..=32 {
        let key = format!("freq{i}_input");
        if let Some(val_str) = map.get(&key) {
            if let Ok(hz) = val_str.parse::<f32>() {
                let mhz = hz / 1_000_000.0;
                let label = map
                    .get(&format!("freq{i}_label"))
                    .cloned()
                    .unwrap_or_else(|| format!("Clock #{i}"));

                sensors.push(Sensor {
                    identifier: format!("/sysfs/hwmon/{hw_idx}/freq/{i}"),
                    name: label,
                    sensor_type: SensorType::Clock,
                    index: i as u32,
                    value: Some(mhz),
                    min: None,
                    max: None,
                    avg: None,
                });
            }
        }
    }

    sensors
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_classify_hardware() {
        assert_eq!(classify_hardware("k10temp"), HardwareType::Cpu);
        assert_eq!(classify_hardware("coretemp"), HardwareType::Cpu);
        assert_eq!(classify_hardware("amdgpu"), HardwareType::GpuAti);
        assert_eq!(classify_hardware("nvme"), HardwareType::Hdd);
        assert_eq!(classify_hardware("spd5118"), HardwareType::Ram);
    }

    #[test]
    fn test_scan_hwmon_dir() {
        let unique_name = format!("sensorview_test_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        let dpath = std::env::temp_dir().join(unique_name);
        fs::create_dir_all(&dpath).unwrap();

        File::create(dpath.join("name")).unwrap().write_all(b"k10temp\n").unwrap();
        File::create(dpath.join("temp1_input")).unwrap().write_all(b"45500\n").unwrap();
        File::create(dpath.join("temp1_label")).unwrap().write_all(b"Tctl\n").unwrap();

        let sensors = scan_hwmon_dir(&dpath, "k10temp", 0);
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].name, "Tctl");
        assert_eq!(sensors[0].sensor_type, SensorType::Temperature);
        assert_eq!(sensors[0].value, Some(45.5));

        let _ = fs::remove_dir_all(dpath);
    }
}
