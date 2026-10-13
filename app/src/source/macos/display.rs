//! Connected displays, for the System Summary.
//!
//! CoreGraphics is the authority on what is attached and how it is driven
//! (pixels, "looks like" points, refresh rate, physical size). It does not
//! name monitors, so names come from the IOKit display services, whose
//! `DisplayAttributes` → `ProductAttributes` carry the EDID product name and
//! product code; the code is what CoreGraphics reports as the model number,
//! which is how the two are joined.
//!
//! Without a window-server session (a launchd daemon, some SSH sessions)
//! CoreGraphics reports no displays; that is an empty list, not an error.

use std::ffi::c_void;

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};

use crate::sysinfo::DisplayInfo;

use super::iokit;

type CGDirectDisplayID = u32;
type CGDisplayModeRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGGetActiveDisplayList(max: u32, displays: *mut CGDirectDisplayID, count: *mut u32) -> i32;
    fn CGDisplayIsBuiltin(display: CGDirectDisplayID) -> u32;
    fn CGDisplayIsMain(display: CGDirectDisplayID) -> u32;
    fn CGDisplayModelNumber(display: CGDirectDisplayID) -> u32;
    fn CGDisplayScreenSize(display: CGDirectDisplayID) -> CGSize;
    fn CGDisplayCopyDisplayMode(display: CGDirectDisplayID) -> CGDisplayModeRef;
    fn CGDisplayModeGetWidth(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeGetHeight(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeGetPixelWidth(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeGetRefreshRate(mode: CGDisplayModeRef) -> f64;
    fn CGDisplayModeGetIOFlags(mode: CGDisplayModeRef) -> u32;
    fn CGDisplayModeRelease(mode: CGDisplayModeRef);
    fn CGDisplayCopyAllDisplayModes(display: CGDirectDisplayID, options: CFDictionaryRef) -> CFArrayRef;
    static kCGDisplayShowDuplicateLowResolutionModes: CFStringRef;
}

/// IOGraphicsTypes.h: the mode that matches the panel's physical pixels.
const K_DISPLAY_MODE_NATIVE_FLAG: u32 = 0x0200_0000;

/// The panel's native pixel size.
///
/// The *current* mode on a Retina display is a scaled backing store — a
/// MacBook Air's 2560 × 1664 panel "looking like" 1470 × 956 renders at
/// 2940 × 1912 and is downsampled — so the current mode's pixel size is not
/// the panel's. The native mode is flagged; it can be a 1x "duplicate
/// low-resolution" mode, hence the option that lists those too.
fn native_pixels(id: CGDirectDisplayID) -> Option<(u32, u32)> {
    let options = unsafe {
        CFDictionary::from_CFType_pairs(&[(
            CFString::wrap_under_get_rule(kCGDisplayShowDuplicateLowResolutionModes).as_CFType(),
            CFBoolean::true_value().as_CFType(),
        )])
    };
    let modes = unsafe { CGDisplayCopyAllDisplayModes(id, options.as_concrete_TypeRef()) };
    if modes.is_null() {
        return None;
    }
    let modes = unsafe { CFArray::<*const c_void>::wrap_under_create_rule(modes) };
    let mut best: Option<(u32, u32)> = None;
    for mode in modes.iter() {
        let mode: CGDisplayModeRef = *mode;
        if unsafe { CGDisplayModeGetIOFlags(mode) } & K_DISPLAY_MODE_NATIVE_FLAG == 0 {
            continue;
        }
        let px = unsafe { (CGDisplayModeGetPixelWidth(mode) as u32, CGDisplayModeGetPixelHeight(mode) as u32) };
        // Several refresh rates of the native size may all be flagged.
        if best.is_none_or(|b| px.0 * px.1 > b.0 * b.1) {
            best = Some(px);
        }
    }
    best.filter(|(w, h)| *w > 0 && *h > 0)
}

/// Every active display, the main one first.
pub fn displays() -> Vec<DisplayInfo> {
    let mut ids = [0 as CGDirectDisplayID; 16];
    let mut count = 0u32;
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0 {
        return Vec::new();
    }
    let names = edid_names();

    let mut out: Vec<DisplayInfo> = ids[..(count as usize).min(ids.len())]
        .iter()
        .map(|&id| {
            let built_in = unsafe { CGDisplayIsBuiltin(id) } != 0;
            let main = unsafe { CGDisplayIsMain(id) } != 0;

            let mut info = DisplayInfo { built_in, main, ..Default::default() };
            let mode = unsafe { CGDisplayCopyDisplayMode(id) };
            if !mode.is_null() {
                let px = unsafe { (CGDisplayModeGetPixelWidth(mode), CGDisplayModeGetPixelHeight(mode)) };
                let pt = unsafe { (CGDisplayModeGetWidth(mode), CGDisplayModeGetHeight(mode)) };
                // Built-in LCDs report 0 Hz: CoreGraphics' way of saying the
                // panel has no fixed refresh it will commit to.
                let hz = unsafe { CGDisplayModeGetRefreshRate(mode) };
                unsafe { CGDisplayModeRelease(mode) };
                // Native panel pixels where the mode list says which they
                // are; the current mode's pixels otherwise.
                let native = native_pixels(id)
                    .or_else(|| (px.0 > 0 && px.1 > 0).then_some((px.0 as u32, px.1 as u32)));
                if let Some((w, h)) = native {
                    info.width_px = Some(w);
                    info.height_px = Some(h);
                }
                if pt.0 > 0 && pt.1 > 0 && Some((pt.0 as u32, pt.1 as u32)) != native {
                    info.width_pt = Some(pt.0 as u32);
                    info.height_pt = Some(pt.1 as u32);
                }
                info.refresh_hz = (hz.is_finite() && hz > 0.0).then_some(hz);
            }

            let size = unsafe { CGDisplayScreenSize(id) };
            if size.width > 0.0 && size.height > 0.0 {
                let diagonal_mm = (size.width * size.width + size.height * size.height).sqrt();
                info.diagonal_in = Some(diagonal_mm / 25.4);
            }

            let model = unsafe { CGDisplayModelNumber(id) };
            info.name = names
                .iter()
                .find(|(product_id, _)| *product_id == u64::from(model))
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| if built_in { "Built-in Display" } else { "External Display" }.to_string());
            info
        })
        .collect();

    // Main display first, then built-in, then the rest in system order.
    out.sort_by_key(|d| (!d.main, !d.built_in));
    out
}

/// `(EDID product code, product name)` for every display IOKit can name.
///
/// Apple Silicon publishes these on the DCP display services; Intel Macs'
/// framebuffers carry the same `DisplayAttributes` on recent macOS. A display
/// whose name can't be found keeps a generic label.
fn edid_names() -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for class in ["IOMobileFramebuffer", "IOFramebuffer", "IODisplayConnect"] {
        for service in iokit::matching_services(class) {
            let Some(props) = iokit::properties(service.0) else {
                continue;
            };
            let Some(attrs) = iokit::dict_dict(&props, "DisplayAttributes") else {
                continue;
            };
            let Some(product) = iokit::dict_dict(&attrs, "ProductAttributes") else {
                continue;
            };
            let (Some(id), Some(name)) =
                (iokit::dict_i64(&product, "ProductID"), iokit::dict_string(&product, "ProductName"))
            else {
                continue;
            };
            if !out.iter().any(|(existing, _)| *existing == id as u64) {
                out.push((id as u64, name));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_have_sane_geometry() {
        let list = displays();
        if list.is_empty() {
            return crate::source::macos::absent("an active display (no window-server session?)");
        }
        assert!(list.iter().filter(|d| d.main).count() <= 1, "more than one main display");
        for d in &list {
            assert!(!d.name.is_empty());
            if let (Some(w), Some(h)) = (d.width_px, d.height_px) {
                assert!((320..=16384).contains(&w) && (200..=16384).contains(&h), "{w}x{h}");
            }
            if let Some(hz) = d.refresh_hz {
                assert!((20.0..=500.0).contains(&hz), "refresh {hz} Hz");
            }
            if let Some(inches) = d.diagonal_in {
                assert!((5.0..=120.0).contains(&inches), "diagonal {inches} in");
            }
        }
    }
}
