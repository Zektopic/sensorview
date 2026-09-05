//! Static system information for the Main window tree and the System Summary.
//!
//! Queried once at startup on a background thread (WMI/COM on Windows; minimal
//! fallbacks elsewhere). Anything a source can't provide stays `None` and the
//! UI renders "—" — honest placeholders until the native engine (SMBus SPD,
//! CPUID, NVML/ADL) fills them in.

use std::sync::{Arc, RwLock};

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct CpuInfo {
    pub name: String,
    pub cores: Option<u32>,
    pub threads: Option<u32>,
    pub base_clock_mhz: Option<u32>,
    pub max_clock_mhz: Option<u32>,
    pub l2_kb: Option<u32>,
    pub l3_kb: Option<u32>,
    pub socket: Option<String>,
    /// CPUID(1).EAX signature, HWiNFO-style hex (e.g. "00A60F12").
    pub cpuid: String,
    /// Best-effort microarchitecture codename (e.g. "Raphael (Zen 4)").
    pub codename: String,
    pub vendor: String,
    /// ISA feature names detected at runtime (for the Summary features grid).
    pub features: Vec<(&'static str, bool)>,
}

/// Raw CPUID(1).EAX signature + vendor + codename, computed on x86_64.
fn cpuid_info() -> (String, String, String) {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::__cpuid;
        // __cpuid is safe on x86_64 (CPUID is always available).
        let vendor_leaf = __cpuid(0);
        let mut vbytes = Vec::new();
        vbytes.extend_from_slice(&vendor_leaf.ebx.to_le_bytes());
        vbytes.extend_from_slice(&vendor_leaf.edx.to_le_bytes());
        vbytes.extend_from_slice(&vendor_leaf.ecx.to_le_bytes());
        let vendor = String::from_utf8_lossy(&vbytes).to_string();

        let leaf1 = __cpuid(1);
        let eax = leaf1.eax;
        let base_family = (eax >> 8) & 0xf;
        let ext_family = (eax >> 20) & 0xff;
        let family = if base_family == 0xf { base_family + ext_family } else { base_family };
        let base_model = (eax >> 4) & 0xf;
        let ext_model = (eax >> 16) & 0xf;
        let model = (ext_model << 4) | base_model;

        let codename = codename_for(&vendor, family, model);
        (format!("{eax:08X}"), vendor, codename)
    }
    // Apple Silicon has no CPUID. The nearest equivalents are the board id
    // (`hw.model`, e.g. "Mac17,3") and the SoC name from the brand string, so
    // report those rather than leaving the Summary window blank.
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        let vendor = if sysctl_string("machdep.cpu.brand_string")
            .is_some_and(|b| b.starts_with("Apple"))
        {
            "Apple".to_string()
        } else {
            String::new()
        };
        (String::new(), vendor, sysctl_string("hw.model").unwrap_or_default())
    }
    #[cfg(not(any(target_arch = "x86_64", all(target_arch = "aarch64", target_os = "macos"))))]
    {
        (String::new(), String::new(), String::new())
    }
}

/// Microarchitecture codename for an x86 CPU, from its CPUID family and model.
///
/// Sourcing matters here, because a *wrong* codename is worse than none: it is
/// rendered in the System Summary directly beside the CPUID signature a user
/// can check it against. Three sources, none of them guesswork:
///
/// * Intel up to Tiger Lake / Comet Lake — this repository's own
///   `Hardware/CPU/IntelCPU.cs`, the original OpenHardwareMonitor detection
///   these sources are a port of.
/// * Intel from Rocket Lake onwards, plus the Atom and Xeon lines — the Linux
///   kernel's `arch/x86/include/asm/intel-family.h`.
/// * AMD — libcpuid's `recog_amd.c`.
///
/// Where a source names a specific part, that name is used. Where none does,
/// the arm falls back to a *generation* label that is true for every member of
/// the family rather than inventing a codename, and an unrecognised family
/// returns `""`, which callers already render as blank.
#[allow(dead_code)] // Not reachable on non-x86_64 targets; see `cpuid_info`.
fn codename_for(vendor: &str, family: u32, model: u32) -> String {
    if vendor.contains("AuthenticAMD") {
        amd_codename(family, model).to_string()
    } else if vendor.contains("GenuineIntel") {
        intel_codename(family, model).to_string()
    } else {
        // Neither vendor — a VM's synthetic CPUID, a Hygon part, or an
        // emulated x86. Nothing truthful to say.
        String::new()
    }
}

/// AMD, keyed on family then model's high nibble — the grouping AMD itself
/// uses to separate parts within a family.
///
/// Every family arm ends in a catch-all naming only the *generation*, which is
/// safe because AMD does not mix generations within these families: 17h is
/// Zen through Zen 2, 19h is Zen 3 and Zen 4, 1Ah is Zen 5 throughout. A
/// part released after this table was written therefore still reports its
/// generation correctly instead of falling through to blank.
fn amd_codename(family: u32, model: u32) -> &'static str {
    match (family, model) {
        // --- Zen 5, family 1Ah ---------------------------------------------
        // libcpuid: family 26 model 2 (Turin), 36/0x24 (Strix Point),
        // 68/0x44 (Granite Ridge).
        (0x1a, 0x00..=0x0f) => "Turin (Zen 5)",
        (0x1a, 0x20..=0x2f) => "Strix Point (Zen 5)",
        (0x1a, 0x40..=0x4f) => "Granite Ridge (Zen 5)",
        (0x1a, _) => "Zen 5",

        // --- Zen 3, Zen 3+ and Zen 4 all share family 19h ------------------
        // libcpuid: family 25 models 1 (Milan), 33/0x21 (Vermeer),
        // 68/0x44 (Rembrandt), 80/0x50 (Cezanne), 116/0x74 (Phoenix).
        (0x19, 0x00..=0x0f) => "Milan (Zen 3)",
        (0x19, 0x20..=0x2f) => "Vermeer (Zen 3)",
        (0x19, 0x40..=0x4f) => "Rembrandt (Zen 3+)",
        (0x19, 0x50..=0x5f) => "Cezanne (Zen 3)",
        (0x19, 0x60..=0x6f) => "Raphael (Zen 4)",
        (0x19, 0x70..=0x7f) => "Phoenix (Zen 4)",
        (0x19, _) => "Zen 3/Zen 4",

        // --- Zen, Zen+ and Zen 2 share family 17h --------------------------
        // libcpuid: family 23 model 1 (Naples / Whitehaven / Summit Ridge).
        // The rest of 17h is deliberately generic — this used to claim
        // "Matisse/Renoir (Zen 2)" for the whole family, which mislabelled
        // every first-generation Ryzen as a Zen 2 part.
        (0x17, 0x00..=0x0f) => "Summit Ridge/Naples (Zen)",
        (0x17, _) => "Zen/Zen+/Zen 2",

        _ => "",
    }
}

/// Intel. Family 6 carried everything from the Pentium Pro to Panther Lake, so
/// the model is what identifies a part; Nova Lake (12h) and Diamond Rapids
/// (13h) are the first to move off it, which is why this matches on the family
/// rather than assuming 6.
///
/// `intel-family.h` writes those two families in decimal — `IFM(18, ...)` and
/// `IFM(19, ...)` — which are 12h and 13h. Families here are hex throughout,
/// as the arms below are, so that they read consistently against the AMD
/// table, where 19h means something else entirely (Zen 3/Zen 4).
fn intel_codename(family: u32, model: u32) -> &'static str {
    match (family, model) {
        // --- Families 12h and 13h: the move off family 6 -------------------
        (0x12, 0x01 | 0x03) => "Nova Lake (Coyote Cove/Arctic Wolf)",
        (0x12, _) => "Intel (family 12h)",
        (0x13, 0x01) => "Diamond Rapids (Panther Cove)",
        (0x13, _) => "Intel (family 13h)",

        // --- Family 6, newest first ---------------------------------------
        (0x6, 0xE5 | 0xCC) => "Panther Lake (Cougar Cove/Darkmont)",
        (0x6, 0xDD) => "Clearwater Forest (Darkmont)",
        (0x6, 0xD7) => "Bartlett Lake (Raptor Cove)",
        (0x6, 0xD5) => "Wildcat Lake",
        (0x6, 0xC6 | 0xC5 | 0xB5) => "Arrow Lake (Lion Cove/Skymont)",
        (0x6, 0xBD) => "Lunar Lake (Lion Cove/Skymont)",
        (0x6, 0xCF) => "Emerald Rapids (Raptor Cove)",
        (0x6, 0xAD | 0xAE) => "Granite Rapids (Redwood Cove)",
        (0x6, 0xAF) => "Sierra Forest (Crestmont)",
        (0x6, 0xB6) => "Grand Ridge (Crestmont)",
        (0x6, 0xAC | 0xAA) => "Meteor Lake (Redwood Cove/Crestmont)",
        (0x6, 0xB7 | 0xBA | 0xBF) => "Raptor Lake (Raptor Cove/Gracemont)",
        (0x6, 0xBE) => "Alder Lake-N (Gracemont)",
        (0x6, 0x97 | 0x9A) => "Alder Lake (Golden Cove/Gracemont)",
        (0x6, 0x8F) => "Sapphire Rapids (Golden Cove)",
        (0x6, 0xA7) => "Rocket Lake (Cypress Cove)",
        (0x6, 0x8A) => "Lakefield (Sunny Cove/Tremont)",

        // --- Family 6, from Hardware/CPU/IntelCPU.cs -----------------------
        (0x6, 0x8C | 0x8D) => "Tiger Lake",
        (0x6, 0xA5 | 0xA6) => "Comet Lake",
        (0x6, 0x7D | 0x7E | 0x6A | 0x6C | 0x9D) => "Ice Lake",
        (0x6, 0x66) => "Cannon Lake",
        (0x6, 0x8E | 0x9E) => "Kaby Lake",
        (0x6, 0x4E | 0x5E | 0x55) => "Skylake",
        (0x6, 0x3D | 0x47 | 0x4F | 0x56) => "Broadwell",
        (0x6, 0x3C | 0x3F | 0x45 | 0x46) => "Haswell",
        (0x6, 0x3A | 0x3E) => "Ivy Bridge",
        (0x6, 0x2A | 0x2D) => "Sandy Bridge",
        (0x6, 0x25 | 0x2C | 0x2F) => "Westmere",
        (0x6, 0x1A | 0x1E | 0x1F | 0x2E) => "Nehalem",
        (0x6, 0x0F | 0x16 | 0x17 | 0x1D) => "Core 2",

        // Atom. Previously all of these reported "Intel Core", which is not a
        // vaguer answer but a wrong one.
        (0x6, 0x9C) => "Jasper Lake (Tremont)",
        (0x6, 0x96) => "Elkhart Lake (Tremont)",
        (0x6, 0x86) => "Jacobsville (Tremont)",
        (0x6, 0x7A) => "Gemini Lake (Goldmont Plus)",
        (0x6, 0x5C) => "Apollo Lake (Goldmont)",
        (0x6, 0x5F) => "Denverton (Goldmont)",
        (0x6, 0x4C) => "Cherry Trail (Airmont)",
        (0x6, 0x75) => "Lightning Mountain (Airmont)",
        (0x6, 0x37 | 0x4A | 0x4D | 0x5A) => "Silvermont",
        (0x6, 0x35 | 0x36) => "Saltwell",
        (0x6, 0x1C | 0x26 | 0x27) => "Bonnell",
        (0x6, 0x57) => "Knights Landing",
        (0x6, 0x85) => "Knights Mill",

        // Anything else on family 6 is a part newer than this table. "Intel
        // Core" would be a *guess*, not a cautious answer: family 6 also
        // carries every Atom, and calling an Atom a Core is the same error
        // this table exists to remove. Name only the family, as the 12h and
        // 13h arms above do.
        (0x6, _) => "Intel (family 6h)",

        // Family 15h — NetBurst (Hardware/CPU/IntelCPU.cs).
        (0xf, _) => "NetBurst",

        _ => "",
    }
}

/// SMBIOS *Memory Device* (structure type 17) memory-type code → display name.
///
/// Codes are the DMTF SMBIOS specification's, cross-checked against
/// dmidecode's `dmi_memory_device_type` table, which runs from `0x01` to
/// `0x24`. Every assigned code in that range is decoded; `0x15`–`0x17` are
/// Reserved rather than memory types, so they fall to the unknown path along
/// with anything DMTF has yet to assign.
///
/// This used to decode exactly three values — DDR3, DDR4, DDR5 — and answer
/// `"DRAM"` for everything else. The Summary appends " SDRAM" to whatever it
/// gets, so the fallback rendered as the literal string "DRAM SDRAM". Every
/// soldered-memory machine reports an LPDDR code, so that was most current
/// laptop hardware.
///
/// A code the table does not know is reported as `Unknown (type 0x??)` rather
/// than guessed at: the raw code is what lets someone look it up, and DMTF
/// assigns new ones as memory generations ship.
#[allow(dead_code)] // Only reachable from the Windows WMI path.
fn smbios_memory_type(code: u32) -> String {
    let name = match code {
        0x01 | 0x02 => "Unknown", // "Other" and "Unknown" are both non-answers.
        0x03 => "DRAM",
        0x04 => "EDRAM",
        0x05 => "VRAM",
        0x06 => "SRAM",
        0x07 => "RAM",
        0x08 => "ROM",
        0x09 => "Flash",
        0x0A => "EEPROM",
        0x0B => "FEPROM",
        0x0C => "EPROM",
        0x0D => "CDRAM",
        0x0E => "3DRAM",
        0x0F => "SDRAM",
        0x10 => "SGRAM",
        0x11 => "RDRAM",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x19 => "FBD2",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x1F => "Logical non-volatile device",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => return format!("Unknown (type {code:#04X})"),
    };
    name.to_string()
}

/// How the Summary labels a module: "DDR5" becomes "DDR5 SDRAM", but "HBM3"
/// and "Unknown" are left alone.
///
/// The suffix used to be appended unconditionally, which is where "DRAM SDRAM"
/// came from. Only the DDR and LPDDR families are synchronous DRAM in the
/// sense that suffix means.
#[allow(dead_code)] // Only the GUI renders this; headless builds don't link it.
pub fn memory_type_label(memory_type: &str) -> String {
    if memory_type.starts_with("DDR") || memory_type.starts_with("LPDDR") {
        format!("{memory_type} SDRAM")
    } else {
        memory_type.to_string()
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct BoardInfo {
    pub product: String,
    pub manufacturer: String,
    pub bios_version: String,
    pub bios_date: String,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct MemoryModule {
    pub bank: String,
    pub manufacturer: String,
    pub part_number: String,
    pub capacity_gb: f64,
    pub speed_mts: Option<u32>,
    pub configured_speed_mts: Option<u32>,
    pub voltage_mv: Option<u32>,
    pub memory_type: String,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct GpuInfo {
    pub name: String,
    /// WMI AdapterRAM (u32, capped at 4 GB) — kept for the native engine to
    /// replace with NVML/ADL truth; not displayed while unreliable.
    #[allow(dead_code)]
    pub vram_gb: Option<f64>,
    pub driver_version: String,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct DriveInfo {
    pub model: String,
    pub interface: String,
    pub size_gb: Option<f64>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct OsInfo {
    pub caption: String,
    pub build: String,
    pub arch: String,
    pub uefi_boot: Option<bool>,
    pub secure_boot: Option<bool>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SystemInfo {
    pub computer_name: String,
    pub user_name: String,
    pub cpu: CpuInfo,
    pub board: BoardInfo,
    pub memory_modules: Vec<MemoryModule>,
    pub total_memory_gb: Option<f64>,
    pub gpus: Vec<GpuInfo>,
    pub drives: Vec<DriveInfo>,
    pub os: OsInfo,
}

/// Shared handle: `None` until the background query completes.
pub type SystemInfoHandle = Arc<RwLock<Option<SystemInfo>>>;

/// Whether this process is running elevated (`Some(true/false)` on Windows,
/// `None` elsewhere). Reliable and independent of the sidecar — the sidecar is
/// our child, so it inherits our elevation.
// Surfaced as the GUI's "Running as Administrator" badge.
#[allow(dead_code)]
pub fn is_elevated() -> Option<bool> {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct TokenElevation {
            token_is_elevated: u32,
        }
        const TOKEN_QUERY: u32 = 0x0008;
        const TOKEN_ELEVATION_CLASS: i32 = 20; // TokenElevation

        #[link(name = "advapi32")]
        extern "system" {
            fn OpenProcessToken(process: isize, desired: u32, handle: *mut isize) -> i32;
            fn GetTokenInformation(
                token: isize,
                class: i32,
                info: *mut core::ffi::c_void,
                len: u32,
                ret_len: *mut u32,
            ) -> i32;
        }
        extern "system" {
            fn GetCurrentProcess() -> isize;
            fn CloseHandle(h: isize) -> i32;
        }

        unsafe {
            let mut token: isize = 0;
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut elevation = TokenElevation { token_is_elevated: 0 };
            let mut ret_len = 0u32;
            let ok = GetTokenInformation(
                token,
                TOKEN_ELEVATION_CLASS,
                &mut elevation as *mut _ as *mut core::ffi::c_void,
                core::mem::size_of::<TokenElevation>() as u32,
                &mut ret_len,
            );
            CloseHandle(token);
            if ok == 0 {
                None
            } else {
                Some(elevation.token_is_elevated != 0)
            }
        }
    }
    // Reported for the status badge only. Nothing on macOS *needs* root: the
    // IOKit backend reads every sensor unprivileged, so no feature is gated on
    // this (see ui/settings_dialog.rs).
    #[cfg(target_os = "macos")]
    {
        Some(unsafe { libc::geteuid() } == 0)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

// ---- sysctl helpers (macOS) ---------------------------------------------

/// Read a string-valued sysctl by name. `None` if the key doesn't exist —
/// keys come and go between macOS releases, so every caller must tolerate it.
#[cfg(target_os = "macos")]
pub(crate) fn sysctl_string(name: &str) -> Option<String> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut len = 0usize;
    // First call with a null buffer asks for the required size.
    if unsafe {
        libc::sysctlbyname(cname.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0)
    } != 0
        || len == 0
    {
        return None;
    }
    let mut buf = vec![0u8; len];
    if unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buf.truncate(len);
    // sysctl strings are NUL-terminated; drop the terminator and anything after.
    if let Some(nul) = buf.iter().position(|&b| b == 0) {
        buf.truncate(nul);
    }
    let s = String::from_utf8_lossy(&buf).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Read an integer-valued sysctl. Handles both the 4-byte and 8-byte widths the
/// kernel uses (`hw.ncpu` is 32-bit, `hw.memsize` is 64-bit).
#[cfg(target_os = "macos")]
pub(crate) fn sysctl_u64(name: &str) -> Option<u64> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut value = 0u64;
    let mut len = std::mem::size_of::<u64>();
    if unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            (&mut value as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    match len {
        8 => Some(value),
        // The kernel wrote only the low 4 bytes; the upper half is our zeroed
        // initialiser, so mask rather than trusting the whole u64.
        4 => Some(value & 0xffff_ffff),
        _ => None,
    }
}

/// Synchronous `query()` for tests and diagnostics — `spawn_query` returns a
/// handle that only fills in later, which is awkward to assert against.
///
/// Only the macOS system-profile test consumes it today; `allow(dead_code)`
/// keeps `clippy -D warnings` green on platforms that have no caller yet.
#[cfg(test)]
#[allow(dead_code)]
pub fn query_for_test() -> SystemInfo {
    query()
}

/// Kick off the (slow) WMI enumeration without blocking the UI.
pub fn spawn_query() -> SystemInfoHandle {
    let handle: SystemInfoHandle = Arc::new(RwLock::new(None));
    let sink = handle.clone();
    std::thread::spawn(move || {
        let info = query();
        if let Ok(mut slot) = sink.write() {
            *slot = Some(info);
        }
    });
    handle
}

fn cpu_features() -> Vec<(&'static str, bool)> {
    #[cfg(target_arch = "x86_64")]
    {
        vec![
            ("MMX", is_x86_feature_detected!("mmx")),
            ("SSE", is_x86_feature_detected!("sse")),
            ("SSE2", is_x86_feature_detected!("sse2")),
            ("SSE3", is_x86_feature_detected!("sse3")),
            ("SSSE3", is_x86_feature_detected!("ssse3")),
            ("SSE4.1", is_x86_feature_detected!("sse4.1")),
            ("SSE4.2", is_x86_feature_detected!("sse4.2")),
            ("SSE4A", is_x86_feature_detected!("sse4a")),
            ("AVX", is_x86_feature_detected!("avx")),
            ("AVX2", is_x86_feature_detected!("avx2")),
            ("AVX-512F", is_x86_feature_detected!("avx512f")),
            ("FMA", is_x86_feature_detected!("fma")),
            ("BMI1", is_x86_feature_detected!("bmi1")),
            ("BMI2", is_x86_feature_detected!("bmi2")),
            ("AES-NI", is_x86_feature_detected!("aes")),
            ("SHA", is_x86_feature_detected!("sha")),
            ("RDRAND", is_x86_feature_detected!("rdrand")),
            ("RDSEED", is_x86_feature_detected!("rdseed")),
            ("POPCNT", is_x86_feature_detected!("popcnt")),
            ("F16C", is_x86_feature_detected!("f16c")),
        ]
    }
    // Apple Silicon: the kernel publishes ~80 `hw.optional.arm.FEAT_*` flags.
    // Curated rather than enumerated so the grid stays readable and the labels
    // stay `&'static str` — the full list is mostly MTE/SME sub-variants.
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        const FEATURES: &[(&str, &str)] = &[
            ("NEON", "hw.optional.arm.AdvSIMD"),
            ("FP16", "hw.optional.arm.FEAT_FP16"),
            ("BF16", "hw.optional.arm.FEAT_BF16"),
            ("I8MM", "hw.optional.arm.FEAT_I8MM"),
            ("DotProd", "hw.optional.arm.FEAT_DotProd"),
            ("FHM", "hw.optional.arm.FEAT_FHM"),
            ("CRC32", "hw.optional.arm.FEAT_CRC32"),
            ("AES", "hw.optional.arm.FEAT_AES"),
            ("PMULL", "hw.optional.arm.FEAT_PMULL"),
            ("SHA1", "hw.optional.arm.FEAT_SHA1"),
            ("SHA256", "hw.optional.arm.FEAT_SHA256"),
            ("SHA3", "hw.optional.arm.FEAT_SHA3"),
            ("SHA512", "hw.optional.arm.FEAT_SHA512"),
            ("LSE", "hw.optional.arm.FEAT_LSE"),
            ("LSE2", "hw.optional.arm.FEAT_LSE2"),
            ("RDM", "hw.optional.arm.FEAT_RDM"),
            ("JSCVT", "hw.optional.arm.FEAT_JSCVT"),
            ("FCMA", "hw.optional.arm.FEAT_FCMA"),
            ("LRCPC", "hw.optional.arm.FEAT_LRCPC"),
            ("PAuth", "hw.optional.arm.FEAT_PAuth"),
            ("BTI", "hw.optional.arm.FEAT_BTI"),
            ("MTE", "hw.optional.arm.FEAT_MTE"),
            ("DIT", "hw.optional.arm.FEAT_DIT"),
            ("ECV", "hw.optional.arm.FEAT_ECV"),
            ("SME", "hw.optional.arm.FEAT_SME"),
            ("SME2", "hw.optional.arm.FEAT_SME2"),
            ("SSBS", "hw.optional.arm.FEAT_SSBS"),
            ("SPECRES", "hw.optional.arm.FEAT_SPECRES"),
        ];
        FEATURES
            .iter()
            .map(|(label, key)| (*label, sysctl_u64(key).unwrap_or(0) != 0))
            .collect()
    }
    #[cfg(not(any(target_arch = "x86_64", all(target_arch = "aarch64", target_os = "macos"))))]
    {
        Vec::new()
    }
}

#[cfg(windows)]
fn query() -> SystemInfo {
    use std::collections::HashMap;
    use wmi::{Variant, WMIConnection};

    let mut info = SystemInfo {
        computer_name: std::env::var("COMPUTERNAME").unwrap_or_default(),
        user_name: std::env::var("USERNAME").unwrap_or_default(),
        ..Default::default()
    };
    info.cpu.features = cpu_features();
    let (cpuid, vendor, codename) = cpuid_info();
    info.cpu.cpuid = cpuid;
    info.cpu.vendor = vendor;
    info.cpu.codename = codename;

    let Ok(wmi) = WMIConnection::new() else { return info };

    type Row = HashMap<String, Variant>;

    let s = |v: Option<&Variant>| -> String {
        match v {
            Some(Variant::String(x)) => x.trim().to_string(),
            _ => String::new(),
        }
    };
    let u = |v: Option<&Variant>| -> Option<u32> {
        match v {
            Some(Variant::UI4(x)) => Some(*x),
            Some(Variant::I4(x)) => u32::try_from(*x).ok(),
            Some(Variant::UI2(x)) => Some(*x as u32),
            Some(Variant::String(x)) => x.parse().ok(),
            _ => None,
        }
    };
    let u64v = |v: Option<&Variant>| -> Option<u64> {
        match v {
            Some(Variant::UI8(x)) => Some(*x),
            Some(Variant::I8(x)) => u64::try_from(*x).ok(),
            Some(Variant::UI4(x)) => Some(*x as u64),
            Some(Variant::String(x)) => x.parse().ok(),
            _ => None,
        }
    };

    if let Ok(rows) = wmi.raw_query::<Row>(
        "SELECT Name, NumberOfCores, NumberOfLogicalProcessors, MaxClockSpeed, L2CacheSize, L3CacheSize, SocketDesignation FROM Win32_Processor",
    ) {
        if let Some(r) = rows.first() {
            info.cpu.name = s(r.get("Name"));
            info.cpu.cores = u(r.get("NumberOfCores"));
            info.cpu.threads = u(r.get("NumberOfLogicalProcessors"));
            info.cpu.max_clock_mhz = u(r.get("MaxClockSpeed"));
            info.cpu.base_clock_mhz = u(r.get("MaxClockSpeed"));
            info.cpu.l2_kb = u(r.get("L2CacheSize"));
            info.cpu.l3_kb = u(r.get("L3CacheSize"));
            info.cpu.socket = Some(s(r.get("SocketDesignation"))).filter(|x| !x.is_empty());
        }
    }

    if let Ok(rows) = wmi.raw_query::<Row>("SELECT Product, Manufacturer FROM Win32_BaseBoard") {
        if let Some(r) = rows.first() {
            info.board.product = s(r.get("Product"));
            info.board.manufacturer = s(r.get("Manufacturer"));
        }
    }
    if let Ok(rows) = wmi.raw_query::<Row>("SELECT SMBIOSBIOSVersion, ReleaseDate FROM Win32_BIOS") {
        if let Some(r) = rows.first() {
            info.board.bios_version = s(r.get("SMBIOSBIOSVersion"));
            let date = s(r.get("ReleaseDate"));
            // WMI CIM_DATETIME: yyyymmddHHMMSS… → mm/dd/yyyy like HWiNFO shows.
            if date.len() >= 8 {
                info.board.bios_date = format!("{}/{}/{}", &date[4..6], &date[6..8], &date[0..4]);
            }
        }
    }

    if let Ok(rows) = wmi.raw_query::<Row>(
        "SELECT BankLabel, DeviceLocator, Manufacturer, PartNumber, Capacity, Speed, ConfiguredClockSpeed, ConfiguredVoltage, SMBIOSMemoryType FROM Win32_PhysicalMemory",
    ) {
        let mut total = 0.0;
        for r in &rows {
            let capacity_gb = u64v(r.get("Capacity")).map(|b| b as f64 / (1u64 << 30) as f64).unwrap_or(0.0);
            total += capacity_gb;
            // An absent field is left blank; a present one is decoded, so an
            // unrecognised code shows as `Unknown (type 0x??)` rather than
            // being flattened into a wrong name.
            let mem_type = u(r.get("SMBIOSMemoryType"))
                .map(smbios_memory_type)
                .unwrap_or_default();
            info.memory_modules.push(MemoryModule {
                bank: {
                    let bank = s(r.get("BankLabel"));
                    let loc = s(r.get("DeviceLocator"));
                    if bank.is_empty() { loc } else { format!("{bank}/{loc}") }
                },
                manufacturer: s(r.get("Manufacturer")),
                part_number: s(r.get("PartNumber")),
                capacity_gb,
                speed_mts: u(r.get("Speed")),
                configured_speed_mts: u(r.get("ConfiguredClockSpeed")),
                voltage_mv: u(r.get("ConfiguredVoltage")),
                memory_type: mem_type.to_string(),
            });
        }
        if total > 0.0 {
            info.total_memory_gb = Some(total);
        }
    }

    if let Ok(rows) = wmi.raw_query::<Row>("SELECT Name, AdapterRAM, DriverVersion FROM Win32_VideoController") {
        for r in &rows {
            info.gpus.push(GpuInfo {
                name: s(r.get("Name")),
                vram_gb: u64v(r.get("AdapterRAM")).map(|b| b as f64 / (1u64 << 30) as f64),
                driver_version: s(r.get("DriverVersion")),
            });
        }
    }

    if let Ok(rows) = wmi.raw_query::<Row>("SELECT Model, InterfaceType, Size FROM Win32_DiskDrive") {
        for r in &rows {
            info.drives.push(DriveInfo {
                model: s(r.get("Model")),
                interface: s(r.get("InterfaceType")),
                size_gb: u64v(r.get("Size")).map(|b| b as f64 / 1_000_000_000.0),
            });
        }
    }

    if let Ok(rows) = wmi.raw_query::<Row>("SELECT Caption, BuildNumber, OSArchitecture FROM Win32_OperatingSystem") {
        if let Some(r) = rows.first() {
            info.os.caption = s(r.get("Caption"));
            info.os.build = s(r.get("BuildNumber"));
            info.os.arch = s(r.get("OSArchitecture"));
        }
    }

    // Secure Boot / UEFI: registry flag (no clean WMI class for it).
    info.os.secure_boot = read_secure_boot();
    info.os.uefi_boot = info.os.secure_boot.map(|_| true);

    info
}

#[cfg(windows)]
fn read_secure_boot() -> Option<bool> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SYSTEM\CurrentControlSet\Control\SecureBoot\State",
            "/v",
            "UEFISecureBootEnabled",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    if text.contains("0x1") {
        Some(true)
    } else if text.contains("0x0") {
        Some(false)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn query() -> SystemInfo {
    let (cpuid, vendor, codename) = cpuid_info();

    // Apple Silicon is heterogeneous: perflevel0 is the fast cluster (named
    // "Performance" through M4, "Super" on M5 — read the name, don't hardcode
    // it) and perflevel1 the efficiency cluster. Sum both for the core count,
    // and report the layout in `socket` since there is no socket to speak of.
    let mut cluster_desc = Vec::new();
    let mut physical = 0u32;
    for level in 0..4 {
        let Some(count) = sysctl_u64(&format!("hw.perflevel{level}.physicalcpu")) else {
            break;
        };
        physical += count as u32;
        let name = sysctl_string(&format!("hw.perflevel{level}.name"))
            .unwrap_or_else(|| format!("level{level}"));
        cluster_desc.push(format!("{count} {name}"));
    }
    // Fall back to the flat count on any Mac that doesn't publish perflevels.
    let cores = if physical > 0 { Some(physical) } else { sysctl_u64("hw.physicalcpu").map(|v| v as u32) };

    let total_memory_gb = sysctl_u64("hw.memsize").map(|b| b as f64 / (1024.0 * 1024.0 * 1024.0));

    // The SoC is soldered, so there are no per-DIMM SPD entries to enumerate;
    // present the unified memory as a single honest module.
    let memory_modules = total_memory_gb
        .map(|gb| {
            vec![MemoryModule {
                bank: "Unified Memory".into(),
                manufacturer: "Apple".into(),
                capacity_gb: gb,
                memory_type: "LPDDR (on-package)".into(),
                ..Default::default()
            }]
        })
        .unwrap_or_default();

    // The integrated GPU shares the SoC; name it after the chip and its core
    // count rather than inventing a discrete-adapter identity.
    let chip = sysctl_string("machdep.cpu.brand_string").unwrap_or_default();
    let (gpu_cores, metal_driver) = crate::source::macos::sysprofile::gpu_identity();
    let gpus = if chip.is_empty() {
        Vec::new()
    } else {
        let name = match gpu_cores {
            Some(cores) => format!("{chip} GPU ({cores} cores)"),
            None => format!("{chip} GPU"),
        };
        vec![GpuInfo {
            name,
            // Unified memory — there is no separate VRAM pool to report, and
            // quoting total system RAM here would be misleading.
            vram_gb: None,
            driver_version: metal_driver.unwrap_or_default(),
        }]
    };

    // Internal SSD(s), so the Summary's Drives panel isn't blank.
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let drives = crate::source::macos::sysprofile::drives()
        .into_iter()
        .map(|(model, bytes)| DriveInfo {
            model,
            interface: "NVMe".into(),
            size_gb: bytes.map(|b| b as f64 / GB),
        })
        .collect();

    // Performance-cluster DVFS states, for the Base/Max Clock rows.
    let p_states =
        crate::source::macos::dvfs::frequencies_mhz(crate::source::macos::dvfs::Block::Pcpu);

    let os_version = sysctl_string("kern.osproductversion").unwrap_or_default();

    SystemInfo {
        computer_name: sysctl_string("kern.hostname").unwrap_or_default(),
        user_name: std::env::var("USER").unwrap_or_default(),
        cpu: CpuInfo {
            name: chip,
            cores,
            // Apple Silicon has no SMT: one thread per physical core.
            threads: sysctl_u64("hw.logicalcpu").map(|v| v as u32),
            // There is no fixed "base clock" on Apple Silicon; the closest
            // honest equivalents are the bottom and top of the performance
            // cluster's DVFS table.
            base_clock_mhz: p_states.first().map(|mhz| *mhz as u32),
            max_clock_mhz: p_states.last().map(|mhz| *mhz as u32),
            // hw.l2cachesize reports the *efficiency* cluster (6 MB here).
            // The headline figure is the performance cluster's L2
            // (hw.perflevel0.l2cachesize, 16 MB), so prefer that.
            l2_kb: sysctl_u64("hw.perflevel0.l2cachesize")
                .or_else(|| sysctl_u64("hw.l2cachesize"))
                .map(|b| (b / 1024) as u32),
            // Apple Silicon has no per-core L3; the system-level cache is not
            // published anywhere readable, so this stays honestly blank.
            l3_kb: None,
            socket: (!cluster_desc.is_empty()).then(|| cluster_desc.join(" + ")),
            features: cpu_features(),
            cpuid,
            vendor,
            codename,
        },
        board: BoardInfo {
            // Prefer the marketing name ("MacBook Air (13-inch, M5)") over the
            // bare board id ("Mac17,3"), which is already shown as the codename.
            product: crate::source::macos::sysprofile::product_name()
                .or_else(|| sysctl_string("hw.model"))
                .unwrap_or_default(),
            manufacturer: "Apple Inc.".into(),
            // Apple Silicon boots via iBoot, so the closest thing to a BIOS
            // version is the boot firmware revision.
            bios_version: crate::source::macos::sysprofile::firmware_version().unwrap_or_default(),
            // No firmware build date is published anywhere in IOKit; leave it
            // blank rather than guessing from the version string.
            bios_date: String::new(),
        },
        memory_modules,
        total_memory_gb,
        gpus,
        drives,
        os: OsInfo {
            caption: if os_version.is_empty() {
                "macOS".into()
            } else {
                format!("macOS {os_version}")
            },
            build: sysctl_string("kern.osversion").unwrap_or_default(),
            arch: std::env::consts::ARCH.to_string(),
            // Apple Silicon boots via iBoot, not UEFI, and Secure Boot state
            // lives in a different subsystem — leave both indeterminate rather
            // than asserting something false.
            uefi_boot: None,
            secure_boot: None,
        },
    }
}

#[cfg(target_os = "linux")]
fn query() -> SystemInfo {
    use std::fs;
    let (cpuid, vendor, codename) = cpuid_info();
    let mut info = SystemInfo {
        computer_name: std::env::var("HOSTNAME").unwrap_or_default(),
        user_name: std::env::var("USER").unwrap_or_default(),
        cpu: CpuInfo { features: cpu_features(), cpuid, vendor, codename, ..Default::default() },
        ..Default::default()
    };

    // Read CPU Model Name and Cores from /proc/cpuinfo
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") {
        let mut model_name = String::new();
        let mut logical_count = 0u32;
        for line in cpuinfo.lines() {
            if line.starts_with("model name") {
                if let Some(val) = line.split(':').nth(1) {
                    if model_name.is_empty() {
                        model_name = val.trim().to_string();
                    }
                }
            }
            if line.starts_with("processor") {
                logical_count += 1;
            }
        }
        if !model_name.is_empty() {
            info.cpu.name = model_name;
        }
        if logical_count > 0 {
            info.cpu.threads = Some(logical_count);
            info.cpu.cores = Some(logical_count); // best-effort fallback
        }
    }

    // Read Motherboard / DMI Info from /sys/class/dmi/id
    if let Ok(product) = fs::read_to_string("/sys/class/dmi/id/board_name") {
        info.board.product = product.trim().to_string();
    }
    if let Ok(vendor) = fs::read_to_string("/sys/class/dmi/id/board_vendor") {
        info.board.manufacturer = vendor.trim().to_string();
    }
    if let Ok(version) = fs::read_to_string("/sys/class/dmi/id/bios_version") {
        info.board.bios_version = version.trim().to_string();
    }
    if let Ok(date) = fs::read_to_string("/sys/class/dmi/id/bios_date") {
        info.board.bios_date = date.trim().to_string();
    }

    // Read RAM Total from /proc/meminfo
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if line.starts_with("MemTotal:") {
                if let Some(kb_str) = line.split_whitespace().nth(1) {
                    if let Ok(kb) = kb_str.parse::<f64>() {
                        info.total_memory_gb = Some(kb / (1024.0 * 1024.0));
                    }
                }
            }
        }
    }

    // Read OS info from /etc/os-release
    if let Ok(os_release) = fs::read_to_string("/etc/os-release") {
        for line in os_release.lines() {
            if line.starts_with("PRETTY_NAME=") {
                let name = line.trim_start_matches("PRETTY_NAME=").trim_matches('"');
                info.os.caption = name.to_string();
            }
        }
    }

    info
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn query() -> SystemInfo {
    let (cpuid, vendor, codename) = cpuid_info();
    SystemInfo {
        computer_name: std::env::var("HOSTNAME").unwrap_or_default(),
        user_name: std::env::var("USER").unwrap_or_default(),
        cpu: CpuInfo { features: cpu_features(), cpuid, vendor, codename, ..Default::default() },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Codes are the DMTF SMBIOS values, cross-checked against dmidecode's
    // `dmi_memory_device_type` table. These run on every platform, since the
    // decoder takes the raw code rather than reading WMI.

    #[test]
    fn the_three_codes_that_already_worked_still_decode_the_same_way() {
        assert_eq!(smbios_memory_type(24), "DDR3");
        assert_eq!(smbios_memory_type(26), "DDR4");
        assert_eq!(smbios_memory_type(34), "DDR5");
    }

    #[test]
    fn soldered_laptop_memory_is_named_rather_than_flattened_to_dram() {
        // The reason for the change: every one of these used to fall through
        // to "DRAM" and render as "DRAM SDRAM".
        assert_eq!(smbios_memory_type(0x1B), "LPDDR");
        assert_eq!(smbios_memory_type(0x1C), "LPDDR2");
        assert_eq!(smbios_memory_type(0x1D), "LPDDR3");
        assert_eq!(smbios_memory_type(0x1E), "LPDDR4");
        assert_eq!(smbios_memory_type(0x23), "LPDDR5");
    }

    #[test]
    fn stacked_memory_decodes_too() {
        assert_eq!(smbios_memory_type(0x20), "HBM");
        assert_eq!(smbios_memory_type(0x21), "HBM2");
        assert_eq!(smbios_memory_type(0x24), "HBM3");
    }

    #[test]
    fn every_assigned_code_from_1_to_0x24_decodes_to_a_name() {
        // The claim this table makes: no assigned code falls through. 0x15-0x17
        // are Reserved rather than memory types, so they are the exception.
        for code in 0x01..=0x24u32 {
            let decoded = smbios_memory_type(code);
            if (0x15..=0x17).contains(&code) {
                assert_eq!(decoded, format!("Unknown (type {code:#04X})"));
            } else {
                assert!(
                    !decoded.starts_with("Unknown (type"),
                    "assigned code {code:#04X} fell through to the unknown path"
                );
            }
        }
    }

    #[test]
    fn an_unassigned_code_reports_the_raw_value_instead_of_guessing() {
        // 0x25 is past the last code DMTF has assigned. Showing the number is
        // what lets someone look it up.
        assert_eq!(smbios_memory_type(0x25), "Unknown (type 0x25)");
        assert_eq!(smbios_memory_type(0xFF), "Unknown (type 0xFF)");
    }

    #[test]
    fn smbios_other_and_unknown_are_both_reported_as_unknown() {
        assert_eq!(smbios_memory_type(0x01), "Unknown");
        assert_eq!(smbios_memory_type(0x02), "Unknown");
    }

    #[test]
    fn the_sdram_suffix_is_only_added_where_it_means_something() {
        assert_eq!(memory_type_label("DDR5"), "DDR5 SDRAM");
        assert_eq!(memory_type_label("LPDDR5"), "LPDDR5 SDRAM");
        // These are the ones that used to produce nonsense.
        assert_eq!(memory_type_label("HBM3"), "HBM3");
        assert_eq!(memory_type_label("Unknown"), "Unknown");
        assert_eq!(memory_type_label("Unknown (type 0x25)"), "Unknown (type 0x25)");
        assert_eq!(memory_type_label(""), "");
    }

    // These run on every CI leg, including aarch64, because `codename_for`
    // takes the family and model as arguments rather than executing CPUID.

    #[test]
    fn intel_hybrid_parts_are_named_rather_than_all_reporting_intel_core() {
        // The whole point of the change: every one of these used to be
        // indistinguishable, because family 6 was matched without the model.
        assert_eq!(codename_for("GenuineIntel", 0x6, 0x97), "Alder Lake (Golden Cove/Gracemont)");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xBF), "Raptor Lake (Raptor Cove/Gracemont)");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xAA), "Meteor Lake (Redwood Cove/Crestmont)");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xC6), "Arrow Lake (Lion Cove/Skymont)");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xBD), "Lunar Lake (Lion Cove/Skymont)");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xCC), "Panther Lake (Cougar Cove/Darkmont)");
    }

    #[test]
    fn intel_parts_off_family_six_are_recognised() {
        // Nova Lake and Diamond Rapids are the first Intel parts to leave
        // family 6; matching on the family alone reported "" for them.
        assert_eq!(codename_for("GenuineIntel", 0x12, 0x01), "Nova Lake (Coyote Cove/Arctic Wolf)");
        assert_eq!(codename_for("GenuineIntel", 0x13, 0x01), "Diamond Rapids (Panther Cove)");
    }

    #[test]
    fn an_atom_is_not_called_a_core() {
        assert_eq!(codename_for("GenuineIntel", 0x6, 0x9C), "Jasper Lake (Tremont)");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xBE), "Alder Lake-N (Gracemont)");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xAF), "Sierra Forest (Crestmont)");
    }

    #[test]
    fn historic_intel_parts_still_match_the_c_sharp_table_they_came_from() {
        // Hardware/CPU/IntelCPU.cs, which these sources are a port of.
        assert_eq!(codename_for("GenuineIntel", 0x6, 0x8D), "Tiger Lake");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xA5), "Comet Lake");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0x3C), "Haswell");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0x2A), "Sandy Bridge");
        assert_eq!(codename_for("GenuineIntel", 0xf, 0x03), "NetBurst");
    }

    #[test]
    fn zen_five_is_split_by_model_instead_of_all_reporting_granite_ridge() {
        // The bug this fixes: `(0x1a, _)` labelled every Zen 5 part with the
        // desktop codename, so a Strix Point laptop and a Turin server both
        // claimed to be a Granite Ridge desktop.
        assert_eq!(codename_for("AuthenticAMD", 0x1a, 0x44), "Granite Ridge (Zen 5)");
        assert_eq!(codename_for("AuthenticAMD", 0x1a, 0x24), "Strix Point (Zen 5)");
        assert_eq!(codename_for("AuthenticAMD", 0x1a, 0x02), "Turin (Zen 5)");
    }

    #[test]
    fn an_unknown_model_falls_back_to_a_generation_that_is_still_true() {
        // A part released after this table was written must degrade to a
        // correct generation, never to a wrong codename and never to blank.
        assert_eq!(codename_for("AuthenticAMD", 0x1a, 0xF0), "Zen 5");
        assert_eq!(codename_for("AuthenticAMD", 0x19, 0xF0), "Zen 3/Zen 4");
        assert_eq!(codename_for("AuthenticAMD", 0x17, 0xF0), "Zen/Zen+/Zen 2");
        // Not "Intel Core": family 6 carries Atom parts too, so that would
        // be a wrong brand rather than a cautious one.
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xFE), "Intel (family 6h)");
    }

    #[test]
    fn first_generation_ryzen_is_no_longer_labelled_zen_2() {
        // `(0x17, _) => "Matisse/Renoir (Zen 2)"` called Summit Ridge a Zen 2
        // part. Family 17h spans Zen through Zen 2.
        assert_eq!(codename_for("AuthenticAMD", 0x17, 0x01), "Summit Ridge/Naples (Zen)");
    }

    #[test]
    fn zen_three_and_zen_four_share_family_nineteen() {
        assert_eq!(codename_for("AuthenticAMD", 0x19, 0x21), "Vermeer (Zen 3)");
        assert_eq!(codename_for("AuthenticAMD", 0x19, 0x50), "Cezanne (Zen 3)");
        assert_eq!(codename_for("AuthenticAMD", 0x19, 0x44), "Rembrandt (Zen 3+)");
        assert_eq!(codename_for("AuthenticAMD", 0x19, 0x61), "Raphael (Zen 4)");
        assert_eq!(codename_for("AuthenticAMD", 0x19, 0x74), "Phoenix (Zen 4)");
        assert_eq!(codename_for("AuthenticAMD", 0x19, 0x01), "Milan (Zen 3)");
    }

    #[test]
    fn a_non_x86_vendor_string_says_nothing_rather_than_guessing() {
        assert_eq!(codename_for("Apple", 0x0, 0x0), "");
        assert_eq!(codename_for("Qualcomm", 0x0, 0x0), "");
        assert_eq!(codename_for("", 0x6, 0x97), "");
    }

    // --- Cross-check against upstream OpenHardwareMonitor PR #1671 ---------
    //
    // "New Intel Architectures" (Leckrosh, Jul 2026) is the only open upstream
    // PR that overlaps this table. It cannot be merged — the C# tree it edits
    // is reference material this build never compiles — so it was treated as a
    // second opinion on the model numbers instead, and checked model by model.
    //
    // Every architecture it adds was already covered here. Two of its model
    // numbers were *not* adopted, which is the part worth pinning: 0xAB
    // (claimed Meteor Lake) and 0xBC (claimed Lunar Lake) appear in neither
    // the Linux kernel's `arch/x86/include/asm/intel-family.h` nor
    // LibreHardwareMonitor's `IntelCpu.cs` — the engine that actually reads
    // sensors on Windows here. Two independent sources having no such models,
    // and the PR citing none, is not enough to name a part.
    //
    // They are not special-cased. They fall to the family arm and report
    // "Intel (family 6h)", which is the correct answer for a model nobody can
    // source: honest rather than wrong.

    #[test]
    fn every_model_upstream_pr_1671_adds_is_already_covered() {
        for (model, expected) in [
            (0x97, "Alder Lake (Golden Cove/Gracemont)"),
            (0x9A, "Alder Lake (Golden Cove/Gracemont)"),
            (0xA7, "Rocket Lake (Cypress Cove)"),
            (0xB7, "Raptor Lake (Raptor Cove/Gracemont)"),
            (0xBA, "Raptor Lake (Raptor Cove/Gracemont)"),
            (0xBF, "Raptor Lake (Raptor Cove/Gracemont)"),
            (0xAA, "Meteor Lake (Redwood Cove/Crestmont)"),
            (0xAC, "Meteor Lake (Redwood Cove/Crestmont)"),
            (0xB5, "Arrow Lake (Lion Cove/Skymont)"),
            (0xC5, "Arrow Lake (Lion Cove/Skymont)"),
            (0xC6, "Arrow Lake (Lion Cove/Skymont)"),
            (0xBD, "Lunar Lake (Lion Cove/Skymont)"),
            (0xCC, "Panther Lake (Cougar Cove/Darkmont)"),
        ] {
            assert_eq!(
                codename_for("GenuineIntel", 0x6, model),
                expected,
                "model {model:#04X} from upstream PR #1671",
            );
        }
    }

    #[test]
    fn unsourced_models_report_the_family_rather_than_a_guessed_part() {
        // Upstream PR #1671 assigns these; no primary source does. If a
        // future kernel header or LHM release adds either, this test is the
        // thing that should fail and prompt naming them properly.
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xAB), "Intel (family 6h)");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xBC), "Intel (family 6h)");
    }

    #[test]
    fn alder_lake_n_is_not_folded_into_raptor_lake() {
        // Upstream PR #1671 groups 0xBE with Raptor Lake. Both
        // `intel-family.h` (INTEL_ALDERLAKE_N) and LibreHardwareMonitor call
        // it Alder Lake-N, so the PR is the outlier and was not followed.
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xBE), "Alder Lake-N (Gracemont)");
    }

    #[test]
    fn parts_no_upstream_source_covers_yet_are_still_named() {
        // Bartlett Lake, Clearwater Forest, Wildcat Lake, Panther Lake-R,
        // Nova Lake and Diamond Rapids are absent from both upstream OHM and
        // LibreHardwareMonitor's tables; `intel-family.h` carries all six.
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xD7), "Bartlett Lake (Raptor Cove)");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xDD), "Clearwater Forest (Darkmont)");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xD5), "Wildcat Lake");
        assert_eq!(codename_for("GenuineIntel", 0x6, 0xE5), "Panther Lake (Cougar Cove/Darkmont)");
        assert_eq!(codename_for("GenuineIntel", 0x12, 0x03), "Nova Lake (Coyote Cove/Arctic Wolf)");
        assert_eq!(codename_for("GenuineIntel", 0x13, 0x01), "Diamond Rapids (Panther Cove)");
    }
}
