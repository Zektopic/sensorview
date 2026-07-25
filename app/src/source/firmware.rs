//! Firmware table dumps for the Hex Viewer — ACPI and SMBIOS.
//!
//! # Why these tables, and not PCI config space
//!
//! The obvious hex-viewer target is PCI configuration space, but reading it
//! means port I/O through a kernel driver — the same WinRing0 path that
//! Windows' vulnerable-driver blocklist blocks (see the Driver Management tab).
//! ACPI and SMBIOS come out of `GetSystemFirmwareTable`, a plain kernel32 call
//! that needs **no driver and no elevation**, and yields multi-kilobyte dumps
//! with real structure in them. So the viewer is useful today, on a machine
//! with no working Ring-0 driver at all.
//!
//! On Linux the same tables are files under `/sys/firmware/`, though reading
//! them usually requires root.
//!
//! Collected on the slow lane: firmware tables are fixed at boot.

use crate::inventory::{Inventory, InventorySource};
#[allow(unused_imports)]
use crate::model::hexblob::{HexBlob, HexRegion, HexSource, RegionKind};

/// Enumerates the machine's firmware tables once per slow-lane pass.
pub struct FirmwareTables;

impl InventorySource for FirmwareTables {
    fn name(&self) -> &'static str {
        "firmware tables"
    }

    fn collect(&mut self) -> Inventory {
        Inventory { hex: read_all(), ..Default::default() }
    }
}

fn read_all() -> Vec<HexBlob> {
    #[allow(unused_mut)]
    let mut out = Vec::new();
    #[cfg(windows)]
    {
        out.extend(windows_impl::acpi_tables());
        out.extend(windows_impl::smbios());
    }
    #[cfg(target_os = "linux")]
    {
        out.extend(linux_impl::acpi_tables());
    }
    out
}

// ---------------------------------------------------------------------------
// Structure annotation
// ---------------------------------------------------------------------------

/// Length of the standard ACPI System Description Table header.
#[allow(dead_code)]
const ACPI_HEADER_LEN: usize = 36;

/// Label the fields of the ACPI header (ACPI spec §21.2.1), so the viewer can
/// tint them and name whatever is under the cursor.
#[allow(dead_code)]
pub fn acpi_header_regions(bytes: &[u8]) -> Vec<HexRegion> {
    if bytes.len() < ACPI_HEADER_LEN {
        return Vec::new();
    }
    let r = |start: usize, len: usize, label: &str, kind: RegionKind| HexRegion {
        start,
        len,
        label: label.to_string(),
        kind,
    };
    let mut regions = vec![
        r(0, ACPI_HEADER_LEN, "ACPI header", RegionKind::Payload),
        r(0, 4, "Signature", RegionKind::Identity),
        r(4, 4, "Length", RegionKind::Length),
        r(8, 1, "Revision", RegionKind::Checksum),
        r(9, 1, "Checksum", RegionKind::Checksum),
        r(10, 6, "OEM ID", RegionKind::Identity),
        r(16, 8, "OEM Table ID", RegionKind::Identity),
        r(24, 4, "OEM Revision", RegionKind::Checksum),
        r(28, 4, "Creator ID", RegionKind::Identity),
        r(32, 4, "Creator Revision", RegionKind::Checksum),
    ];
    if bytes.len() > ACPI_HEADER_LEN {
        regions.push(r(
            ACPI_HEADER_LEN,
            bytes.len() - ACPI_HEADER_LEN,
            "Table data",
            RegionKind::Payload,
        ));
    }
    regions
}

/// The 8-byte `RawSMBIOSData` header Windows prepends to the DMI blob.
#[allow(dead_code)]
pub fn smbios_header_regions(bytes: &[u8]) -> Vec<HexRegion> {
    if bytes.len() < 8 {
        return Vec::new();
    }
    let r = |start: usize, len: usize, label: &str, kind: RegionKind| HexRegion {
        start,
        len,
        label: label.to_string(),
        kind,
    };
    let mut regions = vec![
        r(0, 1, "Used 2.0 calling method", RegionKind::Checksum),
        r(1, 1, "SMBIOS major version", RegionKind::Identity),
        r(2, 1, "SMBIOS minor version", RegionKind::Identity),
        r(3, 1, "DMI revision", RegionKind::Checksum),
        r(4, 4, "Table length", RegionKind::Length),
    ];
    if bytes.len() > 8 {
        regions.push(r(8, bytes.len() - 8, "DMI structure table", RegionKind::Payload));
    }
    regions
}

/// ACPI signature/OEM fields are fixed-length ASCII; render them readably and
/// fall back to hex for the (malformed) non-printable case.
#[allow(dead_code)]
pub fn ascii_tag(bytes: &[u8]) -> String {
    if bytes.iter().all(|&b| (0x20..0x7f).contains(&b)) {
        String::from_utf8_lossy(bytes).trim_end().to_string()
    } else {
        bytes.iter().map(|b| format!("{b:02X}")).collect()
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod windows_impl {
    use super::*;

    #[link(name = "kernel32")]
    extern "system" {
        fn EnumSystemFirmwareTables(provider: u32, buffer: *mut u8, size: u32) -> u32;
        fn GetSystemFirmwareTable(provider: u32, table: u32, buffer: *mut u8, size: u32) -> u32;
    }

    /// Provider signatures are the 4-character code packed **big-endian**:
    /// 'ACPI' is 0x41435049, not the little-endian 0x49504341 you get from
    /// reinterpreting the bytes. Getting this backwards is the classic way to
    /// have every call return 0 with no error.
    const fn provider(tag: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*tag)
    }

    const ACPI: u32 = provider(b"ACPI");
    const RSMB: u32 = provider(b"RSMB");

    /// Refuse absurd allocations if the API ever reports a nonsense size.
    const MAX_TABLE_BYTES: u32 = 16 * 1024 * 1024;

    fn read_table(provider_sig: u32, table_id: u32) -> Option<Vec<u8>> {
        unsafe {
            // First call with a null buffer asks for the required size.
            let size = GetSystemFirmwareTable(provider_sig, table_id, std::ptr::null_mut(), 0);
            if size == 0 || size > MAX_TABLE_BYTES {
                return None;
            }
            let mut buf = vec![0u8; size as usize];
            let written = GetSystemFirmwareTable(provider_sig, table_id, buf.as_mut_ptr(), size);
            if written == 0 {
                return None;
            }
            // A second call can legitimately return less than the probe did.
            buf.truncate(written.min(size) as usize);
            Some(buf)
        }
    }

    /// Every ACPI table the firmware published.
    pub fn acpi_tables() -> Vec<HexBlob> {
        let ids = unsafe {
            let size = EnumSystemFirmwareTables(ACPI, std::ptr::null_mut(), 0);
            if size == 0 || size > MAX_TABLE_BYTES {
                return Vec::new();
            }
            let mut buf = vec![0u8; size as usize];
            let written = EnumSystemFirmwareTables(ACPI, buf.as_mut_ptr(), size);
            if written == 0 {
                return Vec::new();
            }
            buf.truncate(written.min(size) as usize);
            // The enumeration is an array of table IDs (4-byte signatures).
            buf.chunks_exact(4)
                .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect::<Vec<u32>>()
        };

        // Firmware typically declares a dozen-plus SSDTs, but this API keys
        // tables by *signature* — asking for 'SSDT' fifteen times returns the
        // same bytes fifteen times. Read each signature once and report how
        // many the firmware declared, rather than listing identical copies as
        // if they were distinct tables.
        let mut declared: std::collections::HashMap<u32, usize> = Default::default();
        let mut order: Vec<u32> = Vec::new();
        for id in ids {
            let seen_before = declared.entry(id).or_insert(0);
            if *seen_before == 0 {
                order.push(id);
            }
            *seen_before += 1;
        }

        let mut out = Vec::new();
        for id in order {
            let Some(bytes) = read_table(ACPI, id) else { continue };
            // The table's own header carries its signature — more trustworthy
            // than re-deriving it from the enumerated DWORD's byte order.
            let signature = if bytes.len() >= 4 {
                ascii_tag(&bytes[..4])
            } else {
                ascii_tag(&id.to_ne_bytes())
            };
            let regions = acpi_header_regions(&bytes);
            out.push(
                HexBlob::new(
                    HexSource::AcpiTable {
                        signature,
                        index: 0,
                        of: declared.get(&id).copied().unwrap_or(1),
                    },
                    0,
                    bytes,
                )
                .with_regions(regions),
            );
        }
        out
    }

    /// The raw SMBIOS/DMI table.
    pub fn smbios() -> Vec<HexBlob> {
        let Some(bytes) = read_table(RSMB, 0) else { return Vec::new() };
        let version = if bytes.len() >= 3 {
            format!("{}.{}", bytes[1], bytes[2])
        } else {
            "?".into()
        };
        let regions = smbios_header_regions(&bytes);
        vec![HexBlob::new(HexSource::Smbios { version }, 0, bytes).with_regions(regions)]
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::*;

    /// `/sys/firmware/acpi/tables/*` — one file per table. Readable only by
    /// root on most distributions, so an empty result here is normal and not
    /// worth surfacing as an error.
    pub fn acpi_tables() -> Vec<HexBlob> {
        let Ok(dir) = std::fs::read_dir("/sys/firmware/acpi/tables") else {
            return Vec::new();
        };
        let mut entries: Vec<_> = dir
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| e.path())
            .collect();
        entries.sort();

        // Unlike the Windows API, sysfs exposes each duplicate SSDT as its own
        // file with its own contents, so index and count are both real here.
        let mut tables: Vec<(String, Vec<u8>)> = Vec::new();
        for path in entries {
            let Ok(bytes) = std::fs::read(&path) else { continue };
            if bytes.is_empty() {
                continue;
            }
            // Prefer the in-table signature; the filename encodes the duplicate
            // index (SSDT1, SSDT2, …) which we recompute anyway.
            let signature = if bytes.len() >= 4 {
                ascii_tag(&bytes[..4])
            } else {
                path.file_name().unwrap_or_default().to_string_lossy().to_string()
            };
            tables.push((signature, bytes));
        }

        let mut totals: std::collections::HashMap<String, usize> = Default::default();
        for (sig, _) in &tables {
            *totals.entry(sig.clone()).or_insert(0) += 1;
        }

        let mut seen: std::collections::HashMap<String, usize> = Default::default();
        let mut out = Vec::new();
        for (signature, bytes) in tables {
            let index = seen.entry(signature.clone()).or_insert(0);
            let of = totals.get(&signature).copied().unwrap_or(1);
            let regions = acpi_header_regions(&bytes);
            out.push(
                HexBlob::new(
                    HexSource::AcpiTable { signature: signature.clone(), index: *index, of },
                    0,
                    bytes,
                )
                .with_regions(regions),
            );
            *index += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but well-formed ACPI table: header + 4 bytes of payload.
    fn acpi_fixture() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"DSDT"); // signature
        b.extend_from_slice(&40u32.to_le_bytes()); // length
        b.push(2); // revision
        b.push(0x5A); // checksum
        b.extend_from_slice(b"ALASKA"); // OEM ID (6)
        b.extend_from_slice(b"A M I \0"); // OEM table ID (8, incl. the NUL)
        b.push(0);
        b.extend_from_slice(&1u32.to_le_bytes()); // OEM revision
        b.extend_from_slice(b"INTL"); // creator ID
        b.extend_from_slice(&0x2020_0110u32.to_le_bytes()); // creator revision
        b.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // payload
        assert_eq!(b.len(), 40);
        b
    }

    #[test]
    fn acpi_header_fields_land_at_spec_offsets() {
        let bytes = acpi_fixture();
        let blob = HexBlob::new(
            HexSource::AcpiTable { signature: "DSDT".into(), index: 0, of: 1 },
            0,
            bytes.clone(),
        )
        .with_regions(acpi_header_regions(&bytes));

        assert_eq!(blob.region_at(0).unwrap().label, "Signature");
        assert_eq!(blob.region_at(4).unwrap().label, "Length");
        assert_eq!(blob.region_at(9).unwrap().label, "Checksum");
        assert_eq!(blob.region_at(10).unwrap().label, "OEM ID");
        assert_eq!(blob.region_at(32).unwrap().label, "Creator Revision");
        // Past the 36-byte header the payload region takes over.
        assert_eq!(blob.region_at(36).unwrap().label, "Table data");
    }

    #[test]
    fn a_truncated_table_gets_no_annotations_rather_than_wrong_ones() {
        // Better to show plain hex than to label fields that aren't there.
        assert!(acpi_header_regions(&[0u8; 10]).is_empty());
        assert!(smbios_header_regions(&[0u8; 4]).is_empty());
        // A header with no payload has no "Table data" region.
        let header_only = acpi_header_regions(&[0u8; ACPI_HEADER_LEN]);
        assert!(!header_only.iter().any(|r| r.label == "Table data"));
    }

    #[test]
    fn ascii_tags_fall_back_to_hex_when_not_printable() {
        assert_eq!(ascii_tag(b"DSDT"), "DSDT");
        assert_eq!(ascii_tag(b"A M I   "), "A M I");
        assert_eq!(ascii_tag(&[0x00, 0xFF]), "00FF");
    }

    #[cfg(windows)]
    #[test]
    fn provider_signature_is_packed_big_endian() {
        // 'ACPI' == 0x41435049. The little-endian packing (0x49504341) is the
        // classic mistake and makes every call silently return zero.
        assert_eq!(u32::from_be_bytes(*b"ACPI"), 0x4143_5049);
        assert_eq!(u32::from_be_bytes(*b"RSMB"), 0x5253_4D42);
    }

    /// Exercises the real firmware on this machine when tests run on Windows.
    /// Asserts only what must hold on any conforming system, so it stays green
    /// on CI runners and inside VMs.
    #[cfg(windows)]
    #[test]
    fn reads_real_acpi_tables() {
        let blobs = windows_impl::acpi_tables();
        assert!(!blobs.is_empty(), "every x86 Windows machine publishes ACPI tables");

        for b in &blobs {
            // Each table's declared length must agree with what we read.
            assert!(b.bytes.len() >= 4, "table too short to hold a signature");
            if b.bytes.len() >= 8 {
                let declared =
                    u32::from_le_bytes([b.bytes[4], b.bytes[5], b.bytes[6], b.bytes[7]]) as usize;
                assert_eq!(
                    declared,
                    b.bytes.len(),
                    "{}: header length disagrees with the bytes returned",
                    b.source.label()
                );
            }
        }
        // The FADT ('FACP') is mandatory on every ACPI system and is one of the
        // few this API reliably exposes. Note the DSDT is deliberately *not*
        // asserted: Windows does not publish it through EnumSystemFirmwareTables
        // even though it exists — verified on this machine, which lists 29
        // tables without one.
        assert!(
            blobs.iter().any(|b| matches!(
                &b.source,
                HexSource::AcpiTable { signature, .. } if signature == "FACP"
            )),
            "FADT is mandatory; got {:?}",
            blobs.iter().map(|b| b.source.label()).collect::<Vec<_>>()
        );
    }

    /// The trap this API sets: ACPI tables are keyed by *signature*, so asking
    /// for 'SSDT' fifteen times returns the same bytes fifteen times. Listing
    /// those as fifteen tables would be a straight-up lie about the hardware.
    #[cfg(windows)]
    #[test]
    fn duplicate_signatures_are_collapsed_and_counted() {
        let blobs = windows_impl::acpi_tables();

        let mut labels: Vec<_> = blobs.iter().map(|b| b.source.label()).collect();
        let before = labels.len();
        labels.sort();
        labels.dedup();
        assert_eq!(before, labels.len(), "every entry must be distinct: {labels:?}");

        // No two entries may carry identical bytes.
        for (i, a) in blobs.iter().enumerate() {
            for b in &blobs[i + 1..] {
                assert_ne!(
                    a.bytes, b.bytes,
                    "{} and {} are byte-identical",
                    a.source.label(),
                    b.source.label()
                );
            }
        }

        // Where firmware declared duplicates, the count is surfaced rather than
        // silently dropped.
        if let Some(dup) = blobs.iter().find(
            |b| matches!(&b.source, HexSource::AcpiTable { of, .. } if *of > 1),
        ) {
            assert!(dup.source.label().contains(" of "), "{}", dup.source.label());
        }
    }
}

#[cfg(all(windows, test))]
mod probe {
    #[test]
    #[ignore = "diagnostic: prints what this machine's firmware actually publishes"]
    fn list_tables() {
        for b in super::windows_impl::acpi_tables() {
            println!("{:<20} {:>8} bytes", b.source.label(), b.bytes.len());
        }
        for b in super::windows_impl::smbios() {
            println!("{:<20} {:>8} bytes", b.source.label(), b.bytes.len());
        }
    }
}
