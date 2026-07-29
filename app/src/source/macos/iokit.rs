//! Minimal IOKit FFI + CoreFoundation extraction helpers.
//!
//! IOKit is declared by hand (rather than pulled in as a crate) to match the
//! `extern` blocks already used for advapi32/kernel32 in `sysinfo.rs` and
//! `firmware.rs`. CoreFoundation is *not* hand-rolled: IOKit hands back nested
//! CFDictionary/CFArray/CFNumber trees, and getting retain/release right around
//! those by hand is where that idiom stops paying for itself.
//!
//! Everything here is fallible-by-default. A service class that doesn't exist,
//! a property that was renamed between macOS releases, or a type that isn't
//! what we expected must all produce `None` — never a panic. The release
//! profile is `panic = "abort"`, so a single bad `unwrap` here takes down the
//! whole app rather than greying out one sensor.

#![allow(non_upper_case_globals, non_camel_case_types)]

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::{kCFAllocatorDefault, CFAllocatorRef, CFTypeRef};
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::string::CFStringRef;

pub type io_object_t = u32;
pub type io_iterator_t = io_object_t;
pub type io_registry_entry_t = io_object_t;
pub type kern_return_t = i32;
pub type mach_port_t = u32;

pub const KERN_SUCCESS: kern_return_t = 0;
/// `kIOMainPortDefault` is `MACH_PORT_NULL`; passing 0 selects the default
/// port on every supported macOS without needing the renamed-in-12.0 symbol
/// (`kIOMasterPortDefault` → `kIOMainPortDefault`).
pub const MAIN_PORT_DEFAULT: mach_port_t = 0;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const libc::c_char) -> CFDictionaryRef;
    fn IOServiceGetMatchingServices(
        main_port: mach_port_t,
        matching: CFDictionaryRef,
        existing: *mut io_iterator_t,
    ) -> kern_return_t;
    fn IOIteratorNext(iterator: io_iterator_t) -> io_object_t;
    fn IOObjectRelease(object: io_object_t) -> kern_return_t;
    fn IORegistryEntryCreateCFProperties(
        entry: io_registry_entry_t,
        properties: *mut CFDictionaryRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> kern_return_t;
    fn IORegistryEntryGetName(entry: io_registry_entry_t, name: *mut libc::c_char) -> kern_return_t;
    fn IOObjectGetClass(object: io_object_t, class_name: *mut libc::c_char) -> kern_return_t;
    fn IORegistryEntryFromPath(
        main_port: mach_port_t,
        path: *const libc::c_char,
    ) -> io_registry_entry_t;
    fn IORegistryEntrySearchCFProperty(
        entry: io_registry_entry_t,
        plane: *const libc::c_char,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> CFTypeRef;
}

/// Search options for [`search_property`]: walk toward the registry root.
pub const kIORegistryIterateRecursively: u32 = 0x0000_0001;
pub const kIORegistryIterateParents: u32 = 0x0000_0002;

/// An owned IOKit object handle. Exists so every early return releases the
/// port — leaking `io_object_t`s in a function polled once a second is a slow
/// resource leak that only shows up after hours of running.
pub struct IoObject(pub io_object_t);

impl Drop for IoObject {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { IOObjectRelease(self.0) };
        }
    }
}

/// Every service matching an IOKit class name, e.g. `"AppleSmartBattery"`.
///
/// Returns an empty vec if the class does not exist on this machine — which is
/// the normal case for plenty of classes (no fans on a MacBook Air, no
/// `AppleSMC` at all on M-series).
pub fn matching_services(class: &str) -> Vec<IoObject> {
    let Ok(cname) = std::ffi::CString::new(class) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    unsafe {
        let matching = IOServiceMatching(cname.as_ptr());
        if matching.is_null() {
            return out;
        }
        let mut iter: io_iterator_t = 0;
        // NB: IOServiceGetMatchingServices consumes a reference to `matching`,
        // so it must not be released here even on the error path.
        if IOServiceGetMatchingServices(MAIN_PORT_DEFAULT, matching, &mut iter) != KERN_SUCCESS {
            return out;
        }
        let _iter_guard = IoObject(iter);
        loop {
            let next = IOIteratorNext(iter);
            if next == 0 {
                break;
            }
            out.push(IoObject(next));
        }
    }
    out
}

/// Snapshot an entry's whole property dictionary.
pub fn properties(entry: io_registry_entry_t) -> Option<CFDictionary<CFString, CFType>> {
    unsafe {
        let mut props: CFDictionaryRef = std::ptr::null();
        if IORegistryEntryCreateCFProperties(entry, &mut props, kCFAllocatorDefault, 0)
            != KERN_SUCCESS
            || props.is_null()
        {
            return None;
        }
        Some(CFDictionary::wrap_under_create_rule(props))
    }
}

/// The object's **exact** IOKit class name.
///
/// Needed because `IOServiceMatching` also matches subclasses: asking for
/// `IOMedia` additionally returns every `AppleAPFSMedia` synthesized volume,
/// which is indistinguishable by properties alone (they too report
/// `"Whole" = Yes`). Comparing the concrete class separates real disks from
/// APFS containers.
pub fn object_class(object: io_object_t) -> Option<String> {
    // io_name_t is a fixed char[128] out-parameter.
    let mut buf = [0i8; 128];
    unsafe {
        if IOObjectGetClass(object, buf.as_mut_ptr()) != KERN_SUCCESS {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

/// The registry entry's name (e.g. the `+-o <name>` shown by `ioreg`).
#[allow(dead_code)] // Kept alongside object_class for registry debugging.
pub fn entry_name(entry: io_registry_entry_t) -> Option<String> {
    // io_name_t is a fixed char[128] out-parameter.
    let mut buf = [0i8; 128];
    unsafe {
        if IORegistryEntryGetName(entry, buf.as_mut_ptr()) != KERN_SUCCESS {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

/// Resolve a registry entry by path, e.g.
/// `"IODeviceTree:/arm-io/pmgr"` for the power-manager node that carries the
/// DVFS (`voltage-states*`) tables. Those live in the device-tree plane and
/// are not an `IOService`, so `matching_services` cannot reach them.
pub fn entry_from_path(path: &str) -> Option<IoObject> {
    let cpath = std::ffi::CString::new(path).ok()?;
    let entry = unsafe { IORegistryEntryFromPath(MAIN_PORT_DEFAULT, cpath.as_ptr()) };
    (entry != 0).then_some(IoObject(entry))
}

/// A raw `CFData` property, e.g. a packed `voltage-states` table.
pub fn dict_data(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<Vec<u8>> {
    let value = dict_get(dict, key)?;
    let data = value.downcast::<core_foundation::data::CFData>()?;
    Some(data.bytes().to_vec())
}

/// Look up a property on an entry *or its ancestors*. Needed because the
/// interesting properties often sit on a parent of the service that matched
/// (e.g. NVMe model/serial live above the block-storage driver).
pub fn search_property(entry: io_registry_entry_t, key: &str) -> Option<CFType> {
    let cf_key = CFString::new(key);
    let plane = std::ffi::CString::new("IOService").ok()?;
    unsafe {
        let value = IORegistryEntrySearchCFProperty(
            entry,
            plane.as_ptr(),
            cf_key.as_concrete_TypeRef(),
            kCFAllocatorDefault,
            kIORegistryIterateRecursively | kIORegistryIterateParents,
        );
        if value.is_null() {
            return None;
        }
        Some(CFType::wrap_under_create_rule(value))
    }
}

// ---- CoreFoundation extraction ------------------------------------------
//
// All of these take `&CFDictionary<CFString, CFType>` and a plain &str key so
// call sites read like dictionary lookups rather than FFI.

pub fn dict_get(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<CFType> {
    dict.find(CFString::new(key)).map(|v| v.clone())
}

/// Read a numeric property as a **signed** 64-bit value.
///
/// This is the correct default for IOKit. `AppleSmartBattery` in particular
/// stores negative values (discharge current, `ISS`) as two's-complement in an
/// unsigned slot — `ioreg` shows `"Amperage" = 18446744073709551264`, which is
/// -352. Reading those as unsigned yields ~1.8e19 mA.
pub fn dict_i64(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<i64> {
    let value = dict_get(dict, key)?;
    let number = value.downcast::<CFNumber>()?;
    number.to_i64().or_else(|| number.to_f64().map(|f| f as i64))
}

pub fn dict_f64(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<f64> {
    let value = dict_get(dict, key)?;
    let number = value.downcast::<CFNumber>()?;
    number.to_f64().or_else(|| number.to_i64().map(|i| i as f64))
}

pub fn dict_string(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<String> {
    let value = dict_get(dict, key)?;
    // Some IOKit "string" properties are actually CFData holding raw bytes
    // (e.g. the device-tree `model`), so fall back to a lossy byte decode.
    if let Some(s) = value.downcast::<CFString>() {
        return Some(s.to_string());
    }
    let data = value.downcast::<core_foundation::data::CFData>()?;
    let bytes: &[u8] = data.bytes();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
    (!s.is_empty()).then_some(s)
}

pub fn dict_bool(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<bool> {
    let value = dict_get(dict, key)?;
    if let Some(b) = value.downcast::<core_foundation::boolean::CFBoolean>() {
        return Some(b.into());
    }
    dict_i64(dict, key).map(|v| v != 0)
}

/// Reinterpret a `CFType` as a string-keyed dictionary.
///
/// `downcast` only works for the untyped `CFDictionary` (it is the one that
/// implements `ConcreteCFType`), so the type check happens there and the
/// key/value types are re-applied afterwards. Safe because CoreFoundation
/// dictionaries from IOKit are always `CFString`-keyed.
pub fn as_dict(value: &CFType) -> Option<CFDictionary<CFString, CFType>> {
    let untyped = value.downcast::<CFDictionary>()?;
    Some(unsafe { CFDictionary::wrap_under_get_rule(untyped.as_concrete_TypeRef()) })
}

/// Reinterpret a `CFType` as an array of `CFType`. See [`as_dict`].
pub fn as_array(value: &CFType) -> Option<CFArray<CFType>> {
    let untyped = value.downcast::<CFArray>()?;
    Some(unsafe { CFArray::wrap_under_get_rule(untyped.as_concrete_TypeRef()) })
}

/// A nested dictionary property, e.g. `IOBlockStorageDriver`'s `Statistics`.
pub fn dict_dict(
    dict: &CFDictionary<CFString, CFType>,
    key: &str,
) -> Option<CFDictionary<CFString, CFType>> {
    as_dict(&dict_get(dict, key)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry root always exists, so this exercises the matching +
    /// property-snapshot path without depending on any particular hardware.
    #[test]
    fn platform_expert_is_matchable_and_has_a_model() {
        let services = matching_services("IOPlatformExpertDevice");
        assert!(!services.is_empty(), "IOPlatformExpertDevice should always match on macOS");
        let props = properties(services[0].0).expect("platform expert has properties");
        // `model` is CFData here ("Mac17,3"), which is exactly the CFData
        // fallback in dict_string.
        let model = dict_string(&props, "model").expect("model property");
        assert!(!model.is_empty());
    }

    /// A class that does not exist must yield an empty vec, not a panic — this
    /// is the contract every collector relies on for graceful degradation.
    #[test]
    fn unknown_service_class_yields_empty() {
        assert!(matching_services("SensorViewNoSuchClass").is_empty());
    }

    /// AppleSMC is absent on Apple Silicon; make sure asking for it is benign.
    #[test]
    fn missing_property_yields_none() {
        let services = matching_services("IOPlatformExpertDevice");
        let props = properties(services[0].0).unwrap();
        assert!(dict_i64(&props, "SensorViewNoSuchKey").is_none());
        assert!(dict_string(&props, "SensorViewNoSuchKey").is_none());
        assert!(dict_dict(&props, "SensorViewNoSuchKey").is_none());
    }
}
