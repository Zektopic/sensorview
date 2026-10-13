//! Native macOS sensor backend — Apple Silicon first, Intel where the same
//! mechanisms reach.
//!
//! Unlike the Windows path, this needs **no sidecar process, no kernel driver
//! and no elevation** — every source below is readable by an ordinary user:
//!
//! | Data                       | Mechanism                          | Apple Silicon | Intel |
//! |----------------------------|------------------------------------|---------------|-------|
//! | Die temperatures (per probe) | `IOHIDEventSystemClient` (SPI)   | yes           | few   |
//! | CPU/GPU/ANE power          | `libIOReport` (SPI, dlopen'd)      | yes           | no    |
//! | Clusters' clocks           | IOReport DVFS residency            | yes           | no    |
//! | Fans, system/DC-in power, CPU/GPU die aggregates | `AppleSMC` user client | yes | yes (all temps, power, voltages) |
//! | CPU load, memory           | Mach `host_*` (public)             | yes           | yes   |
//! | GPU utilisation/memory     | IOKit `IOAccelerator` (public)     | yes           | yes, per GPU |
//! | Disk throughput            | IOKit `IOBlockStorageDriver`       | yes           | yes   |
//! | Battery                    | IOKit `AppleSmartBattery`          | yes           | yes   |
//!
//! Two of those are private API. That is fine for `.dmg` distribution and for
//! notarization, but it rules out the Mac App Store, and Apple can change or
//! remove them in any macOS release. Because the release profile sets
//! `panic = "abort"`, an unavailable API must never be a panic: every collector
//! degrades to producing no sensors, and [`SensorSource::diagnostics`] reports
//! which subsystems resolved so the Settings dialog can explain a thin tree.
//!
//! The SMC *does* exist on M-series Macs, which this backend used to deny: it
//! is registered as `AppleSMCKeysEndpoint`, a subclass of `AppleSMC`, so
//! `ioreg -n AppleSMC` (an exact *name* match) finds nothing while
//! `IOServiceMatching("AppleSMC")` finds it. See `smc.rs`. Fans are read from
//! there; the M5 this was developed on is fanless, so the fan path is covered
//! by unit tests of the decoder rather than by live hardware.

pub mod battery;
pub mod display;
pub mod dvfs;
pub mod dynlib;
pub mod freq;
pub mod gpu;
pub mod hid;
pub mod inventory;
pub mod iokit;
pub mod ioreport;
pub mod load;
pub mod smc;
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
    smc: smc::SmcSensors,
    load: load::LoadCollector,
    storage: storage::StorageCollector,
    /// Apple Silicon hardware (decided at runtime; see `is_apple_silicon`).
    arm: bool,
    /// Name for the board-level node: the marketing name where the registry
    /// has one, else the model identifier.
    board_name: String,
    /// Cached at construction: which subsystems came up.
    report: String,
}

impl MacSource {
    pub fn new() -> Self {
        let hid = hid::HidSensors::new();
        let energy = ioreport::EnergyReporter::new();
        let clocks = freq::FrequencyReporter::new();
        let smc = smc::SmcSensors::new();
        let arm = crate::sysinfo::is_apple_silicon();

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
        lines.push(format!("AppleSMC (fans, board power): {}", smc.describe()));
        lines.push(
            "No kernel driver or elevation is required on macOS; all sensors are read via IOKit."
                .to_string(),
        );

        let board_name = sysprofile::product_name()
            .or_else(|| crate::sysinfo::sysctl_string("hw.model"))
            .unwrap_or_else(|| "Mac".to_string());

        Self {
            hid,
            energy,
            clocks,
            smc,
            load: load::LoadCollector::new(),
            storage: storage::StorageCollector::new(),
            arm,
            board_name,
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

        let mut smc = self.smc.sample();

        // The SMC's die-temperature aggregates lead the temperature block:
        // they are the headline figures, and the per-probe HID list after
        // them runs to dozens of rows.
        let mut soc_sensors = self.load.cpu_load_sensors();
        soc_sensors.append(&mut soc_clocks);
        soc_sensors.append(&mut soc_power);
        soc_sensors.append(&mut smc.cpu);
        soc_sensors.extend(self.hid.temperatures());
        disambiguate(&mut soc_sensors);

        if !soc_sensors.is_empty() {
            let brand = crate::sysinfo::sysctl_string("machdep.cpu.brand_string");
            let (identifier, name) = if self.arm {
                ("/applesoc/0", brand.unwrap_or_else(|| "Apple Silicon".to_string()))
            } else {
                ("/intelcpu/0", brand.unwrap_or_else(|| "Intel CPU".to_string()))
            };
            tree.push(Hardware {
                identifier: identifier.into(),
                name,
                hardware_type: HardwareType::Cpu,
                sensors: soc_sensors,
                sub_hardware: Vec::new(),
            });
        }

        // DVFS clocks, IOReport power and SMC die temperatures for the GPU
        // all describe Apple's integrated GPU, so they join that node only —
        // never a discrete GPU on an Intel Mac. If no node took them they
        // fall through to the board node rather than being dropped.
        let mut gpus = gpu::collect();
        match gpus.iter_mut().find(|g| g.hardware_type == HardwareType::GpuApple) {
            Some(apple) => {
                apple.sensors.append(&mut gpu_clocks);
                apple.sensors.append(&mut gpu_power);
                apple.sensors.append(&mut smc.gpu);
            }
            None => smc.system.append(&mut smc.gpu),
        }
        tree.extend(gpus);

        let mut memory_sensors = self.load.memory_sensors();
        memory_sensors.append(&mut ram_power);
        if !memory_sensors.is_empty() {
            tree.push(Hardware {
                identifier: "/ram/0".into(),
                name: if self.arm { "Unified Memory" } else { "Memory" }.into(),
                hardware_type: HardwareType::Ram,
                sensors: memory_sensors,
                sub_hardware: Vec::new(),
            });
        }

        // Fans, total system power and the DC-in rail are properties of the
        // machine, not of any one chip.
        if !smc.system.is_empty() {
            tree.push(Hardware {
                identifier: "/mainboard/0".into(),
                name: self.board_name.clone(),
                hardware_type: HardwareType::Mainboard,
                sensors: smc.system,
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

        if tree.is_empty() {
            return absent("any macOS sensor subsystem");
        }

        // Which nodes exist depends entirely on what the machine exposes, and
        // CI runs macOS virtualized. Report what is missing rather than
        // asserting hardware into existence; the value assertions below are
        // what actually guard correctness.
        let types: Vec<_> = tree.iter().map(|h| h.hardware_type).collect();
        for (ty, label) in [
            (HardwareType::Cpu, "SoC node"),
            (HardwareType::Ram, "memory node"),
            (HardwareType::GpuApple, "GPU node"),
            (HardwareType::Storage, "storage node"),
        ] {
            if !types.contains(&ty) {
                absent(label);
            }
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
        if !tree.iter().any(|hw| hw.sensors.iter().any(|s| s.value.is_some())) {
            absent("any live reading");
        }
    }

    /// The SoC node is the headline: it must carry temperature, power and load
    /// together, which is what proves both SPI paths resolved.
    #[test]
    fn soc_node_has_temperature_power_and_load() {
        let mut source = MacSource::new();
        let _ = source.snapshot();
        std::thread::sleep(std::time::Duration::from_millis(250));
        let tree = source.snapshot();

        let Some(soc) = tree.iter().find(|h| h.hardware_type == HardwareType::Cpu) else {
            return absent("SoC node");
        };
        for wanted in [SensorType::Load, SensorType::Temperature, SensorType::Power] {
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
        if baseline.is_empty() {
            return absent("any macOS sensors");
        }

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
        assert!(diag.driver_report.contains("AppleSMC"));
    }
}
