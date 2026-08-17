//! Core Foundation main run loop ownership.

use std::{
    ffi::c_void,
    ptr::{self, NonNull},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) struct NativeMainRunLoop {
    run_loop: NonNull<c_void>,
    source: NonNull<c_void>,
    stopped: Arc<AtomicBool>,
}

impl NativeMainRunLoop {
    pub(crate) fn new() -> Option<Self> {
        let run_loop = NonNull::new(unsafe { CFRunLoopGetMain() }.cast_mut())?;
        if unsafe { CFRunLoopGetCurrent() } != run_loop.as_ptr().cast_const() {
            return None;
        }
        let mut context = CFRunLoopSourceContext::default();
        let source =
            NonNull::new(unsafe { CFRunLoopSourceCreate(ptr::null(), 0, &raw mut context) })?;
        unsafe {
            CFRunLoopAddSource(
                run_loop.as_ptr().cast_const(),
                source.as_ptr().cast_const(),
                kCFRunLoopDefaultMode,
            );
        }
        Some(Self {
            run_loop,
            source,
            stopped: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn stopper(&self) -> NativeMainRunLoopStopper {
        NativeMainRunLoopStopper {
            run_loop: self.run_loop.as_ptr() as usize,
            stopped: Arc::clone(&self.stopped),
        }
    }

    pub(crate) fn run(&self) {
        while !self.stopped.load(Ordering::Acquire) {
            unsafe { CFRunLoopRun() };
        }
    }
}

impl Drop for NativeMainRunLoop {
    fn drop(&mut self) {
        unsafe {
            CFRunLoopRemoveSource(
                self.run_loop.as_ptr().cast_const(),
                self.source.as_ptr().cast_const(),
                kCFRunLoopDefaultMode,
            );
            CFRelease(self.source.as_ptr().cast_const());
        }
    }
}

pub(crate) struct NativeMainRunLoopStopper {
    run_loop: usize,
    stopped: Arc<AtomicBool>,
}

impl NativeMainRunLoopStopper {
    pub(crate) fn stop(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            unsafe { CFRunLoopStop(self.run_loop as *mut c_void) };
        }
    }
}

impl Drop for NativeMainRunLoopStopper {
    fn drop(&mut self) {
        self.stop();
    }
}

#[repr(C)]
#[derive(Default)]
struct CFRunLoopSourceContext {
    version: isize,
    info: *mut c_void,
    retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<unsafe extern "C" fn(*const c_void)>,
    copy_description: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    equal: Option<unsafe extern "C" fn(*const c_void, *const c_void) -> u8>,
    hash: Option<unsafe extern "C" fn(*const c_void) -> usize>,
    schedule: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void)>,
    cancel: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void)>,
    perform: Option<unsafe extern "C" fn(*mut c_void)>,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: *const c_void;
    fn CFRunLoopGetMain() -> *const c_void;
    fn CFRunLoopGetCurrent() -> *const c_void;
    fn CFRunLoopSourceCreate(
        allocator: *const c_void,
        order: isize,
        context: *mut CFRunLoopSourceContext,
    ) -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *const c_void, source: *const c_void, mode: *const c_void);
    fn CFRunLoopRemoveSource(run_loop: *const c_void, source: *const c_void, mode: *const c_void);
    fn CFRunLoopRun();
    fn CFRunLoopStop(run_loop: *mut c_void);
    fn CFRelease(value: *const c_void);
}
