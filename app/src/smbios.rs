//! A minimal SMBIOS structure-table parser.
//!
//! Intel Macs publish their DMI table through IOKit (`AppleSMBIOS`) rather
//! than through an OS API, so there is no WMI-style decoded view to lean on:
//! the Summary's memory modules and firmware rows come from parsing it here.
//! Layout references are DMTF DSP0134 (SMBIOS 3.x); every offset read below is
//! bounds-checked against the structure's own declared length, because older
//! tables carry shorter versions of the same structure types.
//!
//! Pure functions over bytes, so it is tested on every platform even though
//! only the macOS x86_64 path calls it.

#![allow(dead_code)] // Only an Intel Mac reaches this outside tests.

/// One structure: its header type, formatted area, and string set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Structure<'a> {
    pub kind: u8,
    pub handle: u16,
    /// The formatted area, header included (offsets match DSP0134's tables).
    pub data: &'a [u8],
    pub strings: Vec<String>,
}

impl Structure<'_> {
    pub fn byte(&self, offset: usize) -> Option<u8> {
        self.data.get(offset).copied()
    }

    pub fn word(&self, offset: usize) -> Option<u16> {
        Some(u16::from_le_bytes(self.data.get(offset..offset + 2)?.try_into().ok()?))
    }

    pub fn dword(&self, offset: usize) -> Option<u32> {
        Some(u32::from_le_bytes(self.data.get(offset..offset + 4)?.try_into().ok()?))
    }

    /// The string referenced by the index byte at `offset` (1-based; 0 means
    /// "none"). Blank and padding strings come back as `None`.
    pub fn string(&self, offset: usize) -> Option<String> {
        let index = self.byte(offset)? as usize;
        let s = self.strings.get(index.checked_sub(1)?)?.trim();
        (!s.is_empty()).then(|| s.to_string())
    }
}

/// Split a raw table into structures. Stops at the end-of-table marker (type
/// 127) or at the first structure that runs past the buffer — a truncated
/// table yields what it can rather than nothing.
pub fn parse(table: &[u8]) -> Vec<Structure<'_>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= table.len() {
        let kind = table[pos];
        let len = table[pos + 1] as usize;
        if len < 4 || pos + len > table.len() {
            break;
        }
        let handle = u16::from_le_bytes([table[pos + 2], table[pos + 3]]);
        let data = &table[pos..pos + len];

        // The string set: NUL-terminated strings, ended by an empty one. A
        // structure with no strings still has the double NUL.
        let mut strings = Vec::new();
        let mut cursor = pos + len;
        loop {
            let Some(rel_end) = table.get(cursor..).and_then(|rest| rest.iter().position(|&b| b == 0)) else {
                // Unterminated string set: keep the structure, stop parsing.
                out.push(Structure { kind, handle, data, strings });
                return out;
            };
            if rel_end == 0 {
                cursor += 1;
                break;
            }
            strings.push(String::from_utf8_lossy(&table[cursor..cursor + rel_end]).into_owned());
            cursor += rel_end + 1;
        }
        if strings.is_empty() {
            // Skip the second NUL of an empty string set.
            if table.get(cursor) == Some(&0) {
                cursor += 1;
            }
        }

        out.push(Structure { kind, handle, data, strings });
        if kind == 127 {
            break;
        }
        pos = cursor;
    }
    out
}

/// Firmware vendor/version/date (type 0).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Bios {
    pub vendor: Option<String>,
    pub version: Option<String>,
    pub date: Option<String>,
}

pub fn bios(structures: &[Structure]) -> Option<Bios> {
    let s = structures.iter().find(|s| s.kind == 0)?;
    Some(Bios { vendor: s.string(0x04), version: s.string(0x05), date: s.string(0x08) })
}

/// System manufacturer and product (type 1), e.g. "Apple Inc." /
/// "MacBookPro16,1".
pub fn system(structures: &[Structure]) -> Option<(Option<String>, Option<String>)> {
    let s = structures.iter().find(|s| s.kind == 1)?;
    Some((s.string(0x04), s.string(0x05)))
}

/// One populated memory slot (type 17).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct MemoryDevice {
    pub locator: Option<String>,
    pub bank: Option<String>,
    pub size_mb: u64,
    /// The raw SMBIOS memory-type code; `sysinfo` names it.
    pub memory_type: u8,
    pub speed_mts: Option<u32>,
    pub configured_speed_mts: Option<u32>,
    pub manufacturer: Option<String>,
    pub part_number: Option<String>,
    pub configured_voltage_mv: Option<u32>,
}

/// Populated memory devices. Empty slots (size 0) are skipped.
pub fn memory_devices(structures: &[Structure]) -> Vec<MemoryDevice> {
    structures
        .iter()
        .filter(|s| s.kind == 17)
        .filter_map(|s| {
            let size_mb = match s.word(0x0C)? {
                0 | 0xFFFF => return None, // empty slot / unknown
                // 0x7FFF: the real size (in MB) is in the extended field.
                0x7FFF => u64::from(s.dword(0x1C)? & 0x7FFF_FFFF),
                // Bit 15 set: the value is in KB.
                w if w & 0x8000 != 0 => u64::from(w & 0x7FFF) / 1024,
                w => u64::from(w),
            };
            if size_mb == 0 {
                return None;
            }
            // 0 means unknown; 0xFFFF defers to the 3.3+ extended field.
            let speed = |offset: usize, extended: usize| -> Option<u32> {
                match s.word(offset)? {
                    0 => None,
                    0xFFFF => s.dword(extended).map(|v| v & 0x7FFF_FFFF).filter(|v| *v > 0),
                    v => Some(u32::from(v)),
                }
            };
            Some(MemoryDevice {
                locator: s.string(0x10),
                bank: s.string(0x11),
                size_mb,
                memory_type: s.byte(0x12).unwrap_or(0x02),
                speed_mts: speed(0x15, 0x54),
                configured_speed_mts: speed(0x20, 0x58),
                manufacturer: s.string(0x17).filter(|m| !is_placeholder(m)),
                part_number: s.string(0x1A).filter(|p| !is_placeholder(p)),
                configured_voltage_mv: s.word(0x26).filter(|v| *v > 0).map(u32::from),
            })
        })
        .collect()
}

/// Firmware fills unused string fields with these rather than leaving them
/// out.
fn is_placeholder(s: &str) -> bool {
    let t = s.trim().trim_start_matches("0x");
    t.is_empty()
        || t.chars().all(|c| c == '0' || c == 'F' || c == 'f')
        || ["unknown", "not specified", "to be filled by o.e.m."].contains(&s.trim().to_ascii_lowercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a structure: header + formatted bytes + string set.
    fn structure(kind: u8, handle: u16, formatted: &[u8], strings: &[&str]) -> Vec<u8> {
        let len = 4 + formatted.len();
        let mut out = vec![kind, len as u8];
        out.extend_from_slice(&handle.to_le_bytes());
        out.extend_from_slice(formatted);
        for s in strings {
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        if strings.is_empty() {
            out.push(0);
        }
        out.push(0);
        out
    }

    /// A type 17 body (offsets 0x04..0x28), as an SMBIOS 2.8 Intel Mac
    /// publishes it.
    fn memory_device(size: u16, speed: u16, configured: u16, ty: u8) -> Vec<u8> {
        let mut f = vec![0u8; 0x28 - 4];
        let mut put = |off: usize, bytes: &[u8]| f[off - 4..off - 4 + bytes.len()].copy_from_slice(bytes);
        put(0x0C, &size.to_le_bytes());
        put(0x10, &[1]); // device locator -> string 1
        put(0x11, &[2]); // bank locator -> string 2
        put(0x12, &[ty]);
        put(0x15, &speed.to_le_bytes());
        put(0x17, &[3]); // manufacturer
        put(0x1A, &[4]); // part number
        put(0x20, &configured.to_le_bytes());
        put(0x26, &1200u16.to_le_bytes());
        f
    }

    fn mac_table() -> Vec<u8> {
        let mut t = Vec::new();
        // Type 0: vendor=1, version=2, date=3 at 0x04/0x05/0x08.
        let mut bios = vec![0u8; 0x12 - 4];
        bios[0] = 1;
        bios[1] = 2;
        bios[4] = 3;
        t.extend(structure(0, 0, &bios, &["Apple Inc.", "1916.40.8.0.0 (iBridge: 20.16.420.0.0,0)", "05/05/2023"]));
        let mut sys = vec![0u8; 0x1B - 4];
        sys[0] = 1;
        sys[1] = 2;
        t.extend(structure(1, 1, &sys, &["Apple Inc.", "MacBookPro16,1"]));
        t.extend(structure(
            17,
            0x11,
            &memory_device(16384, 2667, 2667, 0x1A),
            &["ChannelA-DIMM0", "BANK 0", "Hynix", "HMA82GS6CJR8N-VK"],
        ));
        // An empty slot: size 0 must not appear as a module.
        // (Its manufacturer/part indices point past the string set, as
        // firmware leaves them for an empty slot.)
        t.extend(structure(17, 0x12, &memory_device(0, 0, 0, 0x02), &["ChannelB-DIMM0", "BANK 2"]));
        // A KB-granular size (bit 15) and placeholder strings.
        t.extend(structure(
            17,
            0x13,
            &memory_device(0x8000 | 8192, 2133, 0, 0x1D),
            &["Onboard", "BANK 1", "0x0000", "Unknown"],
        ));
        t.extend(structure(127, 0xFFFF, &[], &[]));
        t
    }

    #[test]
    fn walks_every_structure_to_the_end_marker() {
        let t = mac_table();
        let kinds: Vec<u8> = parse(&t).iter().map(|s| s.kind).collect();
        assert_eq!(kinds, vec![0, 1, 17, 17, 17, 127]);
    }

    #[test]
    fn firmware_and_system_identity() {
        let t = mac_table();
        let s = parse(&t);
        let b = bios(&s).unwrap();
        assert_eq!(b.vendor.as_deref(), Some("Apple Inc."));
        assert_eq!(b.version.as_deref(), Some("1916.40.8.0.0 (iBridge: 20.16.420.0.0,0)"));
        assert_eq!(b.date.as_deref(), Some("05/05/2023"));
        assert_eq!(system(&s), Some((Some("Apple Inc.".into()), Some("MacBookPro16,1".into()))));
    }

    #[test]
    fn memory_devices_skip_empty_slots_and_decode_sizes() {
        let t = mac_table();
        let mods = memory_devices(&parse(&t));
        assert_eq!(mods.len(), 2, "{mods:?}");
        assert_eq!(mods[0].size_mb, 16384);
        assert_eq!(mods[0].memory_type, 0x1A);
        assert_eq!(mods[0].speed_mts, Some(2667));
        assert_eq!(mods[0].manufacturer.as_deref(), Some("Hynix"));
        assert_eq!(mods[0].part_number.as_deref(), Some("HMA82GS6CJR8N-VK"));
        assert_eq!(mods[0].locator.as_deref(), Some("ChannelA-DIMM0"));
        assert_eq!(mods[0].configured_voltage_mv, Some(1200));
        // 8192 KB = 8 MB, and the placeholder strings are dropped.
        assert_eq!(mods[1].size_mb, 8);
        assert_eq!(mods[1].configured_speed_mts, None);
        assert_eq!(mods[1].manufacturer, None);
        assert_eq!(mods[1].part_number, None);
    }

    #[test]
    fn extended_size_field_is_used_for_large_modules() {
        let mut body = memory_device(0x7FFF, 4800, 4800, 0x22);
        // Make room for the extended size at 0x1C.
        body[0x1C - 4..0x1C - 4 + 4].copy_from_slice(&65536u32.to_le_bytes());
        let t = [structure(17, 1, &body, &["DIMM", "BANK", "Samsung", "M321R8GA0BB0"]), structure(127, 2, &[], &[])]
            .concat();
        let mods = memory_devices(&parse(&t));
        assert_eq!(mods[0].size_mb, 65536);
    }

    #[test]
    fn truncated_tables_yield_what_they_can() {
        let t = mac_table();
        // Cut inside the third structure's formatted area.
        let cut = &t[..t.len() / 2];
        let s = parse(cut);
        assert!(!s.is_empty());
        assert!(s.iter().all(|s| s.data.len() >= 4));
        // Garbage must not panic.
        assert!(parse(&[0xFF, 0x02]).is_empty());
        assert!(parse(&[]).is_empty());
    }

    #[test]
    fn out_of_range_string_index_is_none() {
        let t = structure(1, 0, &[9, 0, 0, 0], &["only one"]);
        let s = parse(&t);
        assert_eq!(s[0].string(0x04), None);
        assert_eq!(s[0].string(0x05), None, "index 0 means no string");
    }
}
