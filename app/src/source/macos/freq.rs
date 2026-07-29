//! Effective CPU and GPU clocks, reconstructed from DVFS residency.
//!
//! Apple Silicon exposes no "current MHz" register. IOReport's `CPU Stats` /
//! `GPU Stats` groups instead report, per block, how many ticks were spent in
//! each performance state since boot. Differencing two samples gives the time
//! spent in each state during the interval, and weighting the state table from
//! [`super::dvfs`] by that residency gives the average clock over the interval
//! — the same quantity `powermetrics` reports.
//!
//! Idle residency is excluded: including it would report ~0 MHz for a mostly
//! idle core, whereas every other monitor reports the clock the core runs at
//! *while running*. When a block was fully idle across the interval it reports
//! no sensor at all rather than a misleading zero.

use std::time::Instant;

use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFRelease, CFTypeRef};
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::string::CFStringRef;

use crate::model::{Sensor, SensorType};

use super::dvfs::{self, Block, State};
use super::dynlib::Library;
use super::sensor;

// SPI signatures, as in `ioreport.rs`:
//   int32_t     IOReportStateGetCount(CFDictionaryRef);
//   CFStringRef IOReportStateGetNameForIndex(CFDictionaryRef, int32_t);
//   int64_t     IOReportStateGetResidency(CFDictionaryRef, int32_t);
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
type ChannelGetSubGroup = unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef;
type ChannelGetFormat = unsafe extern "C" fn(CFDictionaryRef) -> i32;
type StateGetCount = unsafe extern "C" fn(CFDictionaryRef) -> i32;
type StateGetNameForIndex = unsafe extern "C" fn(CFDictionaryRef, i32) -> CFStringRef;
type StateGetResidency = unsafe extern "C" fn(CFDictionaryRef, i32) -> i64;

/// `kIOReportFormatState` — residency counters per performance state.
const FORMAT_STATE: i32 = 2;

struct Api {
    _library: Library,
    create_samples: CreateSamples,
    create_samples_delta: CreateSamplesDelta,
    channel_name: ChannelGetName,
    channel_subgroup: ChannelGetSubGroup,
    channel_format: ChannelGetFormat,
    state_count: StateGetCount,
    state_name: StateGetNameForIndex,
    state_residency: StateGetResidency,
}

/// One IOReport subscription. `CPU Stats` and `GPU Stats` are separate groups
/// and cannot be subscribed to through a single call, so each gets its own —
/// merging them would silently drop whichever came second, which is exactly
/// the bug that made GPU clocks never appear.
struct Group {
    subscription: CFTypeRef,
    channels: CFDictionaryRef,
    prev: Option<(CFDictionaryRef, Instant)>,
}

impl Drop for Group {
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

pub struct FrequencyReporter {
    api: Option<Api>,
    groups: Vec<Group>,
    ecpu: Vec<State>,
    pcpu: Vec<State>,
    gpu: Vec<State>,
}

// SAFETY: as `ioreport::EnergyReporter` — the CF handles are owned solely by
// this struct, which is moved once onto the poll thread and used only there.
unsafe impl Send for FrequencyReporter {}

impl FrequencyReporter {
    pub fn new() -> Self {
        let mut this = Self {
            api: None,
            groups: Vec::new(),
            ecpu: dvfs::states(Block::Ecpu),
            pcpu: dvfs::states(Block::Pcpu),
            gpu: dvfs::states(Block::Gpu),
        };

        let Some(library) = Library::open("/usr/lib/libIOReport.dylib") else {
            return this;
        };
        let syms = unsafe {
            (
                library.symbol::<CopyChannelsInGroup>("IOReportCopyChannelsInGroup"),
                library.symbol::<CreateSubscription>("IOReportCreateSubscription"),
                library.symbol::<CreateSamples>("IOReportCreateSamples"),
                library.symbol::<CreateSamplesDelta>("IOReportCreateSamplesDelta"),
                library.symbol::<ChannelGetName>("IOReportChannelGetChannelName"),
                library.symbol::<ChannelGetSubGroup>("IOReportChannelGetSubGroup"),
                library.symbol::<ChannelGetFormat>("IOReportChannelGetFormat"),
                library.symbol::<StateGetCount>("IOReportStateGetCount"),
                library.symbol::<StateGetNameForIndex>("IOReportStateGetNameForIndex"),
                library.symbol::<StateGetResidency>("IOReportStateGetResidency"),
            )
        };
        let (
            Some(copy_channels),
            Some(create_subscription),
            Some(create_samples),
            Some(create_samples_delta),
            Some(channel_name),
            Some(channel_subgroup),
            Some(channel_format),
            Some(state_count),
            Some(state_name),
            Some(state_residency),
        ) = syms
        else {
            return this;
        };

        // One subscription per group. The subgroup filter is left null so a
        // renamed subgroup can't silently drop everything.
        for group in ["CPU Stats", "GPU Stats"] {
            let cf = CFString::new(group);
            let desired =
                unsafe { copy_channels(cf.as_concrete_TypeRef(), std::ptr::null(), 0, 0, 0) };
            if desired.is_null() {
                continue;
            }
            let mut subscribed: CFDictionaryRef = std::ptr::null();
            let subscription = unsafe {
                create_subscription(std::ptr::null(), desired, &mut subscribed, 0, std::ptr::null())
            };
            unsafe { CFRelease(desired.cast()) };

            if subscription.is_null() || subscribed.is_null() {
                if !subscription.is_null() {
                    unsafe { CFRelease(subscription) };
                }
                continue;
            }
            this.groups.push(Group { subscription, channels: subscribed, prev: None });
        }
        if this.groups.is_empty() {
            return this;
        }

        this.api = Some(Api {
            _library: library,
            create_samples,
            create_samples_delta,
            channel_name,
            channel_subgroup,
            channel_format,
            state_count,
            state_name,
            state_residency,
        });
        this
    }

    /// True when the subscription exists *and* at least one DVFS table was
    /// readable — without the table, residency can't be turned into MHz.
    pub fn available(&self) -> bool {
        self.api.is_some()
            && !self.groups.is_empty()
            && !(self.ecpu.is_empty() && self.pcpu.is_empty() && self.gpu.is_empty())
    }

    /// Clock sensors in MHz. Empty on the first call (no baseline).
    pub fn clock_sensors(&mut self) -> Vec<Sensor> {
        let Some(api) = self.api.as_ref() else {
            return Vec::new();
        };

        // Collect the per-group deltas first so the borrow of `self.groups`
        // ends before `read_delta` needs `&self` for the DVFS tables.
        let mut deltas = Vec::new();
        for group in &mut self.groups {
            let now =
                unsafe { (api.create_samples)(group.subscription, group.channels, std::ptr::null()) };
            if now.is_null() {
                continue;
            }
            let taken_at = Instant::now();

            let Some((prev, _)) = group.prev.take() else {
                group.prev = Some((now, taken_at));
                continue;
            };
            let delta = unsafe { (api.create_samples_delta)(prev, now, std::ptr::null()) };
            unsafe { CFRelease(prev.cast()) };
            group.prev = Some((now, taken_at));

            if !delta.is_null() {
                deltas.push(delta);
            }
        }

        let mut out = Vec::new();
        for delta in deltas {
            out.extend(self.read_delta(api, delta, &out));
            unsafe { CFRelease(delta.cast()) };
        }
        out
    }

    fn read_delta(&self, api: &Api, delta: CFDictionaryRef, already: &[Sensor]) -> Vec<Sensor> {
        let dict: CFDictionary<CFString, CFType> =
            unsafe { CFDictionary::wrap_under_get_rule(delta) };
        let Some(channels) =
            dict.find(CFString::new("IOReportChannels")).and_then(|v| super::iokit::as_array(&v))
        else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for channel in channels.iter() {
            let raw = channel.as_CFTypeRef() as CFDictionaryRef;
            if unsafe { (api.channel_format)(raw) } != FORMAT_STATE {
                continue;
            }

            let name = cf_string(unsafe { (api.channel_name)(raw) });
            let subgroup = cf_string(unsafe { (api.channel_subgroup)(raw) });

            // Match the channel to a DVFS table. Names are firmware strings
            // ("ECPU", "PCPU0", "GPUPH"), so match on prefix rather than
            // equality, and check the subgroup too because both groups use
            // similar channel names.
            //
            // The names deliberately contain "Core": the System Summary picks
            // CPU clocks out of the tree by matching that substring (so it can
            // exclude things like bus speed), and the GPU panel does the same.
            // Renaming these to "E-Cluster"/"P-Cluster" would silently blank
            // the Max Clock and Avg. Active Clock rows.
            let context = format!("{subgroup} {name}");
            let (table, label, id) = if context.contains("GPU") {
                (&self.gpu, "GPU Core".to_string(), "gpu".to_string())
            } else if name.starts_with("ECPU") {
                (&self.ecpu, "E-Core Clock".to_string(), "ecpu".to_string())
            } else if name.starts_with("PCPU") {
                (&self.pcpu, "P-Core Clock".to_string(), "pcpu".to_string())
            } else {
                continue;
            };
            if table.is_empty() {
                continue;
            }

            let Some((mhz, volts)) = weighted_state(api, raw, table) else {
                continue;
            };

            // A cluster reports several channels (per-core and aggregate);
            // keep the first per block so the UI shows one clock per cluster.
            let identifier = format!("/applesoc/0/clock/{id}");
            let seen = |s: &Sensor| s.identifier == identifier;
            if out.iter().any(seen) || already.iter().any(seen) {
                continue;
            }
            let index = (already.len() + out.len()) as u32;
            out.push(sensor(&identifier, &label, SensorType::Clock, index, mhz));

            // The DVFS table pairs every state with its rail voltage, so the
            // same residency weighting yields a real VID — Apple Silicon has no
            // separate voltage sensor. Skip it for the GPU, where the Summary
            // has no VID column.
            if id != "gpu" {
                out.push(sensor(
                    &format!("/applesoc/0/voltage/{id}"),
                    // "E-Core Clock" -> "E-Core VID"; the Summary matches
                    // CPU voltages on the substring "vid".
                    &format!("{} VID", label.trim_end_matches(" Clock")),
                    SensorType::Voltage,
                    index,
                    volts,
                ));
            }
        }
        out
    }
}

/// Residency-weighted average frequency, excluding idle states.
///
/// When the block was idle for the *whole* interval there is no weighted
/// average to take. It reports the lowest running state rather than `None`,
/// for two reasons: that is the clock the block runs at when it next wakes
/// (and the value this function already returns for a barely-active block),
/// and returning `None` would drop the sensor from the tree entirely — making
/// the row flicker out of the Sensors table and the Summary whenever the GPU
/// or a cluster went quiet.
fn weighted_state(api: &Api, channel: CFDictionaryRef, table: &[State]) -> Option<(f32, f32)> {
    let count = unsafe { (api.state_count)(channel) };
    if count <= 0 {
        return None;
    }

    let mut weighted = 0.0f64;
    let mut weighted_v = 0.0f64;
    let mut active = 0.0f64;

    // Whether the DVFS table itself carries an entry for the idle state
    // decides how residency indices line up with it, and the SoC is not
    // consistent about this: the GPU table (`voltage-states9`) starts with a
    // literal 0 MHz idle entry, while the CPU tables (`voltage-states*-sram`)
    // start directly at the lowest running state. Getting this wrong maps
    // every GPU state one slot low and reports a flat 0 MHz.
    let table_includes_idle = table.first().is_some_and(|s| s.mhz == 0.0);
    let offset = if table_includes_idle { 0 } else { 1 };

    // State index 0 is idle/off on every block; the remaining indices line up
    // with the DVFS table in order.
    for index in 0..count {
        let residency = unsafe { (api.state_residency)(channel, index) };
        if residency <= 0 {
            continue;
        }
        let state = cf_string(unsafe { (api.state_name)(channel, index) });
        // Belt and braces: skip anything the firmware names as idle/off even
        // if it isn't at index 0.
        let idle = index == 0
            || state.eq_ignore_ascii_case("IDLE")
            || state.eq_ignore_ascii_case("OFF")
            || state.eq_ignore_ascii_case("DOWN");
        if idle {
            continue;
        }
        let Some(state) = table.get((index - offset) as usize) else {
            continue;
        };
        // A 0 MHz entry is an idle slot that slipped through; counting it would
        // drag the average toward zero.
        if state.mhz <= 0.0 {
            continue;
        }
        weighted += state.mhz as f64 * residency as f64;
        weighted_v += state.volts as f64 * residency as f64;
        active += residency as f64;
    }

    if active <= 0.0 {
        // Fully idle: fall back to the lowest running state.
        return table.iter().find(|s| s.mhz > 0.0).map(|s| (s.mhz, s.volts));
    }
    let mhz = (weighted / active) as f32;
    let volts = (weighted_v / active) as f32;
    (mhz.is_finite() && volts.is_finite()).then_some((mhz, volts))
}

fn cf_string(raw: CFStringRef) -> String {
    if raw.is_null() {
        return String::new();
    }
    unsafe { CFString::wrap_under_get_rule(raw) }.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks_need_a_baseline_then_report_mhz() {
        let mut reporter = FrequencyReporter::new();
        assert!(
            reporter.clock_sensors().is_empty(),
            "residency counters must not be published as a clock on the first poll"
        );
        if !reporter.available() {
            return crate::source::macos::absent("IOReport CPU/GPU Stats");
        }

        // Give the clusters something to do so they leave idle, otherwise a
        // quiet machine legitimately reports no active residency.
        let spinner = std::thread::spawn(|| {
            let end = Instant::now() + std::time::Duration::from_millis(400);
            let mut x = 0u64;
            while Instant::now() < end {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            }
            x
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        let sensors = reporter.clock_sensors();
        let _ = spinner.join();

        if sensors.is_empty() {
            return crate::source::macos::absent("active DVFS residency");
        }
        // Each cluster contributes a clock and, for the CPU, a VID derived
        // from the same weighting.
        for s in &sensors {
            let v = s.value.unwrap();
            match s.sensor_type {
                // Anything outside this means the state table and the residency
                // indices are misaligned, or the unit scale is wrong.
                SensorType::Clock => assert!(
                    (100.0..=6000.0).contains(&v),
                    "{} = {v} MHz is not a plausible Apple Silicon clock",
                    s.name
                ),
                SensorType::Voltage => assert!(
                    (0.3..=1.5).contains(&v),
                    "{} = {v} V is not a plausible core voltage",
                    s.name
                ),
                other => panic!("unexpected sensor type {other:?} from the clock reporter"),
            }
        }
        assert!(sensors.iter().any(|s| s.sensor_type == SensorType::Clock));
    }

    #[test]
    fn one_clock_sensor_per_block_at_most() {
        let mut reporter = FrequencyReporter::new();
        let _ = reporter.clock_sensors();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let sensors = reporter.clock_sensors();

        let ids: std::collections::HashSet<_> = sensors.iter().map(|s| &s.identifier).collect();
        assert_eq!(ids.len(), sensors.len(), "duplicate clock identifiers");
        let clocks = sensors.iter().filter(|s| s.sensor_type == SensorType::Clock).count();
        assert!(clocks <= 3, "expected at most E-cluster, P-cluster and GPU clocks");
        // VID only for the two CPU clusters, never the GPU.
        let vids = sensors.iter().filter(|s| s.sensor_type == SensorType::Voltage).count();
        assert!(vids <= 2, "expected at most E-cluster and P-cluster VID");
    }
}
