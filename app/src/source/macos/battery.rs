//! Battery telemetry from the `AppleSmartBattery` IOKit service.
//!
//! Unprivileged and present on every portable Mac; desktops simply match no
//! service and contribute no `Battery` node.

use crate::model::{Hardware, HardwareType, SensorType};

use super::iokit::{self, dict_i64};
use super::sensor;

/// `None` on desktop Macs (no battery service).
pub fn collect() -> Option<Hardware> {
    let services = iokit::matching_services("AppleSmartBattery");
    let props = iokit::properties(services.first()?.0)?;

    let mut sensors = Vec::new();

    // Reported in centi-degrees Celsius: the probe read 3057 => 30.57 °C.
    if let Some(raw) = dict_i64(&props, "Temperature") {
        sensors.push(sensor(
            "/battery/0/temperature/0",
            "Battery",
            SensorType::Temperature,
            0,
            raw as f32 / 100.0,
        ));
    }

    // Millivolts.
    if let Some(mv) = dict_i64(&props, "Voltage") {
        sensors.push(sensor(
            "/battery/0/voltage/0",
            "Battery",
            SensorType::Voltage,
            0,
            mv as f32 / 1000.0,
        ));
    }

    // Amperage is signed: negative while discharging, positive while charging.
    // It is stored two's-complement in an unsigned slot, so it MUST be read as
    // i64 — as u64 the probe value 18446744073709551264 (= -352) would render
    // as 1.8e16 A. See iokit::dict_i64.
    if let Some(ma) = dict_i64(&props, "Amperage") {
        sensors.push(sensor(
            "/battery/0/current/0",
            "Battery",
            SensorType::Current,
            0,
            ma as f32 / 1000.0,
        ));

        // Instantaneous power draw, sign-matched to the current.
        if let Some(mv) = dict_i64(&props, "Voltage") {
            sensors.push(sensor(
                "/battery/0/power/0",
                "Battery Rate",
                SensorType::Power,
                0,
                (ma as f32 / 1000.0) * (mv as f32 / 1000.0),
            ));
        }
    }

    // CurrentCapacity is a percentage when MaxCapacity is 100 (the modern
    // reporting style seen on this machine); older firmware reported mAh
    // against a real MaxCapacity, so derive the ratio rather than assuming.
    let current = dict_i64(&props, "CurrentCapacity");
    let max = dict_i64(&props, "MaxCapacity");
    if let (Some(current), Some(max)) = (current, max) {
        if max > 0 {
            sensors.push(sensor(
                "/battery/0/level/0",
                "Charge Level",
                SensorType::Level,
                0,
                (current as f32 / max as f32 * 100.0).clamp(0.0, 100.0),
            ));
        }
    }

    if let Some(cycles) = dict_i64(&props, "CycleCount") {
        sensors.push(sensor(
            "/battery/0/factor/0",
            "Cycle Count",
            SensorType::Factor,
            0,
            cycles as f32,
        ));
    }

    // Health: full-charge capacity against the original design capacity.
    let design = dict_i64(&props, "DesignCapacity");
    let full = dict_i64(&props, "AppleRawMaxCapacity").or_else(|| dict_i64(&props, "NominalChargeCapacity"));
    if let (Some(design), Some(full)) = (design, full) {
        if design > 0 {
            sensors.push(sensor(
                "/battery/0/level/1",
                "Battery Health",
                SensorType::Level,
                1,
                (full as f32 / design as f32 * 100.0).clamp(0.0, 200.0),
            ));
        }
    }

    if sensors.is_empty() {
        return None;
    }

    Some(Hardware {
        identifier: "/battery/0".into(),
        name: "Battery".into(),
        hardware_type: HardwareType::Battery,
        sensors,
        sub_hardware: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the signed-vs-unsigned trap: a naive u64 read of Amperage yields
    /// ~1.8e19 mA. Anything outside a few tens of amps means the sign handling
    /// regressed. Skipped on desktop Macs, which have no battery.
    #[test]
    fn battery_values_are_physically_plausible() {
        let Some(hw) = collect() else {
            return;
        };
        for s in &hw.sensors {
            let v = s.value.unwrap();
            assert!(v.is_finite(), "{} is not finite", s.name);
            let ok = match s.sensor_type {
                SensorType::Temperature => (-20.0..=100.0).contains(&v),
                SensorType::Voltage => (0.0..=30.0).contains(&v),
                SensorType::Current => (-30.0..=30.0).contains(&v),
                SensorType::Power => (-300.0..=300.0).contains(&v),
                SensorType::Level => (0.0..=200.0).contains(&v),
                SensorType::Factor => (0.0..=10000.0).contains(&v),
                _ => true,
            };
            assert!(ok, "{} = {v} {} is out of physical range", s.name, s.sensor_type.unit());
        }
    }
}
