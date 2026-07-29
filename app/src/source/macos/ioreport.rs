//! CPU / GPU / ANE power via `libIOReport`.
//!
//! The "Energy Model" channel group publishes **cumulative energy counters**,
//! not instantaneous power. Watts therefore come from differencing two samples
//! and dividing by the wall-clock interval between them, which means the first
//! poll after startup produces no power sensors at all.
//!
//! `libIOReport.dylib` is not a file on disk on modern macOS — it lives in the
//! dyld shared cache — so it must be `dlopen`ed rather than linked (see
//! `dynlib`). Every symbol is optional and every failure degrades to "no power
//! sensors" rather than a panic.
//!
//! Channel names are firmware strings that differ per SoC generation, so units
//! are read from the channel's own unit label instead of being assumed.

use std::time::Instant;

use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFRelease, CFTypeRef};
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::string::CFStringRef;

use crate::model::{Sensor, SensorType};

use super::dynlib::Library;
use super::sensor;

// C signatures (SPI — kept here because a mismatch is UB):
//   CFMutableDictionaryRef IOReportCopyChannelsInGroup(CFStringRef group, CFStringRef subgroup,
//                                                      uint64_t, uint64_t, uint64_t);
//   IOReportSubscriptionRef IOReportCreateSubscription(void *, CFMutableDictionaryRef desired,
//                                                      CFMutableDictionaryRef *subbed,
//                                                      uint64_t, CFTypeRef);
//   CFDictionaryRef IOReportCreateSamples(IOReportSubscriptionRef, CFMutableDictionaryRef, CFTypeRef);
//   CFDictionaryRef IOReportCreateSamplesDelta(CFDictionaryRef prev, CFDictionaryRef now, CFTypeRef);
//   CFStringRef IOReportChannelGetChannelName(CFDictionaryRef);
//   CFStringRef IOReportChannelGetUnitLabel(CFDictionaryRef);
//   int32_t     IOReportChannelGetFormat(CFDictionaryRef);
//   int64_t     IOReportSimpleGetIntegerValue(CFDictionaryRef, int32_t);
type CopyChannelsInGroup =
    unsafe extern "C" fn(CFStringRef, CFStringRef, u64, u64, u64) -> CFDictionaryRef;
type CreateSubscription = unsafe extern "C" fn(
    *const libc::c_void,
    CFDictionaryRef,
    *mut CFDictionaryRef,
    u64,
    CFTypeRef,
) -> CFTypeRef;
type CreateSamples = unsafe extern "C" fn(CFTypeRef, CFDictionaryRef, CFTypeRef) -> CFDictionaryRef;
type CreateSamplesDelta =
    unsafe extern "C" fn(CFDictionaryRef, CFDictionaryRef, CFTypeRef) -> CFDictionaryRef;
type ChannelGetName = unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef;
type ChannelGetUnitLabel = unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef;
type ChannelGetFormat = unsafe extern "C" fn(CFDictionaryRef) -> i32;
type SimpleGetIntegerValue = unsafe extern "C" fn(CFDictionaryRef, i32) -> i64;

/// `kIOReportFormatSimple` — a single scalar per channel. The Energy Model
/// group uses this; State/Histogram channels (CPU residency, for instance) must
/// NOT be read with `IOReportSimpleGetIntegerValue`.
const FORMAT_SIMPLE: i32 = 1;

struct Api {
    _library: Library,
    create_samples: CreateSamples,
    create_samples_delta: CreateSamplesDelta,
    channel_name: ChannelGetName,
    channel_unit: ChannelGetUnitLabel,
    channel_format: ChannelGetFormat,
    integer_value: SimpleGetIntegerValue,
}

pub struct EnergyReporter {
    api: Option<Api>,
    subscription: CFTypeRef,
    channels: CFDictionaryRef,
    /// Previous cumulative sample + when it was taken, for the rate.
    prev: Option<(CFDictionaryRef, Instant)>,
}

// SAFETY: all three raw pointers are CoreFoundation objects owned exclusively
// by this struct. It is constructed on the main thread, moved once onto the
// poll thread, and thereafter used only from that thread; no handle is shared.
unsafe impl Send for EnergyReporter {}

impl Drop for EnergyReporter {
    fn drop(&mut self) {
        unsafe {
            if let Some((prev, _)) = self.prev.take() {
                if !prev.is_null() {
                    CFRelease(prev.cast());
                }
            }
            if !self.channels.is_null() {
                CFRelease(self.channels.cast());
            }
            if !self.subscription.is_null() {
                CFRelease(self.subscription);
            }
        }
    }
}

impl EnergyReporter {
    pub fn new() -> Self {
        let mut this = Self {
            api: None,
            subscription: std::ptr::null(),
            channels: std::ptr::null(),
            prev: None,
        };

        let Some(library) = Library::open("/usr/lib/libIOReport.dylib") else {
            return this;
        };

        let (copy_channels, create_subscription, create_samples, create_samples_delta, channel_name, channel_unit, channel_format, integer_value) = unsafe {
            (
                library.symbol::<CopyChannelsInGroup>("IOReportCopyChannelsInGroup"),
                library.symbol::<CreateSubscription>("IOReportCreateSubscription"),
                library.symbol::<CreateSamples>("IOReportCreateSamples"),
                library.symbol::<CreateSamplesDelta>("IOReportCreateSamplesDelta"),
                library.symbol::<ChannelGetName>("IOReportChannelGetChannelName"),
                library.symbol::<ChannelGetUnitLabel>("IOReportChannelGetUnitLabel"),
                library.symbol::<ChannelGetFormat>("IOReportChannelGetFormat"),
                library.symbol::<SimpleGetIntegerValue>("IOReportSimpleGetIntegerValue"),
            )
        };
        let (
            Some(copy_channels),
            Some(create_subscription),
            Some(create_samples),
            Some(create_samples_delta),
            Some(channel_name),
            Some(channel_unit),
            Some(channel_format),
            Some(integer_value),
        ) = (
            copy_channels,
            create_subscription,
            create_samples,
            create_samples_delta,
            channel_name,
            channel_unit,
            channel_format,
            integer_value,
        )
        else {
            return this;
        };

        // "Energy Model" is the group carrying CPU/GPU/ANE energy counters.
        let group = CFString::new("Energy Model");
        let desired =
            unsafe { copy_channels(group.as_concrete_TypeRef(), std::ptr::null(), 0, 0, 0) };
        if desired.is_null() {
            return this;
        }

        let mut subscribed: CFDictionaryRef = std::ptr::null();
        let subscription = unsafe {
            create_subscription(
                std::ptr::null(),
                desired,
                &mut subscribed,
                0,
                std::ptr::null(),
            )
        };
        unsafe { CFRelease(desired.cast()) };

        if subscription.is_null() || subscribed.is_null() {
            if !subscription.is_null() {
                unsafe { CFRelease(subscription) };
            }
            return this;
        }

        this.subscription = subscription;
        this.channels = subscribed;
        this.api = Some(Api {
            _library: library,
            create_samples,
            create_samples_delta,
            channel_name,
            channel_unit,
            channel_format,
            integer_value,
        });
        this
    }

    /// True when libIOReport resolved and an Energy Model subscription exists.
    pub fn available(&self) -> bool {
        self.api.is_some() && !self.subscription.is_null()
    }

    /// Power sensors in watts. Empty on the first call (no baseline) and
    /// whenever IOReport is unavailable.
    pub fn power_sensors(&mut self) -> Vec<Sensor> {
        let Some(api) = self.api.as_ref() else {
            return Vec::new();
        };

        let now_sample = unsafe { (api.create_samples)(self.subscription, self.channels, std::ptr::null()) };
        if now_sample.is_null() {
            return Vec::new();
        }
        let taken_at = Instant::now();

        let Some((prev_sample, prev_at)) = self.prev.take() else {
            // First poll: store the baseline and report nothing this tick.
            self.prev = Some((now_sample, taken_at));
            return Vec::new();
        };

        let interval = taken_at.duration_since(prev_at).as_secs_f64();
        let delta =
            unsafe { (api.create_samples_delta)(prev_sample, now_sample, std::ptr::null()) };
        unsafe { CFRelease(prev_sample.cast()) };
        self.prev = Some((now_sample, taken_at));

        if delta.is_null() || interval <= 0.0 {
            if !delta.is_null() {
                unsafe { CFRelease(delta.cast()) };
            }
            return Vec::new();
        }

        let sensors = self.read_delta(api, delta, interval);
        unsafe { CFRelease(delta.cast()) };
        sensors
    }

    fn read_delta(&self, api: &Api, delta: CFDictionaryRef, interval: f64) -> Vec<Sensor> {
        // The delta dictionary holds an "IOReportChannels" array of per-channel
        // dictionaries.
        let dict: CFDictionary<CFString, CFType> =
            unsafe { CFDictionary::wrap_under_get_rule(delta) };
        let Some(channels) =
            dict.find(CFString::new("IOReportChannels")).and_then(|v| super::iokit::as_array(&v))
        else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for (index, channel) in channels.iter().enumerate() {
            let raw = channel.as_CFTypeRef() as CFDictionaryRef;

            let name = unsafe { (api.channel_name)(raw) };
            if name.is_null() {
                continue;
            }
            let name = unsafe { CFString::wrap_under_get_rule(name) }.to_string();

            let unit = unsafe { (api.channel_unit)(raw) };
            let unit = if unit.is_null() {
                String::new()
            } else {
                unsafe { CFString::wrap_under_get_rule(unit) }.to_string()
            };

            // Only Simple-format channels hold a scalar. The second argument is
            // a element index, NOT a Python-style index-from-end: passing -1
            // makes libIOReport dereference 0xffffffff and segfault. Index 0 is
            // the only valid element for a Simple channel.
            if unsafe { (api.channel_format)(raw) } != FORMAT_SIMPLE {
                continue;
            }
            let energy = unsafe { (api.integer_value)(raw, 0) };
            if energy <= 0 {
                continue;
            }

            // Convert the counter's own unit to joules rather than assuming
            // millijoules — the label differs across SoC generations.
            let joules = match unit.trim() {
                "mJ" => energy as f64 / 1.0e3,
                "uJ" | "µJ" => energy as f64 / 1.0e6,
                "nJ" => energy as f64 / 1.0e9,
                // Unknown unit: refuse to guess rather than publish a number
                // that is wrong by three orders of magnitude.
                _ => continue,
            };

            let watts = joules / interval;
            // Sanity bound: a whole Apple Silicon package is tens of watts.
            if !watts.is_finite() || !(0.0..=1000.0).contains(&watts) {
                continue;
            }

            // The Energy Model group publishes ~80 channels, most of them
            // per-core detail rails. Showing all of them buries the handful
            // anyone reads.
            if !is_headline_rail(&name) {
                continue;
            }

            // Identify by a slug of the channel name rather than its position:
            // the channel set differs per SoC and ordering is not guaranteed
            // stable, but graph history and CSV columns key off this string.
            out.push(sensor(
                &format!("/{}/power/{}", rail_of(&name).node(), slug(&name)),
                &friendly_name(&name),
                SensorType::Power,
                index as u32,
                watts as f32,
            ));
        }
        out
    }
}

/// Which hardware node a power rail belongs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rail {
    Soc,
    Gpu,
    Memory,
}

impl Rail {
    fn node(self) -> &'static str {
        match self {
            Rail::Soc => "applesoc/0",
            Rail::Gpu => "gpu/0",
            Rail::Memory => "ram/0",
        }
    }
}

/// Route a channel to the node it physically belongs to, so GPU power appears
/// under the GPU and memory power under memory rather than all of it landing
/// in one undifferentiated CPU node.
pub fn rail_of(channel: &str) -> Rail {
    let name = channel.trim();
    if name.starts_with("GPU") {
        return Rail::Gpu;
    }
    // AMCC and DCS are the memory controller and DRAM command/storage rails.
    if name.starts_with("DRAM") || name.starts_with("AMCC") || name.starts_with("DCS") {
        return Rail::Memory;
    }
    Rail::Soc
}

/// Keep only aggregate rails, dropping the per-core detail channels.
///
/// Dropped: `*DTL*` (per-core detail), `*_SRAM` (per-core cache rails), and
/// numbered per-core rails like `ECPU3` / `PCPU1`. Kept: cluster totals
/// (`ECPU`, `PCPU`), the package total, and the non-CPU blocks (GPU, ANE,
/// DRAM, DISP, ISP, ...).
pub fn is_headline_rail(channel: &str) -> bool {
    let name = channel.trim();
    if name.is_empty() || name.contains("DTL") || name.ends_with("_SRAM") {
        return false;
    }
    // ECPU0, PCPU12 — a cluster prefix followed only by digits.
    for prefix in ["ECPU", "PCPU", "ECPM", "PCPM"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
        }
    }
    true
}

/// Stable, filesystem-ish identifier fragment for a channel name.
fn slug(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect()
}

/// Firmware channel names are terse ("ANE", "GPU Energy"); make them read like
/// the rest of the UI without hiding which channel they came from.
fn friendly_name(raw: &str) -> String {
    let trimmed = raw.trim();
    match trimmed {
        "CPU Energy" => "CPU Package Power".to_string(),
        "GPU Energy" => "GPU Power".to_string(),
        "ANE" | "ANE Energy" => "Neural Engine Power".to_string(),
        other => {
            // "DRAM Energy" -> "DRAM Power": the sensor reports a rate.
            if let Some(stem) = other.strip_suffix(" Energy") {
                format!("{stem} Power")
            } else {
                other.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_model_subscription_succeeds() {
        let reporter = EnergyReporter::new();
        if !reporter.available() {
            crate::source::macos::absent("libIOReport Energy Model");
        }
    }

    #[test]
    fn power_needs_a_baseline_then_reports_watts() {
        let mut reporter = EnergyReporter::new();
        // Holds whether or not IOReport is present: no baseline means no rate.
        assert!(
            reporter.power_sensors().is_empty(),
            "cumulative counters must not be published as power on the first poll"
        );
        if !reporter.available() {
            return crate::source::macos::absent("libIOReport Energy Model");
        }

        std::thread::sleep(std::time::Duration::from_millis(250));
        let sensors = reporter.power_sensors();
        if sensors.is_empty() {
            return crate::source::macos::absent("Energy Model power channels");
        }

        for s in &sensors {
            let w = s.value.unwrap();
            assert!(w.is_finite(), "{} is not finite", s.name);
            // A fanless laptop SoC does not draw 200 W; catching a unit error
            // (mJ read as J) is the whole point of this bound.
            assert!((0.0..200.0).contains(&w), "{} = {w} W implausible", s.name);
        }
    }

    #[test]
    fn energy_suffix_is_rewritten_to_power() {
        assert_eq!(friendly_name("CPU Energy"), "CPU Package Power");
        assert_eq!(friendly_name("DRAM Energy"), "DRAM Power");
        assert_eq!(friendly_name("ANE"), "Neural Engine Power");
        assert_eq!(friendly_name("Something Else"), "Something Else");
    }
}
