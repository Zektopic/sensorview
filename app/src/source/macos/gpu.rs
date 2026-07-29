//! Apple integrated GPU load and memory, from the AGX accelerator's
//! `PerformanceStatistics` dictionary.
//!
//! The IOKit class name is generation-specific (`AGXAcceleratorG17G` on M5,
//! `AGXAcceleratorG16P` and so on earlier), so matching by exact class would
//! break on every new chip. Match the stable parent class `IOAccelerator`
//! instead and read the statistics off whatever concrete driver is attached.

use crate::model::{Hardware, HardwareType, SensorType};

use super::iokit::{self, dict_f64};
use super::sensor;

pub fn collect() -> Option<Hardware> {
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
    let service = services.first()?;
    let props = iokit::properties(service.0)?;
    let stats = iokit::dict_dict(&props, "PerformanceStatistics")?;

    let mut sensors = Vec::new();

    // "Device Utilization %" is the headline busy figure Activity Monitor
    // shows; the renderer/tiler split is useful detail underneath it.
    if let Some(v) = dict_f64(&stats, "Device Utilization %") {
        sensors.push(sensor("/gpu/0/load/0", "GPU Core", SensorType::Load, 0, v as f32));
    }
    if let Some(v) = dict_f64(&stats, "Renderer Utilization %") {
        sensors.push(sensor("/gpu/0/load/1", "GPU Renderer", SensorType::Load, 1, v as f32));
    }
    if let Some(v) = dict_f64(&stats, "Tiler Utilization %") {
        sensors.push(sensor("/gpu/0/load/2", "GPU Tiler", SensorType::Load, 2, v as f32));
    }

    // Unified memory, so this is a slice of system RAM rather than dedicated
    // VRAM — named accordingly so it isn't mistaken for a discrete pool.
    const MB: f64 = 1024.0 * 1024.0;
    if let Some(v) = dict_f64(&stats, "In use system memory") {
        sensors.push(sensor(
            "/gpu/0/smalldata/0",
            "GPU Memory In Use",
            SensorType::SmallData,
            0,
            (v / MB) as f32,
        ));
    }
    if let Some(v) = dict_f64(&stats, "Alloc system memory") {
        sensors.push(sensor(
            "/gpu/0/smalldata/1",
            "GPU Memory Allocated",
            SensorType::SmallData,
            1,
            (v / MB) as f32,
        ));
    }

    if sensors.is_empty() {
        return None;
    }

    // Prefer the marketing name from the registry; fall back to the SoC name.
    let name = iokit::dict_string(&props, "model")
        .or_else(|| {
            crate::sysinfo::sysctl_string("machdep.cpu.brand_string").map(|c| format!("{c} GPU"))
        })
        .unwrap_or_else(|| "Apple GPU".to_string());

    Some(Hardware {
        identifier: "/gpu/0".into(),
        name,
        hardware_type: HardwareType::GpuApple,
        sensors,
        sub_hardware: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_is_present_and_utilisation_is_a_percentage() {
        let Some(hw) = collect() else {
            return crate::source::macos::absent("IOAccelerator (integrated GPU)");
        };
        assert_eq!(hw.hardware_type, HardwareType::GpuApple);

        let load = hw
            .sensors
            .iter()
            .find(|s| s.sensor_type == SensorType::Load)
            .expect("at least one utilisation sensor");
        let v = load.value.unwrap();
        assert!((0.0..=100.0).contains(&v), "GPU utilisation {v} out of range");
    }
}
