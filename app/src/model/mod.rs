//! Core hardware/sensor data model.
//!
//! These types mirror OpenHardwareMonitor's `Hardware/ISensor.cs` and
//! `Hardware/IHardware.cs` so the port stays faithful and the UI can speak the
//! same vocabulary regardless of which [`crate::source::SensorSource`] backend
//! (LHM bridge or native Rust) produced the data.

pub mod hexblob;
pub mod storage;
pub mod topology;

use serde::{Deserialize, Serialize};

/// Physical quantity a sensor reports. Comment shows the canonical unit.
/// Superset of OHM's `SensorType`, extended with LibreHardwareMonitor's
/// additional kinds so the LHM bridge can map 1:1.
// `Ord` so metrics and grouped views can iterate types in a stable order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SensorType {
    Voltage,      // V
    Current,      // A
    Clock,        // MHz
    Temperature,  // °C
    Load,         // %
    Frequency,    // Hz
    Fan,          // RPM
    Flow,         // L/h
    Control,      // %
    Level,        // %
    Factor,       // 1
    Power,        // W
    Data,         // GB = 2^30 bytes
    SmallData,    // MB = 2^20 bytes
    Throughput,   // MB/s
    TimeSpan,     // s
    Energy,       // mWh
    Noise,        // dBA
    Conductivity, // µS/cm
    Humidity,     // %
    #[serde(other)]
    Unknown,
}

impl SensorType {
    /// Canonical display unit, as HWiNFO/OHM present it.
    pub fn unit(self) -> &'static str {
        match self {
            SensorType::Voltage => "V",
            SensorType::Current => "A",
            SensorType::Clock => "MHz",
            SensorType::Temperature => "°C",
            SensorType::Load => "%",
            SensorType::Frequency => "Hz",
            SensorType::Fan => "RPM",
            SensorType::Flow => "L/h",
            SensorType::Control => "%",
            SensorType::Level => "%",
            SensorType::Factor => "",
            SensorType::Power => "W",
            SensorType::Data => "GB",
            SensorType::SmallData => "MB",
            SensorType::Throughput => "MB/s",
            SensorType::TimeSpan => "s",
            SensorType::Energy => "mWh",
            SensorType::Noise => "dBA",
            SensorType::Conductivity => "µS/cm",
            SensorType::Humidity => "%",
            SensorType::Unknown => "",
        }
    }
}

/// Category of a hardware node. Superset of OHM's `HardwareType`, extended
/// with LibreHardwareMonitor's categories so the LHM bridge can map 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HardwareType {
    #[serde(alias = "Motherboard")]
    Mainboard,
    SuperIO,
    #[serde(alias = "CPU")]
    Cpu,
    #[serde(alias = "RAM", alias = "Memory")]
    Ram,
    GpuNvidia,
    #[serde(alias = "GpuAmd", alias = "GpuAti")]
    GpuAti,
    GpuIntel,
    TBalancer,
    Heatmaster,
    #[serde(alias = "HDD")]
    Hdd,
    Storage,
    Network,
    Cooler,
    EmbeddedController,
    Psu,
    Battery,
    #[serde(other)]
    Unknown,
}

/// A single sensor reading with running statistics.
///
/// `value`/`min`/`max` are `Option` because a sensor can be present but not yet
/// have produced a reading (mirrors OHM's `float?`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sensor {
    /// Stable path-like id, e.g. `/intelcpu/0/temperature/0` (see OHM `Identifier`).
    pub identifier: String,
    pub name: String,
    #[serde(rename = "type")]
    pub sensor_type: SensorType,
    pub index: u32,
    pub value: Option<f32>,
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub avg: Option<f32>,
}

/// A hardware node: a device with its sensors and optional sub-devices
/// (e.g. a mainboard containing a Super-I/O chip).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hardware {
    pub identifier: String,
    pub name: String,
    #[serde(rename = "type")]
    pub hardware_type: HardwareType,
    pub sensors: Vec<Sensor>,
    #[serde(default)]
    pub sub_hardware: Vec<Hardware>,
}
