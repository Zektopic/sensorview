//! Static machine facts for the System Summary window.
//!
//! These are one-shot IOKit lookups that don't belong in a sensor collector:
//! the marketing model name, the boot firmware version, memory and SoC
//! identity, boot security, GPU identity, the drive list and the battery's
//! health. `sysinfo::query()` reads them once at startup.
//!
//! Apple Silicon describes itself through the ARM device tree (`/chosen`,
//! `/product`, the root node). Intel Macs have no such tree for most of this
//! and publish SMBIOS instead, read by [`smbios_table`]. Each helper returns
//! `None` where its source doesn't exist, so the Summary shows "—" rather
//! than a guess.
//!
//! Kept here rather than in `sysinfo.rs` so all the IOKit FFI stays behind the
//! one `iokit` helper module.

use super::iokit;
use crate::sysinfo::{BatteryInfo, DriveInfo};

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

/// A device-tree property decoded as text, e.g. `/chosen` → `dram-type`.
fn dt_string(path: &str, key: &str) -> Option<String> {
    let entry = iokit::entry_from_path(path)?;
    let props = iokit::properties(entry.0)?;
    iokit::dict_string(&props, key)
}

/// A device-tree property holding a little-endian `u32` (`<01000000>`).
fn dt_u32(path: &str, key: &str) -> Option<u32> {
    let entry = iokit::entry_from_path(path)?;
    let props = iokit::properties(entry.0)?;
    let bytes = iokit::dict_data(&props, key)?;
    Some(u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?))
}

/// Unified-memory technology and vendor, e.g. `("LPDDR5", "Samsung")`.
///
/// iBoot records both in `/chosen` when it trains the DRAM; this is where
/// System Information's "Type" and "Manufacturer" come from.
pub fn memory_identity() -> (Option<String>, Option<String>) {
    (dt_string("IODeviceTree:/chosen", "dram-type"), dt_string("IODeviceTree:/chosen", "dram-vendor"))
}

/// The SoC's part number, e.g. `"T8142"` for M5 — Apple's own name for the
/// die, and the nearest thing Apple Silicon has to a CPU codename.
///
/// Only a value shaped like one (`T` or `S` plus digits) is returned: a VM's
/// or an Intel Mac's device tree can carry something else under that name,
/// and the callers fall back to the model id rather than show it.
pub fn soc_part() -> Option<String> {
    dt_string("IODeviceTree:/", "platform-name")
        .map(|p| p.trim().to_ascii_uppercase())
        .filter(|p| looks_like_soc_part(p))
}

pub fn looks_like_soc_part(p: &str) -> bool {
    let mut chars = p.chars();
    matches!(chars.next(), Some('T' | 'S'))
        && p.len() >= 4
        && p.len() <= 6
        && chars.all(|c| c.is_ascii_digit())
}

/// The logic-board identifier, e.g. `"J813"`.
pub fn board_id() -> Option<String> {
    dt_string("IODeviceTree:/", "target-type")
}

/// Whether iBoot enforced a verified boot chain (`/chosen` → `secure-boot`).
/// `None` on Intel Macs, which don't publish it.
pub fn secure_boot() -> Option<bool> {
    dt_u32("IODeviceTree:/chosen", "secure-boot").map(|v| v != 0)
}

/// System Integrity Protection: `Some(true)` only when every protection is
/// on, which is what `csrutil status` reports as "enabled".
///
/// `csr_get_active_config` is exported by libsystem_kernel but is not in the
/// public headers, so it is resolved at runtime like the other SPI here.
pub fn sip_enabled() -> Option<bool> {
    // int csr_get_active_config(uint32_t *config);
    type CsrGetActiveConfig = unsafe extern "C" fn(*mut u32) -> i32;
    let f = unsafe { super::dynlib::global_symbol::<CsrGetActiveConfig>("csr_get_active_config") }?;
    let mut config = 0u32;
    (unsafe { f(&mut config) } == 0).then_some(config == 0)
}

/// Physical drives — internal and external — with model, bus and size.
///
/// Walks whole-disk `IOMedia` nodes and reads identity off their
/// `IOBlockStorageDevice` ancestor, which every storage driver publishes
/// (NVMe, AHCI/SATA, USB, Thunderbolt). That replaces matching
/// `IONVMeController` and pairing capacities by position, which missed SATA
/// and external drives entirely. Disk images and APFS containers are not
/// drives and are skipped.
pub fn drives() -> Vec<DriveInfo> {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let mut out = Vec::new();
    for service in iokit::matching_services("IOMedia") {
        // Exact class: `AppleAPFSMedia` synthesized volumes are IOMedia
        // subclasses that also say "Whole" (see inventory.rs).
        if iokit::object_class(service.0).as_deref() != Some("IOMedia") {
            continue;
        }
        let Some(props) = iokit::properties(service.0) else {
            continue;
        };
        if iokit::dict_bool(&props, "Whole") != Some(true) {
            continue;
        }
        let size = iokit::dict_i64(&props, "Size").filter(|s| *s > 0).map(|s| s as u64);

        let device = iokit::search_property(service.0, "Device Characteristics")
            .and_then(|v| iokit::as_dict(&v));
        let protocol = iokit::search_property(service.0, "Protocol Characteristics")
            .and_then(|v| iokit::as_dict(&v));
        let interconnect =
            protocol.as_ref().and_then(|p| iokit::dict_string(p, "Physical Interconnect")).unwrap_or_default();
        let location = protocol
            .as_ref()
            .and_then(|p| iokit::dict_string(p, "Physical Interconnect Location"))
            .unwrap_or_default();
        // Mounted disk images are "Virtual Interface"; RAM disks have no
        // characteristics at all. Neither is hardware.
        if interconnect.is_empty() || interconnect.eq_ignore_ascii_case("Virtual Interface") {
            continue;
        }

        let vendor = device.as_ref().and_then(|d| iokit::dict_string(d, "Vendor Name")).unwrap_or_default();
        let product = device.as_ref().and_then(|d| iokit::dict_string(d, "Product Name")).unwrap_or_default();
        let model = match (vendor.trim(), product.trim()) {
            ("", "") => iokit::dict_string(&props, "IOMediaName").unwrap_or_else(|| "Disk".to_string()),
            ("", p) => p.to_string(),
            (v, "") => v.to_string(),
            (v, p) if p.starts_with(v) => p.to_string(),
            (v, p) => format!("{v} {p}"),
        };

        out.push(DriveInfo {
            model,
            interface: interface_label(&interconnect, &location),
            size_gb: size.map(|b| b as f64 / GB),
        });
    }
    out
}

/// "Apple Fabric" is the internal Apple Silicon SSD's NVMe link; everything
/// else keeps the registry's own bus name. External drives say so.
pub fn interface_label(interconnect: &str, location: &str) -> String {
    let bus = match interconnect {
        "Apple Fabric" => "NVMe",
        "PCI-Express" | "PCI" => "PCIe",
        other => other,
    };
    if location.eq_ignore_ascii_case("External") {
        format!("{bus}, external")
    } else {
        bus.to_string()
    }
}

/// Battery identity and health, for the Summary's Battery panel.
pub fn battery() -> Option<BatteryInfo> {
    let services = iokit::matching_services("AppleSmartBattery");
    let props = iokit::properties(services.first()?.0)?;
    if iokit::dict_bool(&props, "BatteryInstalled") == Some(false) {
        return None;
    }
    let n = |key: &str| iokit::dict_i64(&props, key).filter(|v| *v >= 0).map(|v| v as u32);

    let design = n("DesignCapacity");
    // AppleRawMaxCapacity is the gauge's own full-charge estimate in mAh;
    // on Apple Silicon `MaxCapacity` is a percentage, so it can't be used.
    let full = n("AppleRawMaxCapacity").or_else(|| n("NominalChargeCapacity"));
    let health_pct = match (design, full) {
        (Some(d), Some(f)) if d > 0 => Some(f as f64 / d as f64 * 100.0),
        _ => None,
    };
    let charge_pct = match (n("CurrentCapacity"), n("MaxCapacity")) {
        (Some(c), Some(m)) if m > 0 => Some((c as f64 / m as f64 * 100.0).clamp(0.0, 100.0)),
        _ => None,
    };
    let adapter_watts = iokit::dict_dict(&props, "AdapterDetails")
        .and_then(|a| iokit::dict_i64(&a, "Watts"))
        .filter(|w| *w > 0)
        .map(|w| w as u32);

    Some(BatteryInfo {
        design_capacity_mah: design,
        full_charge_capacity_mah: full,
        cycle_count: n("CycleCount"),
        design_cycle_count: n("DesignCycleCount9C"),
        health_pct,
        charge_pct,
        charging: iokit::dict_bool(&props, "IsCharging"),
        external_power: iokit::dict_bool(&props, "ExternalConnected"),
        adapter_watts,
        gauge: iokit::dict_string(&props, "DeviceName").unwrap_or_default(),
    })
}

/// Discrete and integrated GPUs on an **Intel** Mac: `(name, vram_mb)` per
/// PCI display controller.
///
/// Must not be called on Apple Silicon, whose registry carries a synthetic
/// `"Unknown Unknown"` PCI display node for compatibility.
pub fn pci_gpus() -> Vec<(String, Option<u64>)> {
    iokit::matching_services("IOPCIDevice")
        .iter()
        .filter_map(|service| {
            let props = iokit::properties(service.0)?;
            // class-code is little-endian: 0x03xxxx is a display controller.
            let class = iokit::dict_data(&props, "class-code")?;
            let class = u32::from_le_bytes(class.get(..4)?.try_into().ok()?);
            if class >> 16 != 0x03 {
                return None;
            }
            let name = iokit::dict_string(&props, "model")?;
            let vram = iokit::dict_i64(&props, "VRAM,totalMB")
                .or_else(|| iokit::dict_i64(&props, "ATY,memsize").map(|b| b / (1024 * 1024)))
                .filter(|mb| *mb > 0)
                .map(|mb| mb as u64);
            Some((name, vram))
        })
        .collect()
}

/// The raw SMBIOS structure table on an Intel Mac — the same bytes
/// `dmidecode` reads, from `AppleSMBIOS`'s `"SMBIOS"` property (acidanthera's
/// macOS dmidecode port reads it from the same place). Apple Silicon has no
/// SMBIOS, so this is `None` there.
pub fn smbios_table() -> Option<Vec<u8>> {
    let services = iokit::matching_services("AppleSMBIOS");
    let props = iokit::properties(services.first()?.0)?;
    iokit::dict_data(&props, "SMBIOS").filter(|t| !t.is_empty())
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
        println!("  base/max : {:?} / {:?} MHz", info.cpu.base_clock_mhz, info.cpu.max_clock_mhz);
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
        let Some(drive) = drives.first() else {
            return crate::source::macos::absent("physical IOMedia");
        };
        assert!(!drive.model.is_empty());
        assert!(!drive.interface.is_empty());
        let Some(gb) = drive.size_gb else {
            return crate::source::macos::absent("whole-media size");
        };
        assert!(gb > 1.0, "capacity {gb} GB implausibly small for a disk");
    }

    /// Every physical disk the class-filtered IOMedia walk finds must be
    /// listed once — the old NVMe-controller match listed only the internal
    /// SSD, and paired sizes by position.
    #[test]
    fn drive_list_matches_physical_disks() {
        let listed = drives().len();
        let physical = super::super::inventory::physical_disk_sizes().len();
        // Disk images are physical-looking IOMedia without a real bus, so
        // the drive list may be shorter, never longer.
        assert!(listed <= physical, "{listed} drives listed for {physical} whole disks");
    }

    #[test]
    fn interface_labels() {
        assert_eq!(interface_label("Apple Fabric", "Internal"), "NVMe");
        assert_eq!(interface_label("PCI-Express", "Internal"), "PCIe");
        assert_eq!(interface_label("SATA", "Internal"), "SATA");
        assert_eq!(interface_label("USB", "External"), "USB, external");
    }

    /// The CFData decode must yield clean text (no NUL padding, no
    /// mojibake). What the text *says* is the machine's business — a VM may
    /// report something other than an LPDDR generation.
    #[test]
    fn memory_identity_decodes_to_clean_text() {
        let (kind, vendor) = memory_identity();
        if kind.is_none() && vendor.is_none() {
            return crate::source::macos::absent("/chosen dram-type and dram-vendor");
        }
        for text in [kind, vendor].into_iter().flatten() {
            assert!(!text.is_empty() && text.chars().all(|c| c.is_ascii_graphic() || c == ' '), "{text:?}");
        }
    }

    #[test]
    fn soc_part_numbers_are_recognised_and_junk_is_not() {
        // T8103 (M1) … T8142 (M5), T6000-series for Pro/Max.
        for good in ["T8103", "T8142", "T6031"] {
            assert!(looks_like_soc_part(good), "{good}");
        }
        for bad in ["", "J813", "T81X2", "VMA2MACOSAP", "T"] {
            assert!(!looks_like_soc_part(bad), "{bad}");
        }
        if let Some(part) = soc_part() {
            assert!(looks_like_soc_part(&part));
        }
    }

    #[test]
    fn battery_health_is_plausible() {
        let Some(b) = battery() else {
            return crate::source::macos::absent("AppleSmartBattery");
        };
        if let Some(h) = b.health_pct {
            assert!((10.0..=130.0).contains(&h), "battery health {h}% implausible");
        }
        if let (Some(full), Some(design)) = (b.full_charge_capacity_mah, b.design_capacity_mah) {
            assert!(full > 100 && design > 100, "capacities {full}/{design} mAh implausible");
        }
    }

    #[test]
    fn sip_status_resolves() {
        // Present on every supported macOS; the value itself is the user's
        // business, so only resolution is checked.
        if sip_enabled().is_none() {
            crate::source::macos::absent("csr_get_active_config");
        }
    }

    #[test]
    fn intel_only_sources_are_absent_on_apple_silicon() {
        if !crate::sysinfo::is_apple_silicon() {
            return;
        }
        assert!(smbios_table().is_none(), "Apple Silicon has no SMBIOS");
    }
}
