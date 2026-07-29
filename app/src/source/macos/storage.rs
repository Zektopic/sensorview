//! Internal SSD throughput, from `IOBlockStorageDriver`'s `Statistics`.
//!
//! The registry exposes cumulative byte counters, so throughput is a rate and
//! must be differenced between polls — the first snapshot reports nothing
//! rather than a since-boot average.

use std::time::Instant;

use crate::model::{Hardware, HardwareType, SensorType};

use super::iokit::{self, dict_i64};
use super::sensor;

#[derive(Clone, Copy)]
struct Counters {
    read_bytes: i64,
    write_bytes: i64,
    at: Instant,
}

pub struct StorageCollector {
    /// Previous cumulative counters, keyed by position in the service list.
    prev: Vec<Option<Counters>>,
}

impl StorageCollector {
    pub fn new() -> Self {
        Self { prev: Vec::new() }
    }

    pub fn collect(&mut self) -> Vec<Hardware> {
        let services = iokit::matching_services("IOBlockStorageDriver");
        if self.prev.len() != services.len() {
            self.prev = vec![None; services.len()];
        }

        let mut out = Vec::new();
        for (index, service) in services.iter().enumerate() {
            let Some(props) = iokit::properties(service.0) else {
                continue;
            };
            let Some(stats) = iokit::dict_dict(&props, "Statistics") else {
                continue;
            };

            let read_bytes = dict_i64(&stats, "Bytes (Read)").unwrap_or(0);
            let write_bytes = dict_i64(&stats, "Bytes (Write)").unwrap_or(0);
            let now = Counters { read_bytes, write_bytes, at: Instant::now() };

            let mut sensors = Vec::new();

            // Throughput needs a baseline; skip on the first poll for a device.
            if let Some(before) = self.prev[index] {
                let secs = now.at.duration_since(before.at).as_secs_f64();
                if secs > 0.0 {
                    const MB: f64 = 1024.0 * 1024.0;
                    let rate = |now: i64, before: i64| {
                        // saturating_sub: counters reset if a device re-enumerates.
                        (now.saturating_sub(before).max(0) as f64 / MB / secs) as f32
                    };
                    sensors.push(sensor(
                        &format!("/storage/{index}/throughput/0"),
                        "Read Rate",
                        SensorType::Throughput,
                        0,
                        rate(now.read_bytes, before.read_bytes),
                    ));
                    sensors.push(sensor(
                        &format!("/storage/{index}/throughput/1"),
                        "Write Rate",
                        SensorType::Throughput,
                        1,
                        rate(now.write_bytes, before.write_bytes),
                    ));
                }
            }
            self.prev[index] = Some(now);

            // Cumulative totals are useful on their own and need no baseline.
            const GB: f64 = 1024.0 * 1024.0 * 1024.0;
            sensors.push(sensor(
                &format!("/storage/{index}/data/0"),
                "Total Read",
                SensorType::Data,
                0,
                (read_bytes as f64 / GB) as f32,
            ));
            sensors.push(sensor(
                &format!("/storage/{index}/data/1"),
                "Total Written",
                SensorType::Data,
                1,
                (write_bytes as f64 / GB) as f32,
            ));

            // Model/serial live on an ancestor of the block-storage driver, so
            // this needs the recursive parent search rather than a plain lookup.
            let name = iokit::search_property(service.0, "Model")
                .and_then(|v| v.downcast::<core_foundation::string::CFString>())
                .map(|s| s.to_string().trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("Disk {index}"));

            out.push(Hardware {
                identifier: format!("/storage/{index}"),
                name,
                hardware_type: HardwareType::Storage,
                sensors,
                sub_hardware: Vec::new(),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throughput_appears_only_after_a_baseline() {
        let mut collector = StorageCollector::new();

        let first = collector.collect();
        if first.is_empty() {
            return crate::source::macos::absent("IOBlockStorageDriver");
        }
        assert!(
            first[0].sensors.iter().all(|s| s.sensor_type != SensorType::Throughput),
            "first poll must not invent a rate from cumulative counters"
        );

        std::thread::sleep(std::time::Duration::from_millis(120));
        let second = collector.collect();
        let rates: Vec<_> = second[0]
            .sensors
            .iter()
            .filter(|s| s.sensor_type == SensorType::Throughput)
            .collect();
        assert_eq!(rates.len(), 2, "read + write rate on the second poll");
        for s in rates {
            let v = s.value.unwrap();
            // 100 GB/s would mean the delta or the interval is being misread.
            assert!((0.0..100_000.0).contains(&v), "{} = {v} MB/s implausible", s.name);
        }
    }
}
