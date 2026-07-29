//! Static machine facts for the System Summary window.
//!
//! These are one-shot IOKit lookups that don't belong in a sensor collector:
//! the marketing model name, the boot firmware version, GPU identity and the
//! internal drive list. `sysinfo::query()` reads them once at startup.
//!
//! Kept here rather than in `sysinfo.rs` so all the IOKit FFI stays behind the
//! one `iokit` helper module.

use super::iokit;

/// Marketing name, e.g. `"MacBook Air (13-inch, M5)"`.
///
/// Lives in the device tree, not on `IOPlatformExpertDevice` (which only
/// carries the board id `"Mac17,3"`). Stored as `CFData`, which `dict_string`
/// decodes.
pub fn product_name() -> Option<String> {
    let entry = iokit::entry_from_path("IODeviceTree:/product")?;
    let props = iokit::properties(entry.0)?;
    iokit::dict_string(&props, "product-name")
        .or_else(|| iokit::dict_string(&props, "product-description"))
}

/// Boot firmware version, e.g. `"mBoot-18000.121.3"` — the Apple Silicon
/// analogue of a BIOS version.
pub fn firmware_version() -> Option<String> {
    let entry = iokit::entry_from_path("IODeviceTree:/chosen")?;
    let props = iokit::properties(entry.0)?;
    iokit::dict_string(&props, "system-firmware-version")
}

/// GPU core count and Metal driver name, both best-effort.
pub fn gpu_identity() -> (Option<u32>, Option<String>) {
    let services = {
        let s = iokit::matching_services("IOAccelerator");
        if s.is_empty() {
            iokit::matching_services("AGXAccelerator")
        } else {
            s
        }
    };
    let Some(service) = services.first() else {
        return (None, None);
    };

    // The core count sits on an ancestor of the accelerator, so it needs the
    // recursive parent search rather than a direct property read.
    let cores = iokit::search_property(service.0, "gpu-core-count")
        .and_then(|v| v.downcast::<core_foundation::number::CFNumber>())
        .and_then(|n| n.to_i64())
        .filter(|n| *n > 0)
        .map(|n| n as u32);

    let driver = iokit::properties(service.0)
        .and_then(|props| iokit::dict_string(&props, "MetalPluginName"));

    (cores, driver)
}

/// Internal storage: `(model, capacity_bytes)` per physical NVMe drive.
pub fn drives() -> Vec<(String, Option<u64>)> {
    let capacities = super::inventory::physical_disk_sizes();
    iokit::matching_services("IONVMeController")
        .iter()
        .enumerate()
        .filter_map(|(index, service)| {
            let props = iokit::properties(service.0)?;
            let model = iokit::dict_string(&props, "Model Number")?;
            if model.is_empty() {
                return None;
            }
            Some((model, capacities.get(index).copied()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Diagnostic probe for the System Summary panels. Run with
    /// `cargo test -- --ignored --nocapture dump_system_summary`.
    #[test]
    #[ignore = "diagnostic probe; run explicitly"]
    fn dump_system_summary() {
        let info = crate::sysinfo::query_for_test();
        println!("\ncomputer   : {}", info.computer_name);
        println!("user       : {}", info.user_name);
        println!("--- Motherboard ---");
        println!("  product  : {} {}", info.board.manufacturer, info.board.product);
        println!("  firmware : {}", info.board.bios_version);
        println!("--- CPU ---");
        println!("  name     : {}", info.cpu.name);
        println!("  cores    : {:?} threads {:?}", info.cpu.cores, info.cpu.threads);
        println!("  layout   : {:?}", info.cpu.socket);
        println!("  codename : {}", info.cpu.codename);
        println!("  features : {} detected", info.cpu.features.iter().filter(|f| f.1).count());
        println!("--- Memory ---");
        println!("  total    : {:?} GB", info.total_memory_gb);
        for m in &info.memory_modules {
            println!("  module   : {} {} {:.0} GB", m.bank, m.memory_type, m.capacity_gb);
        }
        println!("--- GPU ---");
        for g in &info.gpus {
            println!("  {} (driver {})", g.name, g.driver_version);
        }
        println!("--- Drives ---");
        for d in &info.drives {
            println!("  {} {} {:?} GB", d.model, d.interface, d.size_gb.map(|g| g as u64));
        }
        println!("--- OS ---");
        println!("  {} build {} {}", info.os.caption, info.os.build, info.os.arch);
    }

    #[test]
    fn product_name_is_a_marketing_name_not_a_board_id() {
        let Some(name) = product_name() else {
            return crate::source::macos::absent("IODeviceTree:/product");
        };
        assert!(!name.is_empty());
        // "Mac17,3" is the board id; this lookup exists precisely to get past
        // it to the human-readable name.
        assert!(
            !name.starts_with("Mac") || name.contains(' '),
            "got the board id {name} instead of a marketing name"
        );
    }

    #[test]
    fn firmware_version_is_populated() {
        let Some(version) = firmware_version() else {
            return crate::source::macos::absent("system-firmware-version");
        };
        assert!(!version.is_empty());
        // Should carry a version number, not just a label.
        assert!(
            version.chars().any(|c| c.is_ascii_digit()),
            "firmware version {version} has no digits"
        );
    }

    #[test]
    fn gpu_core_count_is_plausible() {
        let (cores, _driver) = gpu_identity();
        let Some(cores) = cores else {
            return crate::source::macos::absent("gpu-core-count");
        };
        // Apple Silicon ranges from 7 cores (base M-series) to 80 (Ultra).
        assert!((4..=128).contains(&cores), "GPU core count {cores} is implausible");
    }

    #[test]
    fn drives_are_listed_with_capacity() {
        let drives = drives();
        if drives.is_empty() {
            return crate::source::macos::absent("IONVMeController");
        }
        let (model, size) = &drives[0];
        assert!(!model.is_empty());
        let bytes = size.expect("capacity");
        assert!(bytes > 100_000_000_000, "capacity {bytes} too small for an internal SSD");
    }
}
