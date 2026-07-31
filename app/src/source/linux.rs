//! Linux native hardware sensor polling implementation via `/sys/class/hwmon`, `/proc/stat`, and `/proc/meminfo`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

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
/// Cumulative (read, write) counters per device, and when they were sampled.
/// Both /proc/diskstats and /proc/net/dev have this shape.
type Counters = (HashMap<String, (u64, u64)>, Instant);

pub struct LinuxSysfsSource {
    prev_cpu_tick: Option<CpuTick>,
    /// Per-core ticks, indexed by the `cpuN` number in /proc/stat.
    prev_core_ticks: Vec<CpuTick>,
    prev_disk: Option<Counters>,
    prev_net: Option<Counters>,
    hwmon_base: PathBuf,
}

impl LinuxSysfsSource {
    pub fn new() -> Self {
        Self {
            prev_cpu_tick: None,
            prev_core_ticks: Vec::new(),
            prev_disk: None,
            prev_net: None,
            hwmon_base: PathBuf::from("/sys/class/hwmon"),
        }
    }

    #[allow(dead_code)]
    pub fn with_hwmon_path(path: PathBuf) -> Self {
        Self {
            prev_cpu_tick: None,
            prev_core_ticks: Vec::new(),
            prev_disk: None,
            prev_net: None,
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

    // ---- Mission Center parity: the metrics Linux was missing ------------
    //
    // Every one of these is a *rate*, so the first poll establishes a baseline
    // and publishes nothing. That is the same discipline the macOS backend
    // follows, and the reason `sensor_set_is_stable_across_polls` exists —
    // a sensor that appears and disappears makes rows flicker and breaks
    // history graphs.

    /// Per-core load from the `cpuN` lines of /proc/stat.
    ///
    /// `read_cpu_load` deliberately reads only the aggregate `cpu ` line; this
    /// walks the rest. Mission Center's CPU page is per-thread, and SensorView
    /// had no per-core figure on Linux at all.
    fn read_per_core_load(&mut self) -> Vec<Sensor> {
        let Ok(content) = fs::read_to_string("/proc/stat") else {
            return Vec::new();
        };
        let ticks = Self::parse_core_ticks(&content);
        self.diff_core_ticks(ticks)
    }

    /// The `cpuN` lines of /proc/stat. Pure so it can be tested against
    /// captured kernel output from any platform.
    fn parse_core_ticks(content: &str) -> Vec<CpuTick> {
        let mut ticks: Vec<CpuTick> = Vec::new();
        for line in content.lines() {
            // "cpu0", "cpu1", ... but not the aggregate "cpu ".
            let Some(rest) = line.strip_prefix("cpu") else { continue };
            let Some((idx, values)) = rest.split_once(' ') else { continue };
            if idx.is_empty() || !idx.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let p: Vec<u64> = values.split_whitespace().filter_map(|s| s.parse().ok()).collect();
            if p.len() < 7 {
                continue;
            }
            ticks.push(CpuTick {
                user: p[0],
                nice: p[1],
                system: p[2],
                idle: p[3],
                iowait: p.get(4).copied().unwrap_or(0),
                irq: p.get(5).copied().unwrap_or(0),
                softirq: p.get(6).copied().unwrap_or(0),
                steal: p.get(7).copied().unwrap_or(0),
            });
        }
        ticks
    }

    fn diff_core_ticks(&mut self, ticks: Vec<CpuTick>) -> Vec<Sensor> {
        let mut sensors = Vec::new();
        // Only difference against a baseline of the same shape — a CPU going
        // offline changes the count and would otherwise pair the wrong cores.
        if self.prev_core_ticks.len() == ticks.len() {
            for (i, (now, before)) in ticks.iter().zip(&self.prev_core_ticks).enumerate() {
                let total = now.total().saturating_sub(before.total());
                let idle = now.idle_total().saturating_sub(before.idle_total());
                if total == 0 {
                    continue;
                }
                let pct = (total.saturating_sub(idle) as f32 / total as f32) * 100.0;
                sensors.push(Sensor {
                    identifier: format!("/sysfs/cpu/0/load/{}", i + 1),
                    name: format!("CPU Core #{}", i + 1),
                    sensor_type: SensorType::Load,
                    index: i as u32 + 1,
                    value: Some(pct.clamp(0.0, 100.0)),
                    min: None,
                    max: None,
                    avg: None,
                });
            }
        }
        self.prev_core_ticks = ticks;
        sensors
    }

    /// Disk throughput from /proc/diskstats.
    ///
    /// Fields 3 and 7 (0-indexed after the name) are sectors read and written;
    /// the kernel always reports them in 512-byte units here regardless of the
    /// device's real sector size.
    fn read_disk_throughput(&mut self) -> Vec<Hardware> {
                let Ok(content) = fs::read_to_string("/proc/diskstats") else {
            return Vec::new();
        };
        let now = Self::parse_diskstats(&content);
        self.diff_disks(now)
    }

    /// Whole-disk sector counters from /proc/diskstats, partitions excluded.
    fn parse_diskstats(content: &str) -> HashMap<String, (u64, u64)> {
        let mut now: HashMap<String, (u64, u64)> = HashMap::new();
        for line in content.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 {
                continue;
            }
            let name = f[2];
            // Skip partitions and virtual devices: "sda1" duplicates "sda",
            // and loop/ram devices are noise on a performance page.
            if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("dm-") {
                continue;
            }
            if name.chars().last().is_some_and(|c| c.is_ascii_digit())
                && (name.starts_with("sd") || name.starts_with("hd") || name.starts_with("vd"))
            {
                continue;
            }
            // nvme0n1 is a whole disk; nvme0n1p1 is a partition.
            if name.contains('p') && name.starts_with("nvme") && name.rsplit('p').next().is_some_and(|t| t.chars().all(|c| c.is_ascii_digit())) {
                continue;
            }
            let (Ok(read), Ok(written)) = (f[5].parse::<u64>(), f[9].parse::<u64>()) else {
                continue;
            };
            now.insert(name.to_string(), (read, written));
        }
        now
    }

    fn diff_disks(&mut self, now: HashMap<String, (u64, u64)>) -> Vec<Hardware> {
        const SECTOR: f64 = 512.0;
        let taken = Instant::now();
        let mut out = Vec::new();
        if let Some((before, then)) = self.prev_disk.take() {
            let secs = taken.duration_since(then).as_secs_f64();
            if secs > 0.0 {
                for (i, (name, (r, w))) in now.iter().enumerate() {
                    let Some((pr, pw)) = before.get(name) else { continue };
                    const MB: f64 = 1024.0 * 1024.0;
                    let rate = |now: u64, prev: u64| {
                        (now.saturating_sub(prev) as f64 * SECTOR / MB / secs) as f32
                    };
                    out.push(Hardware {
                        identifier: format!("/sysfs/disk/{name}"),
                        name: name.clone(),
                        hardware_type: HardwareType::Storage,
                        sensors: vec![
                            Sensor {
                                identifier: format!("/sysfs/disk/{name}/throughput/0"),
                                name: "Read Rate".into(),
                                sensor_type: SensorType::Throughput,
                                index: 0,
                                value: Some(rate(*r, *pr)),
                                min: None,
                                max: None,
                                avg: None,
                            },
                            Sensor {
                                identifier: format!("/sysfs/disk/{name}/throughput/1"),
                                name: "Write Rate".into(),
                                sensor_type: SensorType::Throughput,
                                index: 1,
                                value: Some(rate(*w, *pw)),
                                min: None,
                                max: None,
                                avg: None,
                            },
                        ],
                        sub_hardware: Vec::new(),
                    });
                    let _ = i;
                }
            }
        }
        self.prev_disk = Some((now, taken));
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Network throughput from /proc/net/dev. Columns 0 and 8 of the per-
    /// interface data are received and transmitted bytes.
    fn read_network_throughput(&mut self) -> Vec<Hardware> {
        let Ok(content) = fs::read_to_string("/proc/net/dev") else {
            return Vec::new();
        };
        let now = Self::parse_net_dev(&content);
        self.diff_net(now)
    }

    /// Per-interface (rx, tx) byte counters from /proc/net/dev.
    fn parse_net_dev(content: &str) -> HashMap<String, (u64, u64)> {
        let mut now: HashMap<String, (u64, u64)> = HashMap::new();
        for line in content.lines().skip(2) {
            let Some((iface, rest)) = line.split_once(':') else { continue };
            let iface = iface.trim();
            // Loopback is not a network interface anyone monitors.
            if iface == "lo" {
                continue;
            }
            let v: Vec<u64> = rest.split_whitespace().filter_map(|s| s.parse().ok()).collect();
            if v.len() < 9 {
                continue;
            }
            now.insert(iface.to_string(), (v[0], v[8]));
        }
        now
    }

    fn diff_net(&mut self, now: HashMap<String, (u64, u64)>) -> Vec<Hardware> {
        let taken = Instant::now();
        let mut out = Vec::new();
        if let Some((before, then)) = self.prev_net.take() {
            let secs = taken.duration_since(then).as_secs_f64();
            if secs > 0.0 {
                for (iface, (rx, tx)) in &now {
                    let Some((prx, ptx)) = before.get(iface) else { continue };
                    const MB: f64 = 1024.0 * 1024.0;
                    let rate =
                        |now: u64, prev: u64| (now.saturating_sub(prev) as f64 / MB / secs) as f32;
                    out.push(Hardware {
                        identifier: format!("/sysfs/net/{iface}"),
                        name: iface.clone(),
                        hardware_type: HardwareType::Network,
                        sensors: vec![
                            Sensor {
                                identifier: format!("/sysfs/net/{iface}/throughput/0"),
                                name: "Download".into(),
                                sensor_type: SensorType::Throughput,
                                index: 0,
                                value: Some(rate(*rx, *prx)),
                                min: None,
                                max: None,
                                avg: None,
                            },
                            Sensor {
                                identifier: format!("/sysfs/net/{iface}/throughput/1"),
                                name: "Upload".into(),
                                sensor_type: SensorType::Throughput,
                                index: 1,
                                value: Some(rate(*tx, *ptx)),
                                min: None,
                                max: None,
                                avg: None,
                            },
                        ],
                        sub_hardware: Vec::new(),
                    });
                }
            }
        }
        self.prev_net = Some((now, taken));
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// GPU utilisation from the DRM sysfs node.
    ///
    /// `gpu_busy_percent` is amdgpu's; the equivalents differ per driver, so
    /// this is a level rather than a rate and simply reports what is there.
    /// Intel and nouveau expose nothing comparable, and NVIDIA's proprietary
    /// driver needs NVML — both are gaps rather than bugs.
    fn read_gpu_busy() -> Vec<Sensor> {
        let Ok(dir) = fs::read_dir("/sys/class/drm") else {
            return Vec::new();
        };
        let mut cards: Vec<(String, f32)> = Vec::new();
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // "card0" but not "card0-DP-1", which is a connector.
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let path = entry.path().join("device/gpu_busy_percent");
            if let Ok(text) = fs::read_to_string(&path) {
                if let Ok(pct) = text.trim().parse::<f32>() {
                    cards.push((name, pct.clamp(0.0, 100.0)));
                }
            }
        }
        cards.sort_by(|a, b| a.0.cmp(&b.0));
        cards
            .into_iter()
            .enumerate()
            .map(|(i, (name, pct))| Sensor {
                identifier: format!("/sysfs/gpu/{name}/load/0"),
                name: "GPU Core".into(),
                sensor_type: SensorType::Load,
                index: i as u32,
                value: Some(pct),
                min: None,
                max: None,
                avg: None,
            })
            .collect()
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

        // Per-core load. The block above contributes one aggregate number;
        // Mission Center's CPU page — and the Task Manager sidebar — want
        // per-thread, which Linux simply did not have here before.
        let per_core = self.read_per_core_load();
        if !per_core.is_empty() {
            if let Some(cpu) = nodes.iter_mut().find(|n| n.hardware_type == HardwareType::Cpu) {
                cpu.sensors.extend(per_core);
            }
        }

        // GPU utilisation, folded into whichever GPU node hwmon already
        // classified. If hwmon found none, it becomes a node of its own rather
        // than the reading being dropped.
        let gpu_busy = Self::read_gpu_busy();
        if !gpu_busy.is_empty() {
            let existing = nodes.iter_mut().find(|n| {
                matches!(
                    n.hardware_type,
                    HardwareType::GpuAti | HardwareType::GpuIntel | HardwareType::GpuNvidia
                )
            });
            match existing {
                Some(gpu) => {
                    gpu.sensors.splice(0..0, gpu_busy);
                }
                None => nodes.push(Hardware {
                    identifier: "/sysfs/gpu/0".into(),
                    name: "GPU".into(),
                    hardware_type: HardwareType::GpuAti,
                    sensors: gpu_busy,
                    sub_hardware: Vec::new(),
                }),
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

        // Disks and network interfaces are top-level nodes of their own, which
        // is what the Task Manager sidebar numbers as "Disk 0", "Network 0".
        nodes.extend(self.read_disk_throughput());
        nodes.extend(self.read_network_throughput());

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


    // ---- Mission Center parity collectors --------------------------------
    //
    // Parsing is split from I/O so these run against captured kernel output on
    // any platform, not only on Linux.

    #[test]
    fn per_core_ticks_skip_the_aggregate_line() {
        // Real shape: the aggregate "cpu " first, then cpu0/cpu1, then other
        // sections that must not be mistaken for cores.
        let stat = "\
cpu  100 0 50 800 0 0 0 0
cpu0 60 0 30 400 0 0 0 0
cpu1 40 0 20 400 0 0 0 0
intr 12345 0 0
ctxt 999
";
        let ticks = LinuxSysfsSource::parse_core_ticks(stat);
        assert_eq!(ticks.len(), 2, "only cpu0/cpu1 are cores");
        assert_eq!(ticks[0].user, 60);
        assert_eq!(ticks[1].user, 40);
    }

    /// Rates need a baseline: the first poll must publish nothing rather than
    /// inventing a number from a single sample.
    #[test]
    fn per_core_load_needs_a_baseline_then_reports() {
        let mut src = LinuxSysfsSource::new();
        let first = "cpu0 100 0 0 900 0 0 0 0\ncpu1 0 0 0 1000 0 0 0 0\n";
        assert!(
            src.diff_core_ticks(LinuxSysfsSource::parse_core_ticks(first)).is_empty(),
            "first sample must not produce a load"
        );

        // cpu0 spent 100 more ticks busy out of 200 elapsed -> 50 %.
        let second = "cpu0 200 0 0 1000 0 0 0 0\ncpu1 0 0 0 1200 0 0 0 0\n";
        let out = src.diff_core_ticks(LinuxSysfsSource::parse_core_ticks(second));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "CPU Core #1");
        assert!((out[0].value.unwrap() - 50.0).abs() < 0.01, "{:?}", out[0].value);
        assert_eq!(out[1].value, Some(0.0), "an idle core reads zero");
    }

    /// A CPU going offline changes the core count; pairing the new list
    /// against the old one would attribute the wrong ticks to the wrong core.
    #[test]
    fn core_count_change_resets_rather_than_mispairs() {
        let mut src = LinuxSysfsSource::new();
        src.diff_core_ticks(LinuxSysfsSource::parse_core_ticks(
            "cpu0 0 0 0 100 0 0 0 0\ncpu1 0 0 0 100 0 0 0 0\n",
        ));
        let out = src.diff_core_ticks(LinuxSysfsSource::parse_core_ticks(
            "cpu0 50 0 0 150 0 0 0 0\n",
        ));
        assert!(out.is_empty(), "a changed core count must re-baseline, not mispair");
    }

    /// Partitions duplicate their parent disk and would double-count.
    #[test]
    fn diskstats_excludes_partitions_and_virtual_devices() {
        let stats = "\
   8       0 sda 100 0 2000 0 50 0 1000 0 0 0 0
   8       1 sda1 90 0 1800 0 40 0 900 0 0 0 0
 259       0 nvme0n1 10 0 400 0 5 0 200 0 0 0 0
 259       1 nvme0n1p1 9 0 380 0 4 0 190 0 0 0 0
   7       0 loop0 1 0 2 0 0 0 0 0 0 0 0
";
        let disks = LinuxSysfsSource::parse_diskstats(stats);
        let mut names: Vec<&String> = disks.keys().collect();
        names.sort();
        assert_eq!(names, vec!["nvme0n1", "sda"], "got {names:?}");
    }

    #[test]
    fn disk_throughput_needs_a_baseline_then_converts_sectors_to_mb() {
        let mut src = LinuxSysfsSource::new();
        let one = "   8       0 sda 0 0 0 0 0 0 0 0 0 0 0\n";
        assert!(
            src.diff_disks(LinuxSysfsSource::parse_diskstats(one)).is_empty(),
            "first poll must not publish a rate"
        );

        // +2048 sectors read = 1 MiB. The interval is real time, so only the
        // sign and magnitude are asserted, not an exact rate.
        std::thread::sleep(std::time::Duration::from_millis(60));
        let two = "   8       0 sda 0 0 2048 0 0 0 0 0 0 0 0\n";
        let out = src.diff_disks(LinuxSysfsSource::parse_diskstats(two));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hardware_type, HardwareType::Storage);
        let read = out[0].sensors.iter().find(|s| s.name == "Read Rate").unwrap();
        assert!(read.value.unwrap() > 0.0, "read rate should be positive");
        let write = out[0].sensors.iter().find(|s| s.name == "Write Rate").unwrap();
        assert_eq!(write.value, Some(0.0), "nothing was written");
    }

    /// Loopback is not an interface anyone monitors, and the two header lines
    /// must not be parsed as data.
    #[test]
    fn net_dev_skips_headers_and_loopback() {
        let dev = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets
    lo: 1000      10    0    0    0     0          0         0     1000      10
  eth0: 5000      50    0    0    0     0          0         0     2500      25
";
        let nics = LinuxSysfsSource::parse_net_dev(dev);
        assert_eq!(nics.len(), 1, "only eth0: {nics:?}");
        assert_eq!(nics.get("eth0"), Some(&(5000, 2500)));
    }

    #[test]
    fn network_throughput_needs_a_baseline_and_labels_both_directions() {
        let mut src = LinuxSysfsSource::new();
        let hdr = "h\nh\n";
        let one = format!("{hdr}  eth0: 0 0 0 0 0 0 0 0 0 0\n");
        assert!(src.diff_net(LinuxSysfsSource::parse_net_dev(&one)).is_empty());

        std::thread::sleep(std::time::Duration::from_millis(60));
        let two = format!("{hdr}  eth0: 1048576 0 0 0 0 0 0 0 524288 0\n");
        let out = src.diff_net(LinuxSysfsSource::parse_net_dev(&two));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hardware_type, HardwareType::Network);
        let names: Vec<&str> = out[0].sensors.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Download", "Upload"]);
        assert!(out[0].sensors[0].value.unwrap() > 0.0);
    }

    /// Counters reset when a device re-enumerates; a negative delta must not
    /// wrap into an enormous positive rate.
    #[test]
    fn counter_reset_does_not_produce_a_huge_rate() {
        let mut src = LinuxSysfsSource::new();
        let hdr = "h\nh\n";
        let big = format!("{hdr}  eth0: 999999999 0 0 0 0 0 0 0 999999999 0\n");
        src.diff_net(LinuxSysfsSource::parse_net_dev(&big));
        std::thread::sleep(std::time::Duration::from_millis(60));
        let reset = format!("{hdr}  eth0: 100 0 0 0 0 0 0 0 100 0\n");
        let out = src.diff_net(LinuxSysfsSource::parse_net_dev(&reset));
        assert_eq!(out[0].sensors[0].value, Some(0.0), "saturating_sub must floor at zero");
    }

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
