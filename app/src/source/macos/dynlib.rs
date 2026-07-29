//! Runtime symbol lookup for Apple's private sensor APIs.
//!
//! `IOHIDEventSystemClient*` (in IOKit.framework) and the whole of
//! `libIOReport.dylib` are SPI: the symbols exist and are callable, but they
//! are absent from the public SDK stubs, so linking against them directly
//! either fails at link time or hard-fails at launch if Apple ever drops them.
//!
//! Resolving them at runtime instead makes their absence a *recoverable*
//! condition. That matters more than usual here: the release profile is
//! `panic = "abort"`, so "sensor unavailable" has to be representable as data,
//! not as a panic. Every lookup returns `Option`, and every caller degrades to
//! producing no sensors.
//!
//! Consequence worth stating: an app using these can be distributed as a
//! signed/notarized `.dmg` (notarization does not inspect SPI use) but can
//! never ship on the Mac App Store.

use std::ffi::CString;

/// Look a symbol up in an explicitly opened dylib.
pub struct Library {
    handle: *mut libc::c_void,
}

// SAFETY: the handle is only ever passed to dlsym, which is thread-safe, and
// the library is never closed for the process lifetime.
unsafe impl Send for Library {}

impl Library {
    /// `None` if the library isn't present on this macOS version.
    pub fn open(path: &str) -> Option<Self> {
        let cpath = CString::new(path).ok()?;
        // RTLD_LAZY|RTLD_LOCAL: we only need the symbols we ask for.
        let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        (!handle.is_null()).then_some(Self { handle })
    }

    /// Resolve a symbol and transmute it to a function pointer type.
    ///
    /// # Safety
    /// `F` must exactly match the symbol's real ABI signature. Getting this
    /// wrong is UB, so every call site keeps the C declaration in a comment
    /// next to the type alias.
    pub unsafe fn symbol<F: Copy>(&self, name: &str) -> Option<F> {
        debug_assert_eq!(
            std::mem::size_of::<F>(),
            std::mem::size_of::<*const libc::c_void>(),
            "F must be a plain function pointer"
        );
        let cname = CString::new(name).ok()?;
        let ptr = libc::dlsym(self.handle, cname.as_ptr());
        (!ptr.is_null()).then(|| std::mem::transmute_copy(&ptr))
    }
}

/// Resolve a symbol from any image already loaded into the process.
///
/// Used for the `IOHIDEventSystemClient*` family: IOKit.framework is linked in
/// already (see `iokit.rs`), so its private symbols are reachable without
/// dlopen'ing the framework a second time.
///
/// # Safety
/// As [`Library::symbol`] — `F` must match the real signature.
pub unsafe fn global_symbol<F: Copy>(name: &str) -> Option<F> {
    debug_assert_eq!(
        std::mem::size_of::<F>(),
        std::mem::size_of::<*const libc::c_void>(),
        "F must be a plain function pointer"
    );
    let cname = CString::new(name).ok()?;
    let ptr = libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr());
    (!ptr.is_null()).then(|| std::mem::transmute_copy(&ptr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_library_is_none_not_a_panic() {
        assert!(Library::open("/usr/lib/libSensorViewNoSuchThing.dylib").is_none());
    }

    #[test]
    fn missing_symbol_is_none_not_a_panic() {
        type Fp = unsafe extern "C" fn() -> i32;
        assert!(unsafe { global_symbol::<Fp>("SensorViewNoSuchSymbol") }.is_none());
    }

    /// Documents the load-bearing assumption: libIOReport is not a file on
    /// disk on modern macOS, it lives in the dyld shared cache, and dlopen
    /// still resolves it. If this ever fails, power sensors go away and the
    /// app must still run.
    #[test]
    fn libioreport_is_dlopenable_from_the_shared_cache() {
        assert!(
            !std::path::Path::new("/usr/lib/libIOReport.dylib").exists(),
            "if this became a real file the comment above is stale"
        );
        assert!(Library::open("/usr/lib/libIOReport.dylib").is_some());
    }
}
