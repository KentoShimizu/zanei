//! Objective-C and Core Foundation primitives shared by the capture and recognition bindings.

use std::{
    ffi::{CStr, c_char, c_int, c_void},
    mem::transmute,
    ptr::NonNull,
    sync::OnceLock,
};

use crate::ffi::geometry::AxFrame;

pub(crate) type ObjcId = *mut c_void;
pub(crate) type ObjcSel = *mut c_void;
pub(crate) type CfRef = *const c_void;
#[cfg(target_arch = "x86_64")]
pub(crate) type ObjcBool = i8;
#[cfg(not(target_arch = "x86_64"))]
pub(crate) type ObjcBool = bool;

const RTLD_LAZY: c_int = 0x1;

pub(crate) type Classes<const N: usize> = OnceLock<Option<[usize; N]>>;

/// Loads a system framework once and resolves class objects as addresses; class objects are
/// immortal and thread-safe. `None` when the framework or any class is missing on this macOS.
pub(crate) fn load_classes<const N: usize>(
    cache: &'static Classes<N>,
    framework: &CStr,
    names: [&CStr; N],
) -> Option<[usize; N]> {
    *cache.get_or_init(|| {
        // SAFETY: loading a system framework by absolute path; the handle is never closed.
        if unsafe { dlopen(framework.as_ptr(), RTLD_LAZY) }.is_null() {
            return None;
        }
        let mut classes = [0; N];
        for (slot, name) in classes.iter_mut().zip(names) {
            *slot = NonNull::new(unsafe { class(name) })?.as_ptr() as usize;
        }
        Some(classes)
    })
}

pub(crate) fn srgb_color_space() -> Option<OwnedCf> {
    // SAFETY: the name is a CoreGraphics constant; the result follows the create rule.
    unsafe { OwnedCf::from_created(CGColorSpaceCreateWithName(kCGColorSpaceSRGB)) }
}

pub(crate) struct OwnedObject(NonNull<c_void>);

// SAFETY: the retained ScreenCaptureKit and Vision objects held here may be released on any thread.
unsafe impl Send for OwnedObject {}

impl OwnedObject {
    pub(crate) unsafe fn from_retained(object: ObjcId) -> Option<Self> {
        NonNull::new(object).map(Self)
    }

    pub(crate) unsafe fn retain(object: ObjcId) -> Option<Self> {
        NonNull::new(unsafe { objc_retain(object) }).map(Self)
    }

    pub(crate) fn as_ptr(&self) -> ObjcId {
        self.0.as_ptr()
    }
}

impl Drop for OwnedObject {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns exactly one retain.
        unsafe { objc_release(self.as_ptr()) };
    }
}

pub(crate) struct OwnedCf(NonNull<c_void>);

// SAFETY: the Core Foundation and CoreGraphics objects held here may be released on any thread.
unsafe impl Send for OwnedCf {}

impl OwnedCf {
    pub(crate) unsafe fn from_created(value: CfRef) -> Option<Self> {
        NonNull::new(value.cast_mut()).map(Self)
    }

    pub(crate) unsafe fn retain(value: CfRef) -> Option<Self> {
        if value.is_null() {
            return None;
        }
        unsafe { Self::from_created(CFRetain(value)) }
    }

    pub(crate) fn as_ptr(&self) -> CfRef {
        self.0.as_ptr().cast_const()
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns exactly one Core Foundation reference.
        unsafe { CFRelease(self.as_ptr()) };
    }
}

/// Drains objects that Objective-C autoreleases on threads without their own pool.
pub(crate) struct AutoreleasePool(ObjcId);

impl AutoreleasePool {
    pub(crate) fn new() -> Self {
        Self(unsafe { objc_autoreleasePoolPush() })
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        unsafe { objc_autoreleasePoolPop(self.0) };
    }
}

pub(crate) unsafe fn class(name: &CStr) -> ObjcId {
    unsafe { objc_getClass(name.as_ptr()) }
}

pub(crate) unsafe fn sel(name: &CStr) -> ObjcSel {
    unsafe { sel_registerName(name.as_ptr()) }
}

/// `objc_msgSend` cast to the method's C signature, for calls without a typed helper below.
pub(crate) fn msg_send<F>() -> F {
    const { assert!(size_of::<F>() == size_of::<unsafe extern "C" fn()>()) };
    // SAFETY: F is a function pointer type (checked size); callers name the method's signature.
    unsafe { std::mem::transmute_copy(&(objc_msgSend as unsafe extern "C" fn())) }
}

pub(crate) unsafe fn send_id_0(receiver: ObjcId, selector: ObjcSel) -> ObjcId {
    unsafe { msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel) -> ObjcId>()(receiver, selector) }
}

pub(crate) unsafe fn send_id_id(receiver: ObjcId, selector: ObjcSel, argument: ObjcId) -> ObjcId {
    let function = msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel, ObjcId) -> ObjcId>();
    unsafe { function(receiver, selector, argument) }
}

pub(crate) unsafe fn send_id_usize(receiver: ObjcId, selector: ObjcSel, argument: usize) -> ObjcId {
    let function = msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel, usize) -> ObjcId>();
    unsafe { function(receiver, selector, argument) }
}

pub(crate) unsafe fn send_void_bool(receiver: ObjcId, selector: ObjcSel, argument: bool) {
    let function = msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel, ObjcBool)>();
    unsafe { function(receiver, selector, ObjcBool::from(argument)) };
}

pub(crate) unsafe fn send_usize_0(receiver: ObjcId, selector: ObjcSel) -> usize {
    unsafe { msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel) -> usize>()(receiver, selector) }
}

/// The `code` of an `NSError`, or `None` for nil.
pub(crate) unsafe fn error_code(error: ObjcId) -> Option<isize> {
    let code = msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel) -> isize>();
    (!error.is_null()).then(|| unsafe { code(error, sel(c"code")) })
}

pub(crate) unsafe fn send_frame_0(receiver: ObjcId, selector: ObjcSel) -> AxFrame {
    // x86_64 returns a CGRect through memory, which needs the struct-return entry point.
    #[cfg(target_arch = "x86_64")]
    let entry = objc_msgSend_stret as unsafe extern "C" fn();
    #[cfg(not(target_arch = "x86_64"))]
    let entry = objc_msgSend as unsafe extern "C" fn();
    let function: unsafe extern "C" fn(ObjcId, ObjcSel) -> AxFrame = unsafe { transmute(entry) };
    unsafe { function(receiver, selector) }
}

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> ObjcId;
    fn sel_registerName(name: *const c_char) -> ObjcSel;
    fn objc_msgSend();
    #[cfg(target_arch = "x86_64")]
    fn objc_msgSend_stret();
    fn objc_retain(object: ObjcId) -> ObjcId;
    fn objc_release(object: ObjcId);
    fn objc_autoreleasePoolPush() -> ObjcId;
    fn objc_autoreleasePoolPop(pool: ObjcId);
}

// Foundation and AppKit classes (`NSArray`, `NSDictionary`, `NSApplication`) are looked up by name.
#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGColorSpaceSRGB: CfRef;
    fn CGColorSpaceCreateWithName(name: CfRef) -> CfRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(value: CfRef) -> CfRef;
    fn CFRelease(value: CfRef);
}
