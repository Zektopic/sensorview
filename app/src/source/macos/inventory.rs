//! Slow-lane inventory for macOS: internal storage identity and capacity.
//!
//! Apple Silicon has no ACPI or SMBIOS tables (the firmware describes hardware
//! with an ARM device tree), so unlike Windows and Linux there is nothing here
//! for the Hex Viewer to dump — `FirmwareTables` is deliberately not used.
//!
//! Scope limit worth being explicit about: this reports drive **identity** from
//! IOKit, not the full NVMe Health Information Log (log page 0x02). Apple's
//! internal controller (`AppleANS3CGv2Controller`) is not a standard NVMe
//! endpoint and the log page is not reachable through the public registry, so
//! `power_on_hours`, `life_remaining_pct` and the rest stay `None` rather than
//! being invented. Drive temperature is already published as a live sensor
//! (`NAND CH0 temp`) by the fast lane.

use crate::inventory::{Inventory, InventorySource};
use crate::model::storage::{HealthStatus, StorageHealth, StorageProtocol};

use super::iokit::{self, dict_i64, dict_string};

pub struct MacInventory;

impl InventorySource for MacInventory {
    fn name(&self) -> &'static str {
        "macOS IOKit"
    }

    fn collect(&mut self) -> Inventory {
        Inventory { storage: collect_storage(), ..Default::default() }
    }
}

fn collect_storage() -> Vec<StorageHealth> {
    // The whole-media size is on IOMedia, not the controller, so pair them up
    // by order — Apple Silicon has exactly one internal NVMe controller.
    let capacity = physical_disk_sizes();

    iokit::matching_services("IONVMeController")
        .iter()
        .enumerate()
        .filter_map(|(index, service)| {
            let props = iokit::properties(service.0)?;
            let model = dict_string(&props, "Model Number").unwrap_or_default();
            // A controller with no model string is not something we can
            // meaningfully report on.
            if model.is_empty() {
                return None;
            }
            Some(StorageHealth {
                identifier: format!("/storage/{index}"),
                model,
                serial: dict_string(&props, "Serial Number").unwrap_or_default(),
                firmware: dict_string(&props, "Firmware Revision").unwrap_or_default(),
                protocol: StorageProtocol::Nvme,
                capacity_bytes: capacity.get(index).copied(),
                // Everything below needs the NVMe health log — see module docs.
                temperature_c: None,
                power_on_hours: None,
                power_cycles: None,
                life_remaining_pct: None,
                total_bytes_written: None,
                total_bytes_read: None,
                // Not "Good": we have no health data, and claiming a healthy
                // drive on no evidence is worse than admitting we don't know.
                status: HealthStatus::Unknown,
                warnings: Vec::new(),
                attributes: Vec::new(),
                nvme: None,
            })
        })
        .collect()
}

/// Sizes of the *physical* whole disks, largest first.
///
/// Two filters are needed, and neither alone is sufficient:
///
/// - `"Whole" = Yes` drops partitions (`disk0s1`, …).
/// - An exact class check drops APFS synthesized volumes. `IOServiceMatching`
///   matches subclasses, so asking for `IOMedia` also returns every
///   `AppleAPFSMedia` container — and those report `"Whole" = Yes` too, so on
///   this machine the naive version reported four "disks" (500 GB physical
///   plus 494 GB, 5.4 GB and 577 MB APFS containers).
pub fn physical_disk_sizes() -> Vec<u64> {
    let mut sizes: Vec<u64> = iokit::matching_services("IOMedia")
        .iter()
        .filter_map(|service| {
            if iokit::object_class(service.0).as_deref() != Some("IOMedia") {
                return None;
            }
            let props = iokit::properties(service.0)?;
            if iokit::dict_bool(&props, "Whole") != Some(true) {
                return None;
            }
            dict_i64(&props, "Size").filter(|s| *s > 0).map(|s| s as u64)
        })
        .collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    sizes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_drive_is_identified() {
        let drives = collect_storage();
        if drives.is_empty() {
            return crate::source::macos::absent("IONVMeController");
        }

        let drive = &drives[0];
        assert!(!drive.model.is_empty(), "model number should be populated");
        assert_eq!(drive.protocol, StorageProtocol::Nvme);

        // Capacity must look like a real SSD, not a partition or a byte count
        // that got mistaken for something else.
        let bytes = drive.capacity_bytes.expect("whole-media size");
        assert!(
            bytes > 100_000_000_000,
            "capacity {bytes} bytes is too small to be the internal disk"
        );
    }

    /// Health fields are deliberately unset; this pins that down so nobody
    /// later reports a cheerful "Good" without actually reading the log page.
    #[test]
    fn health_is_reported_as_unknown_not_invented() {
        for drive in collect_storage() {
            assert_eq!(drive.status, HealthStatus::Unknown);
            assert!(drive.power_on_hours.is_none());
            assert!(drive.life_remaining_pct.is_none());
        }
    }

    /// Regression test for the two-filter rule in `whole_media_capacities`.
    /// Before the class check this returned four entries on an M5 Air — the
    /// real 500 GB disk plus three APFS synthesized containers.
    #[test]
    fn apfs_containers_and_partitions_are_not_counted_as_disks() {
        let sizes = physical_disk_sizes();
        if sizes.is_empty() {
            return crate::source::macos::absent("physical IOMedia");
        }
        assert!(
            sizes.iter().all(|s| *s > 10_000_000_000),
            "a partition or APFS container leaked in: {sizes:?}"
        );
        // One internal SSD, and no synthesized duplicates of it.
        assert_eq!(sizes.len(), 1, "expected exactly one physical disk, got {sizes:?}");
    }
}
