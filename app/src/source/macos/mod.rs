//! Native macOS (Apple Silicon) sensor backend.
//!
//! Unlike the Windows path, this needs **no sidecar process, no kernel driver
//! and no elevation** — every source below is readable by an ordinary user:
//!
//! | Data                    | Mechanism                          |
//! |-------------------------|------------------------------------|
//! | Die temperatures        | `IOHIDEventSystemClient` (SPI)     |
//! | CPU/GPU/ANE power       | `libIOReport` (SPI, dlopen'd)      |
//! | CPU load, memory        | Mach `host_*` (public)             |
//! | GPU utilisation/memory  | IOKit `IOAccelerator` (public)     |
//! | SSD throughput          | IOKit `IOBlockStorageDriver`       |
//! | Battery                 | IOKit `AppleSmartBattery`          |
//!
//! Two of those are private API. That is fine for `.dmg` distribution and for
//! notarization, but it rules out the Mac App Store, and Apple can change or
//! remove them in any macOS release. Because the release profile sets
//! `panic = "abort"`, an unavailable API must never be a panic: every collector
//! degrades to producing no sensors, and [`SensorSource::diagnostics`] reports
//! which subsystems resolved so the Settings dialog can explain a thin tree.
//!
//! `AppleSMC` is deliberately not used — it does not exist on M-series Macs
//! (verified on an M5: `ioreg -n AppleSMC` matches nothing). Fan sensors would
//! come from the same HID plane as temperatures on Macs that have fans; this
//! machine is fanless, so that path is untested.

pub mod battery;
pub mod dvfs;
pub mod dynlib;
pub mod freq;
pub mod gpu;
pub mod hid;
pub mod inventory;
pub mod iokit;
pub mod ioreport;
pub mod load;
pub mod storage;
pub mod sysprofile;

use crate::model::{Hardware, HardwareType, Sensor, SensorType};

use super::{Diagnostics, SensorSource};

/// Build a reading. min/max/avg are the `Monitor`'s job, matching `DemoSource`.
pub(crate) fn sensor(
    identifier: &str,
    name: &str,
    sensor_type: SensorType,
    index: u32,
    value: f32,
) -> Sensor {
    sensor_opt(identifier, name, sensor_type, index, Some(value))
}

/// Build a reading that may have no value this tick.
///
/// `Sensor::value` is `Option` precisely so a sensor can be present without
/// having produced a reading (see `model/mod.rs`). Publishing the sensor with
/// `None` keeps the row in the table — and its position stable — where
/// omitting it entirely would make the row flicker in and out. `poll::enrich`
/// skips `None` without disturbing the accumulated min/max/avg, and the CSV
/// logger writes an empty cell without shifting columns.
pub(crate) fn sensor_opt(
    identifier: &str,
    name: &str,
    sensor_type: SensorType,
    index: u32,
    value: Option<f32>,
) -> Sensor {
    Sensor {
        identifier: identifier.to_string(),
        name: name.to_string(),
        sensor_type,
        index,
        value,
        min: None,
        max: None,
        avg: None,
    }
}

/// Note a piece of hardware or SPI that isn't present, so a test can skip the
/// *presence* check without going quiet about it.
///
/// CI runs macOS on virtualized runners, where the IOHID sensor plane, an
/// integrated GPU, an NVMe controller or a battery may all be missing. Failing
/// there would say nothing about the code. Only presence is ever skipped —
/// every consistency, range and uniqueness assertion still runs whenever the
/// hardware *is* there, which is where the real coverage comes from.
#[cfg(test)]
pub(crate) fn absent(what: &str) {
    eprintln!("SKIP: {what} is not available on this machine");
}

/// Suffix repeated display names with `#2`, `#3`, … within one node.
///
/// The firmware exposes several sensors sharing a name (this M5 reports seven
/// distinct `"gas gauge battery"` probes). Identifiers stay unique regardless,
/// but a table showing the same label seven times is unreadable.
///
/// Keyed on name **and type**: a name reused across types is not a collision,
/// because the UI shows those in different unit columns. "GPU Core" as both a
/// load and a clock is the intended LHM naming, and "Memory Used" appears
/// deliberately as both a percentage and a size.
fn disambiguate(sensors: &mut [Sensor]) {
    let mut seen: std::collections::HashMap<(String, SensorType), u32> =
        std::collections::HashMap::new();
    for s in sensors.iter_mut() {
        let count = seen.entry((s.name.clone(), s.sensor_type)).or_insert(0);
        *count += 1;
        if *count > 1 {
            s.name = format!("{} #{}", s.name, *count);
        }
    }
}

pub struct MacSource {
    hid: hid::HidSensors,
    energy: ioreport::EnergyReporter,
    clocks: freq::FrequencyReporter,
    load: load::LoadCollector,
    storage: storage::StorageCollector,
    /// Cached at construction: which subsystems came up.
    report: String,
}

impl MacSource {
    pub fn new() -> Self {
        let hid = hid::HidSensors::new();
        let energy = ioreport::EnergyReporter::new();
        let clocks = freq::FrequencyReporter::new();

        // Record availability once, at startup, so the Settings dialog can say
        // *why* the tree is thin instead of silently showing fewer sensors.
        let mut lines = Vec::new();
        lines.push(format!(
            "IOHIDEventSystem (temperatures): {}",
            if hid.available() {
                format!("ok — {} sensors", hid.sensor_count())
            } else {
                "unavailable — no temperature sensors".to_string()
            }
        ));
        lines.push(format!(
            "libIOReport (power): {}",
            if energy.available() { "ok" } else { "unavailable — no power sensors" }
        ));
        lines.push(format!(
            "IOReport DVFS residency (clocks): {}",
            if clocks.available() { "ok" } else { "unavailable — no clock sensors" }
        ));
        lines.push(
            "No kernel driver or elevation is required on macOS; all sensors are read via IOKit."
                .to_string(),
        );

        Self {
            hid,
            energy,
            clocks,
            load: load::LoadCollector::new(),
            storage: storage::StorageCollector::new(),
            report: lines.join("\n"),
        }
    }
}

impl SensorSource for MacSource {
    fn name(&self) -> &'static str {
        "macOS IOKit (native)"
    }

    fn snapshot(&mut self) -> Vec<Hardware> {
        let mut tree = Vec::new();

        // Power rails belong to the block that burns them, so split the one
        // IOReport sample across the SoC / GPU / memory nodes rather than
        // dumping every rail under the CPU.
        let (mut soc_power, mut gpu_power, mut ram_power) = (Vec::new(), Vec::new(), Vec::new());
        for s in self.energy.power_sensors() {
            match ioreport::rail_of(&s.name) {
                ioreport::Rail::Gpu => gpu_power.push(s),
                ioreport::Rail::Memory => ram_power.push(s),
                ioreport::Rail::Soc => soc_power.push(s),
            }
        }

        // --- SoC: temperatures + power + CPU load in one node --------------
        // Apple Silicon is a single package, so splitting CPU temperature from
        // CPU power into separate nodes would misrepresent the hardware.
        // Clocks are reconstructed from DVFS residency, so the GPU's clock
        // arrives on the same sample as the CPU clusters' and has to be split
        // out to the GPU node alongside its power.
        let mut gpu_clocks = Vec::new();
        let mut soc_clocks = Vec::new();
        for s in self.clocks.clock_sensors() {
            if s.identifier.ends_with("/gpu") {
                gpu_clocks.push(s);
            } else {
                soc_clocks.push(s);
            }
        }

        let mut soc_sensors = self.load.cpu_load_sensors();
        soc_sensors.append(&mut soc_clocks);
        soc_sensors.append(&mut soc_power);
        soc_sensors.extend(self.hid.temperatures());
        disambiguate(&mut soc_sensors);

        if !soc_sensors.is_empty() {
            let name = crate::sysinfo::sysctl_string("machdep.cpu.brand_string")
                .unwrap_or_else(|| "Apple Silicon".to_string());
            tree.push(Hardware {
                identifier: "/applesoc/0".into(),
                name,
                hardware_type: HardwareType::Cpu,
                sensors: soc_sensors,
                sub_hardware: Vec::new(),
            });
        }

        if let Some(mut gpu) = gpu::collect() {
            gpu.sensors.append(&mut gpu_clocks);
            gpu.sensors.append(&mut gpu_power);
            tree.push(gpu);
        }

        let mut memory_sensors = self.load.memory_sensors();
        memory_sensors.append(&mut ram_power);
        if !memory_sensors.is_empty() {
            tree.push(Hardware {
                identifier: "/ram/0".into(),
                name: "Unified Memory".into(),
                hardware_type: HardwareType::Ram,
                sensors: memory_sensors,
                sub_hardware: Vec::new(),
            });
        }

        tree.extend(self.storage.collect());

        if let Some(battery) = battery::collect() {
            tree.push(battery);
        }

        // Cheap, and keeps a future firmware revision that repeats a rail name
        // from producing an unreadable table.
        for hw in &mut tree {
            disambiguate(&mut hw.sensors);
        }

        tree
    }

    fn diagnostics(&self) -> Diagnostics {
        Diagnostics {
            engine_version: format!("macOS IOKit backend {}", env!("CARGO_PKG_VERSION")),
            driver_report: self.report.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end: the backend must produce a real tree on this machine, and
    /// must do so on the *second* poll — several collectors are rate-based and
    /// legitimately return nothing on the first.
    #[test]
    fn snapshot_produces_a_populated_tree() {
        let mut source = MacSource::new();
        let _ = source.snapshot();
        std::thread::sleep(std::time::Duration::from_millis(250));
        let tree = source.snapshot();

        assert!(!tree.is_empty(), "expected a non-empty hardware tree");

        // CPU load and memory come from the public Mach interfaces, which work
        // everywhere including virtualized runners — so these are unconditional.
        let types: Vec<_> = tree.iter().map(|h| h.hardware_type).collect();
        assert!(types.contains(&HardwareType::Cpu), "SoC node missing");
        assert!(types.contains(&HardwareType::Ram), "memory node missing");
        // GPU and storage depend on IOKit services a VM may not expose.
        if !types.contains(&HardwareType::GpuApple) {
            absent("GPU node");
        }
        if !types.contains(&HardwareType::Storage) {
            absent("storage node");
        }

        // A sensor may be present without a reading this tick (see
        // `sensor_opt`), but any value it does carry must be a real number —
        // NaN/inf would propagate into the graphs and the CSV log.
        for hw in &tree {
            for s in &hw.sensors {
                if let Some(v) = s.value {
                    assert!(v.is_finite(), "{}/{} is not finite", hw.name, s.name);
                }
            }
        }
        assert!(
            tree.iter().any(|hw| hw.sensors.iter().any(|s| s.value.is_some())),
            "the whole tree produced no readings"
        );
    }

    /// The SoC node is the headline: it must carry temperature, power and load
    /// together, which is what proves both SPI paths resolved.
    #[test]
    fn soc_node_has_temperature_power_and_load() {
        let mut source = MacSource::new();
        let _ = source.snapshot();
        std::thread::sleep(std::time::Duration::from_millis(250));
        let tree = source.snapshot();

        let soc = tree
            .iter()
            .find(|h| h.hardware_type == HardwareType::Cpu)
            .expect("SoC node");
        // Load is from Mach and always available; temperature and power come
        // from SPI that a virtualized runner may not expose.
        assert!(
            soc.sensors.iter().any(|s| s.sensor_type == SensorType::Load),
            "SoC node has no Load sensor"
        );
        for wanted in [SensorType::Temperature, SensorType::Power] {
            if !soc.sensors.iter().any(|s| s.sensor_type == wanted) {
                absent(&format!("SoC {wanted:?} sensors"));
            }
        }
    }

    /// Rate-derived sensors used to drop out whenever a block idled: a power
    /// rail that burned no measurable energy, or a cluster/GPU with no
    /// non-idle residency, produced no sensor that tick. The row then vanished
    /// from the Sensors table and blanked the Summary, reappearing on the next
    /// poll — visible as values flickering in and out.
    ///
    /// After warm-up the published set must be identical from poll to poll.
    #[test]
    fn sensor_set_is_stable_across_polls() {
        let mut source = MacSource::new();
        // Two polls to establish every baseline (power, clocks, storage, load).
        let _ = source.snapshot();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = source.snapshot();

        let ids = |tree: &[Hardware]| -> std::collections::BTreeSet<String> {
            tree.iter()
                .flat_map(|hw| hw.sensors.iter().map(|s| s.identifier.clone()))
                .collect()
        };

        std::thread::sleep(std::time::Duration::from_millis(200));
        let baseline = ids(&source.snapshot());
        assert!(!baseline.is_empty());

        // Several quiet polls: this is exactly when idle blocks used to drop.
        for poll in 0..4 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let current = ids(&source.snapshot());
            let missing: Vec<_> = baseline.difference(&current).collect();
            let added: Vec<_> = current.difference(&baseline).collect();
            assert!(
                missing.is_empty() && added.is_empty(),
                "sensor set changed on poll {poll}: disappeared {missing:?}, appeared {added:?}"
            );
        }
    }

    #[test]
    fn identifiers_are_globally_unique() {
        let mut source = MacSource::new();
        let _ = source.snapshot();
        std::thread::sleep(std::time::Duration::from_millis(250));
        let tree = source.snapshot();

        let mut seen = std::collections::HashSet::new();
        for hw in &tree {
            assert!(seen.insert(hw.identifier.clone()), "duplicate hardware id {}", hw.identifier);
            for s in &hw.sensors {
                assert!(seen.insert(s.identifier.clone()), "duplicate sensor id {}", s.identifier);
            }
        }
    }

    /// Not an assertion — a probe. `cargo test -- --ignored --nocapture
    /// dump_live_tree` prints what the backend actually reads, for eyeballing
    /// against `powermetrics` / `ioreg` / Activity Monitor.
    #[test]
    #[ignore = "diagnostic probe; run explicitly"]
    fn dump_live_tree() {
        let mut source = MacSource::new();
        let _ = source.snapshot();
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("\n=== {} ===\n{}\n", source.name(), source.diagnostics().driver_report);
        for hw in source.snapshot() {
            println!("{:?}  {}", hw.hardware_type, hw.name);
            for s in &hw.sensors {
                println!(
                    "    {:<28} {:>10.2} {}",
                    s.name,
                    s.value.unwrap_or(f32::NAN),
                    s.sensor_type.unit()
                );
            }
        }
    }

    #[test]
    fn diagnostics_describe_the_backend() {
        let source = MacSource::new();
        let diag = source.diagnostics();
        assert!(diag.engine_version.contains("macOS IOKit"));
        assert!(diag.driver_report.contains("IOHIDEventSystem"));
        assert!(diag.driver_report.contains("libIOReport"));
    }
}
