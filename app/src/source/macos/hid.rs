//! Die temperatures via the `IOHIDEventSystemClient` sensor plane.
//!
//! This is how Apple Silicon exposes thermals. There is no `AppleSMC` service
//! on M-series machines (verified: `ioreg -n AppleSMC` returns nothing on M5),
//! so the SMC key protocol that works on Intel Macs is not an option here.
//!
//! Sensors live as HID services on Apple's vendor usage page (0xFF00) and are
//! read by asking each service for a temperature event. Their names are
//! firmware strings like `"NAND CH0 temp"` or `"PMU tdev1"` and differ between
//! chip generations, so nothing here may hardcode a specific sensor name.
//!
//! All symbols are resolved at runtime (see `dynlib`) and every failure path
//! yields zero sensors rather than a panic.

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFAllocatorRef, CFRelease, CFTypeRef};
use core_foundation_sys::string::CFStringRef;

use crate::model::{Sensor, SensorType};

use super::dynlib;
use super::sensor;

/// `kHIDPage_AppleVendor`.
const APPLE_VENDOR_USAGE_PAGE: i32 = 0xff00;
/// `kHIDUsage_AppleVendor_TemperatureSensor`.
const APPLE_USAGE_TEMPERATURE: i32 = 5;
/// `kIOHIDEventTypeTemperature`.
const EVENT_TYPE_TEMPERATURE: i64 = 15;
/// Event fields are `(type << 16) | offset`; offset 0 is the level itself.
const FIELD_TEMPERATURE_LEVEL: i32 = (EVENT_TYPE_TEMPERATURE as i32) << 16;

// C signatures, kept beside the aliases because getting these wrong is UB:
//   IOHIDEventSystemClientRef IOHIDEventSystemClientCreate(CFAllocatorRef);
//   void IOHIDEventSystemClientSetMatching(IOHIDEventSystemClientRef, CFDictionaryRef);
//   CFArrayRef IOHIDEventSystemClientCopyServices(IOHIDEventSystemClientRef);
//   CFTypeRef IOHIDServiceClientCopyProperty(IOHIDServiceClientRef, CFStringRef);
//   IOHIDEventRef IOHIDServiceClientCopyEvent(IOHIDServiceClientRef, int64_t, int32_t, int64_t);
//   double IOHIDEventGetFloatValue(IOHIDEventRef, int32_t);
type ClientCreate = unsafe extern "C" fn(CFAllocatorRef) -> CFTypeRef;
type ClientSetMatching = unsafe extern "C" fn(CFTypeRef, CFTypeRef);
type ClientCopyServices = unsafe extern "C" fn(CFTypeRef) -> CFTypeRef;
type ServiceCopyProperty = unsafe extern "C" fn(CFTypeRef, CFStringRef) -> CFTypeRef;
type ServiceCopyEvent = unsafe extern "C" fn(CFTypeRef, i64, i32, i64) -> CFTypeRef;
type EventGetFloatValue = unsafe extern "C" fn(CFTypeRef, i32) -> f64;

struct Api {
    copy_services: ClientCopyServices,
    copy_property: ServiceCopyProperty,
    copy_event: ServiceCopyEvent,
    event_float: EventGetFloatValue,
}

pub struct HidSensors {
    api: Option<Api>,
    /// The HID client, retained for the collector's lifetime. Creating one per
    /// poll leaks kernel ports and is measurably slow.
    client: CFTypeRef,
    /// Sensor names discovered at construction, in service order.
    names: Vec<String>,
}

// SAFETY: `client` is a CoreFoundation object owned solely by this struct. The
// collector is built on the main thread and immediately moved to the polling
// thread, then only ever touched from there — CF objects may be used from any
// one thread at a time, and no aliasing handle is kept anywhere.
unsafe impl Send for HidSensors {}

impl Drop for HidSensors {
    fn drop(&mut self) {
        if !self.client.is_null() {
            unsafe { CFRelease(self.client) };
        }
    }
}

impl HidSensors {
    pub fn new() -> Self {
        let mut this =
            Self { api: None, client: std::ptr::null(), names: Vec::new() };

        // IOKit.framework is already linked into the process, so the private
        // IOHID* symbols are reachable via RTLD_DEFAULT without dlopen.
        let (create, set_matching, copy_services, copy_property, copy_event, event_float) = unsafe {
            (
                dynlib::global_symbol::<ClientCreate>("IOHIDEventSystemClientCreate"),
                dynlib::global_symbol::<ClientSetMatching>("IOHIDEventSystemClientSetMatching"),
                dynlib::global_symbol::<ClientCopyServices>("IOHIDEventSystemClientCopyServices"),
                dynlib::global_symbol::<ServiceCopyProperty>("IOHIDServiceClientCopyProperty"),
                dynlib::global_symbol::<ServiceCopyEvent>("IOHIDServiceClientCopyEvent"),
                dynlib::global_symbol::<EventGetFloatValue>("IOHIDEventGetFloatValue"),
            )
        };
        // Any missing symbol means this macOS doesn't expose the sensor plane
        // the way we expect; report no sensors instead of guessing.
        let (Some(create), Some(set_matching), Some(copy_services), Some(copy_property), Some(copy_event), Some(event_float)) =
            (create, set_matching, copy_services, copy_property, copy_event, event_float)
        else {
            return this;
        };

        let client = unsafe { create(std::ptr::null()) };
        if client.is_null() {
            return this;
        }

        // Restrict to Apple-vendor temperature sensors.
        let matching = CFDictionary::from_CFType_pairs(&[
            (
                CFString::new("PrimaryUsagePage").as_CFType(),
                CFNumber::from(APPLE_VENDOR_USAGE_PAGE).as_CFType(),
            ),
            (
                CFString::new("PrimaryUsage").as_CFType(),
                CFNumber::from(APPLE_USAGE_TEMPERATURE).as_CFType(),
            ),
        ]);
        unsafe { set_matching(client, matching.as_CFTypeRef()) };

        this.client = client;
        this.api = Some(Api { copy_services, copy_property, copy_event, event_float });
        this.names = this.service_names();
        this
    }

    /// The matched services, as a CF array. Caller owns the array.
    fn services(&self) -> Option<CFArray<CFType>> {
        let api = self.api.as_ref()?;
        if self.client.is_null() {
            return None;
        }
        let array = unsafe { (api.copy_services)(self.client) };
        if array.is_null() {
            return None;
        }
        Some(unsafe { CFArray::<CFType>::wrap_under_create_rule(array.cast()) })
    }

    fn service_names(&self) -> Vec<String> {
        let Some(api) = self.api.as_ref() else {
            return Vec::new();
        };
        let Some(services) = self.services() else {
            return Vec::new();
        };
        let key = CFString::new("Product");
        services
            .iter()
            .enumerate()
            .map(|(i, service)| {
                let raw =
                    unsafe { (api.copy_property)(service.as_CFTypeRef(), key.as_concrete_TypeRef()) };
                if raw.is_null() {
                    return format!("Sensor {i}");
                }
                let value = unsafe { CFType::wrap_under_create_rule(raw) };
                value
                    .downcast::<CFString>()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("Sensor {i}"))
            })
            .collect()
    }

    /// True when the sensor plane resolved and matched at least one service.
    pub fn available(&self) -> bool {
        self.api.is_some() && !self.names.is_empty()
    }

    pub fn sensor_count(&self) -> usize {
        self.names.len()
    }

    /// One `Temperature` sensor per responding service.
    pub fn temperatures(&self) -> Vec<Sensor> {
        let Some(api) = self.api.as_ref() else {
            return Vec::new();
        };
        let Some(services) = self.services() else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for (index, service) in services.iter().enumerate() {
            let event =
                unsafe { (api.copy_event)(service.as_CFTypeRef(), EVENT_TYPE_TEMPERATURE, 0, 0) };
            if event.is_null() {
                // Sensors go quiet when the subsystem they measure is powered
                // down; that's normal, so skip rather than reporting 0 °C.
                continue;
            }
            let celsius = unsafe { (api.event_float)(event, FIELD_TEMPERATURE_LEVEL) };
            unsafe { CFRelease(event) };

            // Reject obvious garbage: a powered-down or misparsed sensor
            // reports 0 or a wild value, and a fake 0 °C reading in the UI is
            // worse than an absent sensor.
            if !celsius.is_finite() || !(1.0..=150.0).contains(&celsius) {
                continue;
            }

            let name = self.names.get(index).cloned().unwrap_or_else(|| format!("Sensor {index}"));
            out.push(sensor(
                &format!("/applesoc/0/temperature/{index}"),
                &name,
                SensorType::Temperature,
                index as u32,
                celsius as f32,
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensor_plane_resolves_on_apple_silicon() {
        let hid = HidSensors::new();
        if !hid.available() {
            return crate::source::macos::absent("IOHIDEventSystem sensor plane");
        }
        assert!(hid.sensor_count() > 0);
    }

    #[test]
    fn temperatures_are_plausible_and_named() {
        let hid = HidSensors::new();
        let temps = hid.temperatures();
        if temps.is_empty() {
            return crate::source::macos::absent("live temperature sensors");
        }
        for s in &temps {
            let v = s.value.unwrap();
            assert!((1.0..=150.0).contains(&v), "{} = {v} °C out of range", s.name);
            assert!(!s.name.is_empty());
        }
    }

    /// Identifiers key history graphs and CSV columns, so they must be stable
    /// and unique across polls.
    #[test]
    fn identifiers_are_unique_and_stable() {
        let hid = HidSensors::new();
        let first = hid.temperatures();
        if first.is_empty() {
            return crate::source::macos::absent("live temperature sensors");
        }
        let ids: std::collections::HashSet<_> = first.iter().map(|s| &s.identifier).collect();
        assert_eq!(ids.len(), first.len(), "duplicate sensor identifiers");

        let second = hid.temperatures();
        let second_ids: std::collections::HashSet<_> =
            second.iter().map(|s| &s.identifier).collect();
        assert_eq!(ids, second_ids, "identifiers must not change between polls");
    }
}
