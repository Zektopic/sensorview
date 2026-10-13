//! GPU load, memory and (on discrete Intel-Mac GPUs) temperature and clocks,
//! from each accelerator's `PerformanceStatistics` dictionary.
//!
//! The IOKit class name is generation- and vendor-specific
//! (`AGXAcceleratorG17G` on M5, `AMDRadeonX6000_AMDNavi14GraphicsAccelerator`,
//! `IntelAccelerator`, …), so matching by exact class would break on every
//! new chip. Match the stable parent class `IOAccelerator` instead and read
//! the statistics off whatever concrete driver is attached.
//!
//! Apple Silicon has exactly one accelerator. Intel Macs commonly have two —
//! the CPU's integrated graphics and a discrete AMD (or, before 2014, NVIDIA)
//! GPU — and each becomes its own node. The statistics keys differ by driver;
//! every one is optional, and a key a driver doesn't publish simply yields no
//! sensor. The AMD/Intel key names follow the Stats project's GPU reader
//! (github.com/exelban/stats, `Modules/GPU/reader.swift`).

use crate::model::{Hardware, HardwareType, SensorType};

use super::iokit::{self, dict_f64};
use super::sensor;

/// Vendor, from the driver class name — the same mapping Activity Monitor's
/// GPU history and the Stats project use.
pub fn vendor_of(io_class: &str) -> HardwareType {
    let c = io_class.to_ascii_lowercase();
    // `ati` only as a prefix (`ATIRadeonX2000`): as a substring it matches
    // half the English language ("Configuration", "Accelerati…").
    if c.contains("agx") {
        HardwareType::GpuApple
    } else if c.contains("amd") || c.contains("radeon") || c.starts_with("ati") {
        HardwareType::GpuAti
    } else if c.contains("intel") {
        HardwareType::GpuIntel
    } else if c.contains("nvidia") || c.starts_with("nv") || c.starts_with("gf") || c.starts_with("gk") {
        HardwareType::GpuNvidia
    } else {
        HardwareType::Unknown
    }
}

/// Every GPU with performance statistics, in registry order.
pub fn collect() -> Vec<Hardware> {
    // `IOAccelerator` is the stable superclass; every AGX driver registers as
    // one. Fall back to the concrete AGX class in case matching by superclass
    // ever stops working.
    let services = {
        let s = iokit::matching_services("IOAccelerator");
        if s.is_empty() {
            iokit::matching_services("AGXAccelerator")
        } else {
            s
        }
    };

    let mut out = Vec::new();
    for service in &services {
        let Some(props) = iokit::properties(service.0) else {
            continue;
        };
        let Some(stats) = iokit::dict_dict(&props, "PerformanceStatistics") else {
            continue;
        };
        let index = out.len();
        let id = |kind: &str, n: u32| format!("/gpu/{index}/{kind}/{n}");

        let mut sensors = Vec::new();
        let mut push = |kind: &str, n: u32, name: &str, ty: SensorType, v: Option<f64>| {
            if let Some(v) = v.filter(|v| v.is_finite()) {
                sensors.push(sensor(&id(kind, n), name, ty, n, v as f32));
            }
        };

        // "Device Utilization %" is the headline busy figure Activity Monitor
        // shows; older AMD drivers call it "GPU Activity(%)". The
        // renderer/tiler split is useful detail underneath it on Apple GPUs.
        let busy = dict_f64(&stats, "Device Utilization %").or_else(|| dict_f64(&stats, "GPU Activity(%)"));
        push("load", 0, "GPU Core", SensorType::Load, busy);
        push("load", 1, "GPU Renderer", SensorType::Load, dict_f64(&stats, "Renderer Utilization %"));
        push("load", 2, "GPU Tiler", SensorType::Load, dict_f64(&stats, "Tiler Utilization %"));

        // Discrete Intel-Mac GPUs report their own sensors here; Apple GPUs
        // don't (their temperature comes from the SMC, their clock from DVFS
        // residency). A zero temperature is the driver's "not measured".
        push(
            "temperature",
            0,
            "GPU Core",
            SensorType::Temperature,
            dict_f64(&stats, "Temperature(C)").filter(|t| *t > 0.0),
        );
        push("clock", 0, "GPU Core", SensorType::Clock, dict_f64(&stats, "Core Clock(MHz)"));
        push("clock", 1, "GPU Memory", SensorType::Clock, dict_f64(&stats, "Memory Clock(MHz)"));
        push("control", 0, "GPU Fan", SensorType::Control, dict_f64(&stats, "Fan Speed(%)"));
        push("fan", 0, "GPU Fan", SensorType::Fan, dict_f64(&stats, "Fan Speed(RPM)"));

        // Apple GPUs use unified memory, so this is a slice of system RAM
        // rather than dedicated VRAM — named accordingly so it isn't mistaken
        // for a discrete pool. Discrete GPUs publish their VRAM use instead.
        const MB: f64 = 1024.0 * 1024.0;
        push(
            "smalldata",
            0,
            "GPU Memory In Use",
            SensorType::SmallData,
            dict_f64(&stats, "In use system memory").map(|v| v / MB),
        );
        push(
            "smalldata",
            1,
            "GPU Memory Allocated",
            SensorType::SmallData,
            dict_f64(&stats, "Alloc system memory").map(|v| v / MB),
        );
        push(
            "smalldata",
            2,
            "GPU VRAM Used",
            SensorType::SmallData,
            dict_f64(&stats, "vramUsedBytes").map(|v| v / MB),
        );
        push(
            "smalldata",
            3,
            "GPU VRAM Free",
            SensorType::SmallData,
            dict_f64(&stats, "vramFreeBytes").map(|v| v / MB),
        );

        if sensors.is_empty() {
            continue;
        }

        let io_class = iokit::object_class(service.0).unwrap_or_default();
        let hardware_type = vendor_of(&io_class);

        // The marketing name sits on the accelerator for Apple GPUs and on
        // the parent PCI device for discrete ones, so search upward.
        let name = iokit::search_property(service.0, "model")
            .and_then(|v| cf_text(&v))
            .or_else(|| {
                (hardware_type == HardwareType::GpuApple)
                    .then(|| crate::sysinfo::sysctl_string("machdep.cpu.brand_string").map(|c| format!("{c} GPU")))
                    .flatten()
            })
            .unwrap_or_else(|| "GPU".to_string());

        out.push(Hardware {
            identifier: format!("/gpu/{index}"),
            name,
            hardware_type,
            sensors,
            sub_hardware: Vec::new(),
        });
    }
    out
}

/// A registry value that is text, whether stored as `CFString` or as the
/// NUL-terminated `CFData` the device tree and PCI nodes use.
pub fn cf_text(value: &core_foundation::base::CFType) -> Option<String> {
    if let Some(s) = value.downcast::<core_foundation::string::CFString>() {
        let s = s.to_string();
        return (!s.trim().is_empty()).then(|| s.trim().to_string());
    }
    let data = value.downcast::<core_foundation::data::CFData>()?;
    let bytes: &[u8] = data.bytes();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_is_present_and_utilisation_is_a_percentage() {
        let gpus = collect();
        let Some(hw) = gpus.first() else {
            return crate::source::macos::absent("IOAccelerator (GPU)");
        };
        if crate::sysinfo::is_apple_silicon() {
            assert_eq!(hw.hardware_type, HardwareType::GpuApple);
        }

        let Some(load) = hw.sensors.iter().find(|s| s.sensor_type == SensorType::Load) else {
            return crate::source::macos::absent("GPU utilisation statistics");
        };
        let v = load.value.unwrap();
        assert!((0.0..=100.0).contains(&v), "GPU utilisation {v} out of range");
    }

    /// Identifiers are positional; two GPUs must never share one.
    #[test]
    fn every_gpu_gets_its_own_identifier() {
        let gpus = collect();
        let mut ids: Vec<_> = gpus.iter().map(|g| g.identifier.clone()).collect();
        ids.dedup();
        assert_eq!(ids.len(), gpus.len());
    }

    #[test]
    fn driver_classes_map_to_vendors() {
        assert_eq!(vendor_of("AGXAcceleratorG17G"), HardwareType::GpuApple);
        assert_eq!(vendor_of("AMDRadeonX6000_AMDNavi14GraphicsAccelerator"), HardwareType::GpuAti);
        assert_eq!(vendor_of("IntelAccelerator"), HardwareType::GpuIntel);
        assert_eq!(vendor_of("AppleIntelKBLGraphics"), HardwareType::GpuIntel);
        assert_eq!(vendor_of("nvAccelerator"), HardwareType::GpuNvidia);
        assert_eq!(vendor_of("ATIRadeonX2000"), HardwareType::GpuAti);
        // "Configuration" contains "ati"; it must not read as AMD.
        assert_eq!(vendor_of("IntelConfigurationAccelerator"), HardwareType::GpuIntel);
        assert_eq!(vendor_of("SomethingElse"), HardwareType::Unknown);
    }
}
