//! Core Foundation ownership used by the AX wrapper.

use std::{
    ffi::{CStr, CString, c_char, c_void},
    ptr::{self, NonNull},
    time::Duration,
};

pub(super) type CfRef = *const c_void;

pub(super) struct OwnedCf(NonNull<c_void>);

impl OwnedCf {
    pub(super) unsafe fn from_create(value: CfRef) -> Option<Self> {
        NonNull::new(value.cast_mut()).map(Self)
    }

    pub(super) unsafe fn retain(value: CfRef) -> Option<Self> {
        if value.is_null() {
            return None;
        }
        NonNull::new(unsafe { CFRetain(value) }.cast_mut()).map(Self)
    }

    pub(super) fn as_ptr(&self) -> CfRef {
        self.0.as_ptr()
    }
}

unsafe impl Send for OwnedCf {}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.as_ptr()) };
    }
}

pub(super) fn cf_string(value: &str) -> Option<OwnedCf> {
    let value = CString::new(value).ok()?;
    let string =
        unsafe { CFStringCreateWithCString(ptr::null(), value.as_ptr(), CF_STRING_ENCODING_UTF8) };
    unsafe { OwnedCf::from_create(string) }
}

pub(super) fn string_value(value: CfRef) -> Option<String> {
    if value.is_null() || unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
        return None;
    }
    let length = unsafe { CFStringGetLength(value) };
    let maximum = unsafe { CFStringGetMaximumSizeForEncoding(length, CF_STRING_ENCODING_UTF8) };
    let capacity = usize::try_from(maximum).ok()?.checked_add(1)?;
    let mut buffer = vec![0_u8; capacity];
    let success = unsafe {
        CFStringGetCString(
            value,
            buffer.as_mut_ptr().cast::<c_char>(),
            isize::try_from(capacity).ok()?,
            CF_STRING_ENCODING_UTF8,
        )
    };
    if success == 0 {
        return None;
    }
    unsafe { CStr::from_ptr(buffer.as_ptr().cast::<c_char>()) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

pub(super) fn i64_value(value: CfRef) -> Option<i64> {
    if value.is_null() || unsafe { CFGetTypeID(value) } != unsafe { CFNumberGetTypeID() } {
        return None;
    }
    let mut number = 0_i64;
    let success = unsafe {
        CFNumberGetValue(
            value,
            CF_NUMBER_SINT64_TYPE,
            (&raw mut number).cast::<c_void>(),
        )
    };
    (success != 0).then_some(number)
}

pub(super) fn run_loop_tick(timeout: Duration) {
    unsafe {
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, timeout.as_secs_f64(), 1);
    }
}

pub(super) unsafe fn add_current_run_loop_source(source: CfRef) {
    unsafe {
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
    }
}

pub(super) unsafe fn remove_current_run_loop_source(source: CfRef) {
    unsafe {
        CFRunLoopRemoveSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
    }
}

pub(super) fn boolean_true() -> CfRef {
    unsafe { kCFBooleanTrue }
}

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT64_TYPE: isize = 4;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: CfRef;
    static kCFBooleanTrue: CfRef;
    fn CFRetain(value: CfRef) -> CfRef;
    fn CFRelease(value: CfRef);
    fn CFGetTypeID(value: CfRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        value: *const c_char,
        encoding: u32,
    ) -> CfRef;
    fn CFStringGetLength(value: CfRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        value: CfRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
    fn CFNumberGetValue(value: CfRef, number_type: isize, output: *mut c_void) -> u8;
    fn CFRunLoopGetCurrent() -> CfRef;
    fn CFRunLoopRunInMode(mode: CfRef, seconds: f64, return_after_source: u8) -> i32;
    fn CFRunLoopAddSource(run_loop: CfRef, source: CfRef, mode: CfRef);
    fn CFRunLoopRemoveSource(run_loop: CfRef, source: CfRef, mode: CfRef);
}
