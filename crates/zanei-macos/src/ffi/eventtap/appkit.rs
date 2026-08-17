//! AppKit and window-context bindings used from the EventTap worker.

use std::{
    ffi::{CStr, c_char, c_int, c_void},
    mem,
    ptr::{self, NonNull},
    sync::{
        OnceLock,
        mpsc::{Receiver, SyncSender, sync_channel},
    },
};

use crate::eventtap::logic::{PasteboardContent, PasteboardKind};

type ObjcId = *mut c_void;
type ObjcClass = *mut c_void;
type ObjcSel = *mut c_void;
type ObjcIvar = *mut c_void;
#[cfg(target_arch = "x86_64")]
type ObjcBool = i8;
#[cfg(not(target_arch = "x86_64"))]
type ObjcBool = bool;
type CfRef = *const c_void;
type CfMutableRef = *mut c_void;

const CF_STRING_UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT64: isize = 4;
const WINDOW_LIST_OPTIONS: u32 = (1 << 0) | (1 << 4);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeApp {
    pub(crate) name: String,
    pub(crate) bundle_id: Option<String>,
    pub(crate) pid: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeWindow {
    pub(crate) title: Option<String>,
    pub(crate) id: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeContext {
    pub(crate) app: NativeApp,
    pub(crate) window: Option<NativeWindow>,
}

pub(crate) fn current_context() -> Option<NativeContext> {
    let _pool = AutoreleasePool::new();
    // SAFETY: Selectors match NSWorkspace and NSRunningApplication signatures.
    unsafe {
        let workspace = msg_id(
            objc_getClass(c"NSWorkspace".as_ptr()),
            sel(c"sharedWorkspace"),
        );
        let application = msg_id(workspace, sel(c"frontmostApplication"));
        if application.is_null() {
            return None;
        }
        let name = ns_string(msg_id(application, sel(c"localizedName")))?;
        let bundle_id = ns_string(msg_id(application, sel(c"bundleIdentifier")));
        let pid = msg_i32(application, sel(c"processIdentifier"));
        if pid <= 0 {
            return None;
        }
        Some(NativeContext {
            app: NativeApp {
                name,
                bundle_id,
                pid: i64::from(pid),
            },
            window: front_window(i64::from(pid)),
        })
    }
}

pub(crate) struct Pasteboard;

impl Pasteboard {
    pub(crate) const fn new() -> Self {
        Self
    }

    pub(crate) fn change_count(&self) -> i64 {
        let _pool = AutoreleasePool::new();
        // SAFETY: Selectors match documented NSPasteboard signatures.
        unsafe {
            let pasteboard = msg_id(
                objc_getClass(c"NSPasteboard".as_ptr()),
                sel(c"generalPasteboard"),
            );
            msg_i64(pasteboard, sel(c"changeCount"))
        }
    }

    pub(crate) fn content(&self, include_content: bool) -> PasteboardContent {
        let _pool = AutoreleasePool::new();
        // SAFETY: Selectors and pasteboard type constants are documented AppKit API.
        unsafe {
            let pasteboard = msg_id(
                objc_getClass(c"NSPasteboard".as_ptr()),
                sel(c"generalPasteboard"),
            );
            let types = msg_id(pasteboard, sel(c"types"));
            let (kind, data_type) = if array_contains(types, NSPasteboardTypeFileURL) {
                (PasteboardKind::File, NSPasteboardTypeFileURL)
            } else if array_contains(types, NSPasteboardTypePNG) {
                (PasteboardKind::Image, NSPasteboardTypePNG)
            } else if array_contains(types, NSPasteboardTypeTIFF) {
                (PasteboardKind::Image, NSPasteboardTypeTIFF)
            } else if array_contains(types, NSPasteboardTypeString) {
                (PasteboardKind::Text, NSPasteboardTypeString)
            } else {
                (PasteboardKind::Other, ptr::null_mut())
            };
            if !include_content {
                return PasteboardContent {
                    kind,
                    size_bytes: None,
                    text: None,
                };
            }
            if kind == PasteboardKind::Text {
                let text = ns_string(msg_id_arg(pasteboard, sel(c"stringForType:"), data_type));
                PasteboardContent {
                    kind,
                    size_bytes: text
                        .as_ref()
                        .and_then(|value| u64::try_from(value.len()).ok()),
                    text,
                }
            } else {
                let data = (!data_type.is_null())
                    .then(|| msg_id_arg(pasteboard, sel(c"dataForType:"), data_type))
                    .filter(|value| !value.is_null());
                PasteboardContent {
                    kind,
                    size_bytes: data
                        .and_then(|value| u64::try_from(msg_usize(value, sel(c"length"))).ok()),
                    text: None,
                }
            }
        }
    }
}

pub(crate) struct WakeObserver {
    observer: NonNull<c_void>,
    center: NonNull<c_void>,
    receiver: Receiver<()>,
    _context: Box<WakeContext>,
}

struct WakeContext {
    sender: SyncSender<()>,
}

impl WakeObserver {
    pub(crate) fn new(queue_capacity: usize) -> Result<Self, &'static str> {
        let _pool = AutoreleasePool::new();
        let class = wake_observer_class()?;
        let (sender, receiver) = sync_channel(queue_capacity);
        let context = Box::new(WakeContext { sender });
        // SAFETY: The class has the pointer ivar and callback configured below.
        unsafe {
            let observer = NonNull::new(msg_id(class, sel(c"new")))
                .ok_or("failed to allocate wake observer")?;
            let ivar = class_getInstanceVariable(class, c"_zaneiContext".as_ptr());
            object_setIvar(
                observer.as_ptr(),
                ivar,
                (&*context as *const WakeContext).cast_mut().cast(),
            );
            let workspace = msg_id(
                objc_getClass(c"NSWorkspace".as_ptr()),
                sel(c"sharedWorkspace"),
            );
            let center = NonNull::new(msg_id(workspace, sel(c"notificationCenter")))
                .ok_or("NSWorkspace notificationCenter returned null")?;
            msg_void_observer(
                center.as_ptr(),
                sel(c"addObserver:selector:name:object:"),
                observer.as_ptr(),
                sel(c"zaneiDidWake:"),
                NSWorkspaceDidWakeNotification,
                ptr::null_mut(),
            );
            Ok(Self {
                observer,
                center,
                receiver,
                _context: context,
            })
        }
    }

    pub(crate) fn take_wake(&self) -> bool {
        let mut observed = false;
        while self.receiver.try_recv().is_ok() {
            observed = true;
        }
        observed
    }
}

impl Drop for WakeObserver {
    fn drop(&mut self) {
        // SAFETY: Registration is removed before the observer and context are dropped.
        unsafe {
            msg_void_arg(
                self.center.as_ptr(),
                sel(c"removeObserver:"),
                self.observer.as_ptr(),
            );
            objc_release(self.observer.as_ptr());
        }
    }
}

unsafe fn front_window(pid: i64) -> Option<NativeWindow> {
    // SAFETY: CoreGraphics returns a retained CFArray or null.
    let windows = NonNull::new(unsafe { CGWindowListCopyWindowInfo(WINDOW_LIST_OPTIONS, 0) })?;
    let count = unsafe { CFArrayGetCount(windows.as_ptr()) };
    let mut result = None;
    for index in 0..count {
        // SAFETY: index is inside this valid CFArray.
        let dictionary = unsafe { CFArrayGetValueAtIndex(windows.as_ptr(), index) };
        if unsafe { dictionary_i64(dictionary, kCGWindowOwnerPID) } == Some(pid)
            && unsafe { dictionary_i64(dictionary, kCGWindowLayer) } == Some(0)
        {
            result = Some(NativeWindow {
                title: unsafe { dictionary_string(dictionary, kCGWindowName) },
                id: unsafe { dictionary_i64(dictionary, kCGWindowNumber) },
            });
            break;
        }
    }
    // SAFETY: balances Copy ownership.
    unsafe { CFRelease(windows.as_ptr().cast_const()) };
    result
}

unsafe fn dictionary_i64(dictionary: CfRef, key: CfRef) -> Option<i64> {
    let value = unsafe { CFDictionaryGetValue(dictionary, key) };
    if value.is_null() {
        return None;
    }
    let mut number = 0_i64;
    // SAFETY: These documented window-info keys contain CFNumber values.
    unsafe { CFNumberGetValue(value, CF_NUMBER_SINT64, (&mut number as *mut i64).cast()) };
    Some(number)
}

unsafe fn dictionary_string(dictionary: CfRef, key: CfRef) -> Option<String> {
    cf_string(unsafe { CFDictionaryGetValue(dictionary, key) })
}

fn cf_string(value: CfRef) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let mut buffer = vec![0_i8; 4_096];
    // SAFETY: value is a CFString and buffer is writable for its full length.
    let copied = unsafe {
        CFStringGetCString(
            value,
            buffer.as_mut_ptr(),
            isize::try_from(buffer.len()).ok()?,
            CF_STRING_UTF8,
        )
    };
    (copied != 0).then(|| {
        // SAFETY: success writes a NUL-terminated string.
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    })
}

fn wake_observer_class() -> Result<ObjcClass, &'static str> {
    static CLASS: OnceLock<usize> = OnceLock::new();
    if let Some(class) = CLASS.get() {
        return Ok(*class as ObjcClass);
    }
    // SAFETY: Registration completes before any instance is created.
    unsafe {
        let class = objc_allocateClassPair(
            objc_getClass(c"NSObject".as_ptr()),
            c"ZaneiEventTapWakeObserver".as_ptr(),
            0,
        );
        if class.is_null() {
            return NonNull::new(objc_getClass(c"ZaneiEventTapWakeObserver".as_ptr()))
                .map(NonNull::as_ptr)
                .ok_or("failed to register wake observer class");
        }
        let pointer_size = mem::size_of::<*mut c_void>();
        if class_addIvar(
            class,
            c"_zaneiContext".as_ptr(),
            pointer_size,
            pointer_size.trailing_zeros() as u8,
            c"^v".as_ptr(),
        ) as u8
            == 0
            || class_addMethod(
                class,
                sel(c"zaneiDidWake:"),
                mem::transmute::<extern "C" fn(ObjcId, ObjcSel, ObjcId), unsafe extern "C" fn()>(
                    wake_callback,
                ),
                c"v@:@".as_ptr(),
            ) as u8
                == 0
        {
            objc_disposeClassPair(class);
            return Err("failed to configure wake observer class");
        }
        objc_registerClassPair(class);
        let _ = CLASS.set(class as usize);
        Ok(class)
    }
}

extern "C" fn wake_callback(observer: ObjcId, _selector: ObjcSel, _notification: ObjcId) {
    // SAFETY: This method is installed only on the registered observer class.
    unsafe {
        let class = object_getClass(observer);
        let ivar = class_getInstanceVariable(class, c"_zaneiContext".as_ptr());
        if let Some(context) = object_getIvar(observer, ivar)
            .cast::<WakeContext>()
            .as_ref()
        {
            let _ = context.sender.try_send(());
        }
    }
}

struct AutoreleasePool(NonNull<c_void>);

impl AutoreleasePool {
    fn new() -> Self {
        // SAFETY: libobjc returns a valid autorelease pool token.
        let value = unsafe { objc_autoreleasePoolPush() };
        Self(NonNull::new(value).expect("objc_autoreleasePoolPush returned null"))
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        // SAFETY: balances objc_autoreleasePoolPush.
        unsafe { objc_autoreleasePoolPop(self.0.as_ptr()) };
    }
}

unsafe fn array_contains(array: ObjcId, value: ObjcId) -> bool {
    !array.is_null() && unsafe { msg_bool_arg(array, sel(c"containsObject:"), value) as u8 != 0 }
}

unsafe fn ns_string(value: ObjcId) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let bytes = unsafe { msg_c_string(value, sel(c"UTF8String")) };
    NonNull::new(bytes.cast_mut()).map(|bytes| {
        // SAFETY: NSString UTF8String is NUL-terminated for the object's lifetime.
        unsafe { CStr::from_ptr(bytes.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    })
}

unsafe fn sel(name: &CStr) -> ObjcSel {
    unsafe { sel_registerName(name.as_ptr()) }
}

macro_rules! msg_send_fn {
    ($name:ident, $return:ty, ($($arg:ident: $type:ty),*)) => {
        unsafe fn $name(receiver: ObjcId, selector: ObjcSel, $($arg: $type),*) -> $return {
            type Function = unsafe extern "C" fn(ObjcId, ObjcSel, $($type),*) -> $return;
            let function: Function = unsafe { mem::transmute(objc_msgSend as *const ()) };
            unsafe { function(receiver, selector, $($arg),*) }
        }
    };
}

msg_send_fn!(msg_id, ObjcId, ());
msg_send_fn!(msg_i32, c_int, ());
msg_send_fn!(msg_i64, i64, ());
msg_send_fn!(msg_usize, usize, ());
msg_send_fn!(msg_c_string, *const c_char, ());
msg_send_fn!(msg_id_arg, ObjcId, (value: ObjcId));
msg_send_fn!(msg_bool_arg, ObjcBool, (value: ObjcId));
msg_send_fn!(msg_void_arg, (), (value: ObjcId));
msg_send_fn!(msg_void_observer, (), (
    observer: ObjcId,
    callback: ObjcSel,
    name: ObjcId,
    object: ObjcId
));

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(options: u32, relative_window: u32) -> CfMutableRef;
    static kCGWindowOwnerPID: CfRef;
    static kCGWindowLayer: CfRef;
    static kCGWindowName: CfRef;
    static kCGWindowNumber: CfRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: CfRef);
    fn CFArrayGetCount(array: CfRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
    fn CFDictionaryGetValue(dictionary: CfRef, key: CfRef) -> CfRef;
    fn CFNumberGetValue(number: CfRef, number_type: isize, value: *mut c_void) -> u8;
    fn CFStringGetCString(
        string: CfRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSPasteboardTypeString: ObjcId;
    static NSPasteboardTypePNG: ObjcId;
    static NSPasteboardTypeTIFF: ObjcId;
    static NSPasteboardTypeFileURL: ObjcId;
    static NSWorkspaceDidWakeNotification: ObjcId;
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> ObjcClass;
    fn sel_registerName(name: *const c_char) -> ObjcSel;
    fn objc_msgSend();
    fn objc_release(value: ObjcId);
    fn objc_autoreleasePoolPush() -> ObjcId;
    fn objc_autoreleasePoolPop(pool: ObjcId);
    fn objc_allocateClassPair(
        superclass: ObjcClass,
        name: *const c_char,
        extra_bytes: usize,
    ) -> ObjcClass;
    fn objc_registerClassPair(class: ObjcClass);
    fn objc_disposeClassPair(class: ObjcClass);
    fn class_addIvar(
        class: ObjcClass,
        name: *const c_char,
        size: usize,
        alignment: u8,
        types: *const c_char,
    ) -> ObjcBool;
    fn class_addMethod(
        class: ObjcClass,
        selector: ObjcSel,
        implementation: unsafe extern "C" fn(),
        types: *const c_char,
    ) -> ObjcBool;
    fn class_getInstanceVariable(class: ObjcClass, name: *const c_char) -> ObjcIvar;
    fn object_setIvar(object: ObjcId, ivar: ObjcIvar, value: ObjcId);
    fn object_getIvar(object: ObjcId, ivar: ObjcIvar) -> ObjcId;
    fn object_getClass(object: ObjcId) -> ObjcClass;
}
