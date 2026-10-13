//! The System Management Controller, read through the `AppleSMC` user client.
//!
//! On **Intel** Macs this is the only source of most sensors: CPU/GPU die and
//! proximity temperatures, every fan, and the package/system power rails all
//! live behind four-character SMC keys (`TC0P`, `F0Ac`, `PSTR`, …). There is
//! no IOHID temperature plane and no IOReport energy model on those machines.
//!
//! On **Apple Silicon** the SMC still exists — not under the class name
//! `AppleSMC`, but as `AppleSMCKeysEndpoint` (a subclass, so matching
//! `AppleSMC` finds it). Temperatures there are already covered by the HID
//! plane, so on ARM this module contributes what HID can't: fans (MacBook Pro,
//! Mac mini, Mac Studio), total system power and the DC-in rail.
//!
//! Reading needs no privilege: `IOServiceOpen` on the SMC succeeds for an
//! ordinary user, and only *writes* (fan overrides) are gated. This module
//! never writes.
//!
//! The wire format is the 80-byte `SMCParamStruct` used by every open-source
//! SMC tool since smcFanControl; its layout is pinned by a size test below
//! because a single misplaced field silently returns garbage.

#![allow(non_upper_case_globals)]

use crate::model::{Sensor, SensorType};

use super::iokit::{self, io_object_t, kern_return_t, IoObject, KERN_SUCCESS};
use super::sensor_opt;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceOpen(
        service: io_object_t,
        owning_task: mach2::port::mach_port_t,
        conn_type: u32,
        connect: *mut io_object_t,
    ) -> kern_return_t;
    fn IOServiceClose(connect: io_object_t) -> kern_return_t;
    fn IOConnectCallStructMethod(
        connection: io_object_t,
        selector: u32,
        input: *const libc::c_void,
        input_size: usize,
        output: *mut libc::c_void,
        output_size: *mut usize,
    ) -> kern_return_t;
}

/// `kSMCHandleYPCEvent`: the one user-client method; the sub-command goes in
/// `data8`.
const SELECTOR_HANDLE_YPC_EVENT: u32 = 2;
const CMD_READ_KEY: u8 = 5;
const CMD_GET_KEY_FROM_INDEX: u8 = 8;
const CMD_GET_KEY_INFO: u8 = 9;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Version {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct PLimitData {
    version: u16,
    length: u16,
    cpu_p_limit: u32,
    gpu_p_limit: u32,
    mem_p_limit: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct KeyInfo {
    data_size: u32,
    data_type: u32,
    data_attributes: u8,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ParamStruct {
    key: u32,
    vers: Version,
    p_limit_data: PLimitData,
    key_info: KeyInfo,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

/// A key's declared type and payload width, as reported by the SMC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyType {
    pub data_type: [u8; 4],
    pub size: u32,
}

/// Four ASCII characters packed big-endian, which is how the SMC addresses
/// keys and names types.
pub fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

pub fn fourcc_str(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

/// An open connection to the SMC user client.
pub struct Smc {
    conn: io_object_t,
}

// The connection is a Mach port name, valid from any thread of this task, and
// `Smc` only ever issues synchronous calls through `&mut self`.
unsafe impl Send for Smc {}

impl Drop for Smc {
    fn drop(&mut self) {
        unsafe { IOServiceClose(self.conn) };
    }
}

impl Smc {
    /// Open the SMC, or `None` on a machine (or VM) that has none.
    ///
    /// On Apple Silicon several `AppleSMC` subclasses are registered (charger,
    /// PMU, sensor dispatcher) and only `AppleSMCKeysEndpoint` answers the key
    /// protocol, so prefer it; on Intel there is exactly one `AppleSMC`.
    pub fn open() -> Option<Self> {
        let services = iokit::matching_services("AppleSMC");
        let preferred: Vec<&IoObject> = services
            .iter()
            .filter(|s| iokit::object_class(s.0).as_deref() == Some("AppleSMCKeysEndpoint"))
            .chain(services.iter().filter(|s| iokit::object_class(s.0).as_deref() == Some("AppleSMC")))
            .collect();
        for service in preferred {
            let mut conn: io_object_t = 0;
            let kr = unsafe {
                IOServiceOpen(service.0, mach2::traps::mach_task_self(), 0, &mut conn)
            };
            if kr != KERN_SUCCESS || conn == 0 {
                continue;
            }
            let mut smc = Smc { conn };
            // A connection that can't even report its key count is not one
            // that will read anything else either.
            if smc.key_count().is_some_and(|n| n > 0) {
                return Some(smc);
            }
        }
        None
    }

    fn call(&mut self, input: &ParamStruct) -> Option<ParamStruct> {
        let mut output = ParamStruct::default();
        let mut out_size = std::mem::size_of::<ParamStruct>();
        let kr = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                SELECTOR_HANDLE_YPC_EVENT,
                (input as *const ParamStruct).cast(),
                std::mem::size_of::<ParamStruct>(),
                (&mut output as *mut ParamStruct).cast(),
                &mut out_size,
            )
        };
        // `result` is the SMC's own status: 0 ok, 0x84 key not found.
        (kr == KERN_SUCCESS && output.result == 0).then_some(output)
    }

    /// The declared type of a key, or `None` if the key doesn't exist.
    pub fn key_info(&mut self, key: u32) -> Option<KeyType> {
        let input = ParamStruct { key, data8: CMD_GET_KEY_INFO, ..Default::default() };
        let out = self.call(&input)?;
        (out.key_info.data_size > 0 && out.key_info.data_size <= 32).then_some(KeyType {
            data_type: fourcc_str(out.key_info.data_type),
            size: out.key_info.data_size,
        })
    }

    /// The raw payload of a key whose type is already known.
    pub fn read_raw(&mut self, key: u32, ty: KeyType) -> Option<Vec<u8>> {
        let mut input = ParamStruct { key, data8: CMD_READ_KEY, ..Default::default() };
        input.key_info.data_size = ty.size;
        let out = self.call(&input)?;
        Some(out.bytes[..ty.size as usize].to_vec())
    }

    /// How many keys the SMC publishes (`#KEY`).
    pub fn key_count(&mut self) -> Option<u32> {
        let k = fourcc(b"#KEY");
        let ty = self.key_info(k)?;
        let raw = self.read_raw(k, ty)?;
        let bytes: [u8; 4] = raw.get(..4)?.try_into().ok()?;
        Some(u32::from_be_bytes(bytes))
    }

    /// The `index`-th key's name.
    pub fn key_at(&mut self, index: u32) -> Option<[u8; 4]> {
        let input = ParamStruct { data8: CMD_GET_KEY_FROM_INDEX, data32: index, ..Default::default() };
        let out = self.call(&input)?;
        Some(fourcc_str(out.key))
    }

    /// Every key the SMC publishes, with its type. A few thousand round trips
    /// on Apple Silicon (~1–2 k keys), so callers do this once at startup.
    pub fn all_keys(&mut self) -> Vec<([u8; 4], KeyType)> {
        let Some(count) = self.key_count() else {
            return Vec::new();
        };
        // Guard against a nonsense count turning into a multi-second stall.
        (0..count.min(8192))
            .filter_map(|i| {
                let key = self.key_at(i)?;
                let ty = self.key_info(fourcc(&key))?;
                Some((key, ty))
            })
            .collect()
    }
}

/// Decode an SMC payload to a number.
///
/// Intel SMCs store everything big-endian, in fixed-point formats named by
/// their integer/fraction split: `sp78` is signed with 8 fraction bits,
/// `fpe2` unsigned with 2 (`e` = 14 integer bits). Apple Silicon moved to
/// little-endian IEEE `flt `. The fixed-point names are decoded generically
/// from the type string rather than from a table, because the full set
/// (`sp1e` … `spf0`, `fp1f` … `fpe2`) is large and regular.
pub fn decode(ty: KeyType, raw: &[u8]) -> Option<f64> {
    let t = &ty.data_type;
    let be_uint = |n: usize| -> Option<u64> {
        let b = raw.get(..n)?;
        Some(b.iter().fold(0u64, |acc, &x| (acc << 8) | x as u64))
    };
    let value = match t {
        b"flt " if raw.len() >= 4 => f32::from_le_bytes(raw[..4].try_into().ok()?) as f64,
        b"ui8 " | b"ui16" | b"ui32" | b"ui64" => be_uint(ty.size as usize)? as f64,
        b"si8 " => raw.first().map(|&b| b as i8 as f64)?,
        b"si16" => i16::from_be_bytes(raw.get(..2)?.try_into().ok()?) as f64,
        b"si32" => i32::from_be_bytes(raw.get(..4)?.try_into().ok()?) as f64,
        // 48.16 fixed point, little-endian — Apple Silicon fan targets and
        // some rails.
        b"ioft" if raw.len() >= 8 => {
            u64::from_le_bytes(raw[..8].try_into().ok()?) as f64 / 65536.0
        }
        [b's', b'p', i, f] | [b'f', b'p', i, f] if ty.size == 2 => {
            let int_bits = hex_digit(*i)?;
            let frac_bits = hex_digit(*f)?;
            let signed = t[0] == b's';
            // `sp78` is 1 sign + 7 integer + 8 fraction = 16; `fpe2` is
            // 14 + 2. A type whose split doesn't add up isn't one we know.
            if int_bits + frac_bits + u32::from(signed) != 16 {
                return None;
            }
            let word = u16::from_be_bytes(raw.get(..2)?.try_into().ok()?);
            let scaled = if signed { word as i16 as f64 } else { word as f64 };
            scaled / f64::from(1u32 << frac_bits)
        }
        _ => return None,
    };
    value.is_finite().then_some(value)
}

fn hex_digit(c: u8) -> Option<u32> {
    (c as char).to_digit(16)
}

// ---- Sensor collector ----------------------------------------------------

/// Which hardware node a reading belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Cpu,
    Gpu,
    /// Board-level: fans, total system power, the DC-in rail, ambient probes.
    System,
}

/// The SMC readings for one poll, already split by destination node.
#[derive(Default)]
pub struct Readings {
    pub cpu: Vec<Sensor>,
    pub gpu: Vec<Sensor>,
    pub system: Vec<Sensor>,
}

impl Readings {
    fn push(&mut self, group: Group, s: Sensor) {
        match group {
            Group::Cpu => self.cpu.push(s),
            Group::Gpu => self.gpu.push(s),
            Group::System => self.system.push(s),
        }
    }
}

/// Which SMC generation a key-table entry applies to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum On {
    Both,
    Intel,
    Arm,
}

/// A curated key. `#` in the key is a digit placeholder (`TC#C` covers
/// `TC0C` … `TC9C`), and `##` in the name becomes `#` plus that digit.
struct KeyDef {
    key: &'static str,
    name: &'static str,
    sensor_type: SensorType,
    group: Group,
    on: On,
}

const fn k(key: &'static str, name: &'static str, sensor_type: SensorType, group: Group, on: On) -> KeyDef {
    KeyDef { key, name, sensor_type, group, on }
}

/// Named keys, from the tables maintained by the Stats project
/// (github.com/exelban/stats, `Modules/Sensors/values.swift`) — the most
/// widely exercised public source for what each key measures.
///
/// Deliberately short on Apple Silicon. Its ~150 temperature keys duplicate
/// the IOHID plane the backend already reads (under different, cryptic
/// names), so ARM only takes what nothing else provides: total system power,
/// the DC-in rail, and the Wi-Fi/airflow probes. Die temperatures arrive there
/// as the aggregates in [`FAMILIES`]. Intel has no other source at all, so it
/// gets the full set.
const KEYS: &[KeyDef] = &[
    // --- Board power ------------------------------------------------------
    k("PSTR", "System Total", SensorType::Power, Group::System, On::Both),
    k("PDTR", "DC In", SensorType::Power, Group::System, On::Both),
    k("VD0R", "DC In", SensorType::Voltage, Group::System, On::Both),
    k("ID0R", "DC In", SensorType::Current, Group::System, On::Both),
    // --- Intel CPU --------------------------------------------------------
    k("TC0P", "CPU Proximity", SensorType::Temperature, Group::Cpu, On::Intel),
    k("TC0D", "CPU Die", SensorType::Temperature, Group::Cpu, On::Intel),
    k("TC0E", "CPU Die (virtual)", SensorType::Temperature, Group::Cpu, On::Intel),
    k("TC0F", "CPU Die (filtered)", SensorType::Temperature, Group::Cpu, On::Intel),
    k("TC0H", "CPU Heatsink", SensorType::Temperature, Group::Cpu, On::Intel),
    k("TCAD", "CPU Package", SensorType::Temperature, Group::Cpu, On::Intel),
    k("TCXC", "CPU PECI", SensorType::Temperature, Group::Cpu, On::Intel),
    k("TC#C", "CPU Core ##", SensorType::Temperature, Group::Cpu, On::Intel),
    k("PCPC", "CPU Package", SensorType::Power, Group::Cpu, On::Intel),
    k("PCPT", "CPU Package Total", SensorType::Power, Group::Cpu, On::Intel),
    k("PCTR", "CPU Total", SensorType::Power, Group::Cpu, On::Intel),
    k("PC0C", "CPU Core", SensorType::Power, Group::Cpu, On::Intel),
    k("PC0G", "CPU Graphics", SensorType::Power, Group::Cpu, On::Intel),
    k("VC0C", "CPU Core", SensorType::Voltage, Group::Cpu, On::Intel),
    k("VCAC", "CPU IA", SensorType::Voltage, Group::Cpu, On::Intel),
    k("VCSC", "CPU System Agent", SensorType::Voltage, Group::Cpu, On::Intel),
    k("IC0R", "CPU High Side", SensorType::Current, Group::Cpu, On::Intel),
    // --- Intel GPU (board-level: the SMC doesn't say which adapter) ------
    k("TCGC", "Intel Graphics", SensorType::Temperature, Group::System, On::Intel),
    k("TG0P", "GPU Proximity", SensorType::Temperature, Group::System, On::Intel),
    k("TG0D", "GPU Die", SensorType::Temperature, Group::System, On::Intel),
    k("TGDD", "GPU Die (AMD)", SensorType::Temperature, Group::System, On::Intel),
    k("TG0H", "GPU Heatsink", SensorType::Temperature, Group::System, On::Intel),
    k("PCPG", "Intel Graphics", SensorType::Power, Group::System, On::Intel),
    k("PG0R", "GPU", SensorType::Power, Group::System, On::Intel),
    k("VG0C", "GPU Core", SensorType::Voltage, Group::System, On::Intel),
    k("IG0R", "GPU High Side", SensorType::Current, Group::System, On::Intel),
    // --- Intel board ------------------------------------------------------
    k("TA0P", "Ambient", SensorType::Temperature, Group::System, On::Intel),
    k("Tm0P", "Mainboard", SensorType::Temperature, Group::System, On::Intel),
    k("TN0P", "Northbridge Proximity", SensorType::Temperature, Group::System, On::Intel),
    k("TN0D", "Northbridge Die", SensorType::Temperature, Group::System, On::Intel),
    k("TPCD", "PCH Die", SensorType::Temperature, Group::System, On::Intel),
    k("TM0P", "Memory Proximity", SensorType::Temperature, Group::System, On::Intel),
    k("TH0P", "Drive Bay", SensorType::Temperature, Group::System, On::Intel),
    k("TL0P", "Display", SensorType::Temperature, Group::System, On::Intel),
    k("Ts0P", "Palm Rest", SensorType::Temperature, Group::System, On::Intel),
    k("TI0P", "Thunderbolt", SensorType::Temperature, Group::System, On::Intel),
    k("TB0T", "Battery", SensorType::Temperature, Group::System, On::Intel),
    k("PMTR", "Memory Total", SensorType::Power, Group::System, On::Intel),
    k("VM0R", "Memory", SensorType::Voltage, Group::System, On::Intel),
    k("VP0R", "12V Rail", SensorType::Voltage, Group::System, On::Intel),
    // --- Both -------------------------------------------------------------
    k("TW0P", "Wi-Fi Module", SensorType::Temperature, Group::System, On::Both),
    // MacBook Pro intake probes; absent on fanless machines.
    k("TaLP", "Airflow Left", SensorType::Temperature, Group::System, On::Arm),
    k("TaRF", "Airflow Right", SensorType::Temperature, Group::System, On::Arm),
];

/// Apple Silicon die-temperature families, published as average and hottest.
///
/// Each SoC generation renumbers its per-core keys (Stats keeps a separate
/// table per M1/M2/M3/M4/M5 *variant*), so naming individual keys would
/// mislabel cores on every chip the table doesn't list. The prefixes are what
/// stays stable: `Tp`/`Te` are CPU cluster dies and `Tg` the GPU on every
/// generation so far, which is how macmon and Stats derive their CPU and GPU
/// temperatures too.
struct Family {
    prefixes: &'static [&'static str],
    label: &'static str,
    id: &'static str,
    group: Group,
}

const FAMILIES: &[Family] = &[
    Family { prefixes: &["Tp", "Te"], label: "CPU Die", id: "cpu", group: Group::Cpu },
    Family { prefixes: &["Tg"], label: "GPU Die", id: "gpu", group: Group::Gpu },
];

/// One resolved key: where it lives and how to read it.
#[derive(Clone, Copy)]
struct Channel {
    key: u32,
    ty: KeyType,
}

struct Named {
    channel: Channel,
    id: String,
    name: String,
    sensor_type: SensorType,
    group: Group,
}

struct Aggregate {
    channels: Vec<Channel>,
    label: &'static str,
    id: &'static str,
    group: Group,
}

struct Fan {
    number: u32,
    actual: Channel,
    target: Option<Channel>,
}

/// Sensors read from the SMC, with the key set fixed at construction.
///
/// Enumeration is a few thousand user-client calls, so it happens once; each
/// poll then reads only the keys that were chosen, with their types already
/// known. Fixing the set up front is also what keeps the published sensor
/// list stable from poll to poll (see `sensor_set_is_stable_across_polls`):
/// a key that stops answering reports `None` rather than vanishing.
pub struct SmcSensors {
    smc: Option<Smc>,
    named: Vec<Named>,
    aggregates: Vec<Aggregate>,
    fans: Vec<Fan>,
    key_count: usize,
}

/// Plausible range per sensor type. Anything outside is a sentinel (0 °C from
/// a powered-down probe, `-127`, `0xFFFF`-derived nonsense), not a reading.
fn plausible(sensor_type: SensorType, v: f64) -> bool {
    match sensor_type {
        SensorType::Temperature => (1.0..=150.0).contains(&v),
        SensorType::Power => (0.0..=2000.0).contains(&v),
        SensorType::Voltage => (0.0..=60.0).contains(&v),
        SensorType::Current => (0.0..=100.0).contains(&v),
        SensorType::Fan => (0.0..=20_000.0).contains(&v),
        _ => v.is_finite(),
    }
}

/// Whether the SMC on this generation encodes a type in a way [`decode`]
/// reads correctly.
///
/// Apple Silicon's SMC still *labels* integer keys `ui16`/`ui32`, but most of
/// them are little-endian in practice (battery voltage `B0AV` reads 60976 as
/// big-endian, 12526 mV — matching `AppleSmartBattery` — as little), and a few
/// "ui32" keys hold IEEE floats. Byte order varies per key, so on ARM only the
/// unambiguous IEEE `flt ` type is trusted, plus single bytes, which have no
/// byte order. Intel's SMC is consistently big-endian.
fn trusted(arm: bool, ty: KeyType) -> bool {
    !arm || (&ty.data_type == b"flt " && ty.size == 4) || ty.size == 1
}

fn expand(def: &KeyDef) -> Vec<([u8; 4], String)> {
    let key = def.key.as_bytes();
    let Ok(key): Result<[u8; 4], _> = key.try_into() else {
        return Vec::new();
    };
    match key.iter().position(|&c| c == b'#') {
        None => vec![(key, def.name.to_string())],
        Some(pos) => (0..=9u8)
            .map(|digit| {
                let mut concrete = key;
                concrete[pos] = b'0' + digit;
                // Core keys start at 0 on some models and 1 on others, so
                // the label carries the key's own digit rather than a count
                // that would be off by one on half of them.
                let name = def.name.replace("##", &format!("#{digit}"));
                (concrete, name)
            })
            .collect(),
    }
}

fn type_slug(t: SensorType) -> &'static str {
    match t {
        SensorType::Temperature => "temperature",
        SensorType::Power => "power",
        SensorType::Voltage => "voltage",
        SensorType::Current => "current",
        SensorType::Fan => "fan",
        _ => "value",
    }
}

impl SmcSensors {
    pub fn new() -> Self {
        let arm = crate::sysinfo::is_apple_silicon();
        let mut this =
            Self { smc: Smc::open(), named: Vec::new(), aggregates: Vec::new(), fans: Vec::new(), key_count: 0 };
        let Some(smc) = this.smc.as_mut() else {
            return this;
        };

        let keys = smc.all_keys();
        this.key_count = keys.len();
        let types: std::collections::HashMap<[u8; 4], KeyType> = keys.iter().copied().collect();

        // A key qualifies if it exists, its type is one we decode correctly on
        // this generation, and its first reading is physically plausible —
        // which filters the many keys that exist but read as 0 or a sentinel.
        let probe = |smc: &mut Smc, key: [u8; 4], sensor_type: SensorType| -> Option<Channel> {
            let ty = *types.get(&key)?;
            if !trusted(arm, ty) {
                return None;
            }
            let channel = Channel { key: fourcc(&key), ty };
            let v = decode(ty, &smc.read_raw(channel.key, ty)?)?;
            plausible(sensor_type, v).then_some(channel)
        };

        for def in KEYS {
            let applies = match def.on {
                On::Both => true,
                On::Intel => !arm,
                On::Arm => arm,
            };
            if !applies {
                continue;
            }
            for (key, name) in expand(def) {
                if let Some(channel) = probe(smc, key, def.sensor_type) {
                    let key_str = String::from_utf8_lossy(&key).into_owned();
                    this.named.push(Named {
                        channel,
                        id: format!("/smc/0/{}/{key_str}", type_slug(def.sensor_type)),
                        name: name.clone(),
                        sensor_type: def.sensor_type,
                        group: def.group,
                    });
                }
            }
        }

        if arm {
            for family in FAMILIES {
                let mut members: Vec<[u8; 4]> = keys
                    .iter()
                    .map(|(key, _)| *key)
                    .filter(|key| family.prefixes.iter().any(|p| key.starts_with(p.as_bytes())))
                    .collect();
                members.sort_unstable();
                let channels: Vec<Channel> = members
                    .into_iter()
                    .filter_map(|key| probe(smc, key, SensorType::Temperature))
                    .collect();
                if !channels.is_empty() {
                    this.aggregates.push(Aggregate {
                        channels,
                        label: family.label,
                        id: family.id,
                        group: family.group,
                    });
                }
            }
        }

        // Fans: `FNum` is a single byte on both generations. A fanless Mac
        // either lacks the key or reports 0.
        let fan_count = types
            .get(b"FNum")
            .and_then(|ty| decode(*ty, &smc.read_raw(fourcc(b"FNum"), *ty)?))
            .map(|n| n as u32)
            .unwrap_or(0)
            .min(8);
        for i in 0..fan_count {
            let key = |suffix: &[u8; 2]| -> [u8; 4] { [b'F', b'0' + i as u8, suffix[0], suffix[1]] };
            // Fan speed can legitimately read 0 (stopped at idle), so only
            // the type matters here, not the first value.
            let channel = |key: [u8; 4]| -> Option<Channel> {
                let ty = *types.get(&key)?;
                (trusted(arm, ty) && decode(ty, &[0u8; 32][..ty.size as usize]).is_some())
                    .then_some(Channel { key: fourcc(&key), ty })
            };
            let Some(actual) = channel(key(b"Ac")) else {
                continue;
            };
            this.fans.push(Fan { number: i + 1, actual, target: channel(key(b"Tg")) });
        }

        this
    }

    /// One-line status for the Settings dialog.
    pub fn describe(&self) -> String {
        if self.smc.is_none() {
            return "unavailable — no fan or board-power sensors".to_string();
        }
        let sensors = self.named.len() + self.aggregates.len() * 2 + self.fans.len();
        let fans = match self.fans.len() {
            0 => "no fans".to_string(),
            1 => "1 fan".to_string(),
            n => format!("{n} fans"),
        };
        format!("ok — {} keys, {sensors} sensors, {fans}", self.key_count)
    }

    fn value(&mut self, channel: Channel) -> Option<f64> {
        let smc = self.smc.as_mut()?;
        decode(channel.ty, &smc.read_raw(channel.key, channel.ty)?)
    }

    pub fn sample(&mut self) -> Readings {
        let mut out = Readings::default();

        for i in 0..self.named.len() {
            let (channel, sensor_type) = (self.named[i].channel, self.named[i].sensor_type);
            let v = self.value(channel).filter(|v| plausible(sensor_type, *v));
            let n = &self.named[i];
            out.push(n.group, sensor_opt(&n.id, &n.name, n.sensor_type, i as u32, v.map(|v| v as f32)));
        }

        for i in 0..self.aggregates.len() {
            let channels = self.aggregates[i].channels.clone();
            let readings: Vec<f64> = channels
                .into_iter()
                .filter_map(|c| self.value(c))
                .filter(|v| plausible(SensorType::Temperature, *v))
                .collect();
            let (avg, max) = if readings.is_empty() {
                (None, None)
            } else {
                let avg = readings.iter().sum::<f64>() / readings.len() as f64;
                let max = readings.iter().copied().fold(f64::MIN, f64::max);
                (Some(avg as f32), Some(max as f32))
            };
            let a = &self.aggregates[i];
            out.push(
                a.group,
                sensor_opt(
                    &format!("/smc/0/temperature/{}-average", a.id),
                    &format!("{} (Average)", a.label),
                    SensorType::Temperature,
                    100 + 2 * i as u32,
                    avg,
                ),
            );
            out.push(
                a.group,
                sensor_opt(
                    &format!("/smc/0/temperature/{}-hottest", a.id),
                    &format!("{} (Hottest)", a.label),
                    SensorType::Temperature,
                    101 + 2 * i as u32,
                    max,
                ),
            );
        }

        for i in 0..self.fans.len() {
            let (number, actual, target) = (self.fans[i].number, self.fans[i].actual, self.fans[i].target);
            let rpm = self.value(actual).filter(|v| plausible(SensorType::Fan, *v));
            out.system.push(sensor_opt(
                &format!("/smc/0/fan/{}", number - 1),
                &format!("Fan #{number}"),
                SensorType::Fan,
                2 * i as u32,
                rpm.map(|v| v as f32),
            ));
            if let Some(target) = target {
                let rpm = self.value(target).filter(|v| plausible(SensorType::Fan, *v));
                out.system.push(sensor_opt(
                    &format!("/smc/0/fan/{}-target", number - 1),
                    &format!("Fan #{number} Target"),
                    SensorType::Fan,
                    2 * i as u32 + 1,
                    rpm.map(|v| v as f32),
                ));
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel validates the struct size, so a layout slip doesn't corrupt
    /// memory — it just makes every read fail. Pin it.
    #[test]
    fn param_struct_matches_the_kernel_layout() {
        assert_eq!(std::mem::size_of::<ParamStruct>(), 80);
        assert_eq!(std::mem::size_of::<KeyInfo>(), 12);
        assert_eq!(std::mem::offset_of!(ParamStruct, key_info), 28);
        assert_eq!(std::mem::offset_of!(ParamStruct, result), 40);
        assert_eq!(std::mem::offset_of!(ParamStruct, data8), 42);
        assert_eq!(std::mem::offset_of!(ParamStruct, data32), 44);
        assert_eq!(std::mem::offset_of!(ParamStruct, bytes), 48);
    }

    fn ty(t: &[u8; 4], size: u32) -> KeyType {
        KeyType { data_type: *t, size }
    }

    #[test]
    fn intel_fixed_point_formats_decode() {
        // 45.5 °C as sp78: 0x2D80.
        assert_eq!(decode(ty(b"sp78", 2), &[0x2d, 0x80]), Some(45.5));
        // Negative sp78: -1.0 = 0xFF00.
        assert_eq!(decode(ty(b"sp78", 2), &[0xff, 0x00]), Some(-1.0));
        // 1200 RPM as fpe2: 1200 << 2 = 0x12C0.
        assert_eq!(decode(ty(b"fpe2", 2), &[0x12, 0xc0]), Some(1200.0));
        // sp4b: 4 integer + 11 fraction + sign — e.g. a voltage of 1.5.
        assert_eq!(decode(ty(b"sp4b", 2), &[0x0c, 0x00]), Some(1.5));
        // fp88 — unsigned 8.8.
        assert_eq!(decode(ty(b"fp88", 2), &[0x01, 0x80]), Some(1.5));
    }

    #[test]
    fn apple_silicon_float_is_little_endian() {
        let raw = 12.25f32.to_le_bytes();
        assert_eq!(decode(ty(b"flt ", 4), &raw), Some(12.25));
    }

    #[test]
    fn integers_are_big_endian() {
        assert_eq!(decode(ty(b"ui8 ", 1), &[2]), Some(2.0));
        assert_eq!(decode(ty(b"ui16", 2), &[0x01, 0x00]), Some(256.0));
        assert_eq!(decode(ty(b"ui32", 4), &[0, 0, 0x01, 0x02]), Some(258.0));
        assert_eq!(decode(ty(b"si16", 2), &[0xff, 0xfe]), Some(-2.0));
    }

    #[test]
    fn unknown_or_malformed_types_decode_to_none() {
        assert_eq!(decode(ty(b"ch8*", 4), b"abcd"), None);
        // Split doesn't sum to 16 bits.
        assert_eq!(decode(ty(b"sp99", 2), &[0, 0]), None);
        // Truncated payload.
        assert_eq!(decode(ty(b"flt ", 4), &[0, 0]), None);
        assert_eq!(decode(ty(b"flt ", 4), &f32::NAN.to_le_bytes()), None);
    }

    /// Live: whatever SMC this machine has must at least report a key count
    /// and enumerate keys whose types are four printable characters.
    #[test]
    fn live_smc_enumerates_keys() {
        let Some(mut smc) = Smc::open() else {
            return crate::source::macos::absent("AppleSMC user client");
        };
        let count = smc.key_count().unwrap();
        assert!(count > 10, "SMC reports only {count} keys");
        let first = smc.key_at(0).expect("key 0");
        assert!(first.iter().all(|c| c.is_ascii_graphic() || *c == b' '));
    }

    #[test]
    #[ignore = "diagnostic probe; run explicitly"]
    fn dump_smc_keys() {
        let Some(mut smc) = Smc::open() else {
            println!("no SMC");
            return;
        };
        for (key, ty) in smc.all_keys() {
            let raw = smc.read_raw(fourcc(&key), ty).unwrap_or_default();
            println!(
                "{} {} {:>2} {:?}",
                String::from_utf8_lossy(&key),
                String::from_utf8_lossy(&ty.data_type),
                ty.size,
                decode(ty, &raw)
            );
        }
    }
}
