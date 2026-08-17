//! Carbon Text Input Source bindings owned by the process main thread.

use std::{
    ffi::{CStr, c_char, c_void},
    ptr::{self, NonNull},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::input_source::{InputSourceType, input_source_uses_ime};

type CfRef = *const c_void;
type NotificationCallback = extern "C" fn(CfRef, *mut c_void, CfRef, CfRef, CfRef);

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const DELIVER_IMMEDIATELY: isize = 4;

pub(crate) struct NativeInputSourceObserver {
    center: NonNull<c_void>,
    context: Box<ObserverContext>,
}

struct ObserverContext {
    active: Arc<AtomicBool>,
}

impl NativeInputSourceObserver {
    pub(crate) fn new(active: Arc<AtomicBool>) -> Option<Self> {
        if unsafe { CFRunLoopGetCurrent() } != unsafe { CFRunLoopGetMain() } {
            return None;
        }
        let center =
            NonNull::new(unsafe { CFNotificationCenterGetDistributedCenter() }.cast_mut())?;
        let context = Box::new(ObserverContext { active });
        let observer = (&raw const *context).cast_mut().cast();
        unsafe {
            CFNotificationCenterAddObserver(
                center.as_ptr(),
                observer,
                input_source_changed,
                kTISNotifySelectedKeyboardInputSourceChanged,
                ptr::null(),
                DELIVER_IMMEDIATELY,
            );
        }
        let observer = Self { center, context };
        let initial = current_source_uses_ime();
        observer.context.active.store(initial, Ordering::Release);
        Some(observer)
    }
}

impl Drop for NativeInputSourceObserver {
    fn drop(&mut self) {
        let observer = (&raw const *self.context).cast_mut().cast();
        unsafe {
            CFNotificationCenterRemoveObserver(
                self.center.as_ptr(),
                observer,
                kTISNotifySelectedKeyboardInputSourceChanged,
                ptr::null(),
            );
        }
    }
}

extern "C" fn input_source_changed(
    _center: CfRef,
    observer: *mut c_void,
    _name: CfRef,
    _object: CfRef,
    _user_info: CfRef,
) {
    let Some(context) = (unsafe { observer.cast::<ObserverContext>().as_ref() }) else {
        return;
    };
    refresh(context);
}

fn refresh(context: &ObserverContext) {
    // A failed TIS read is privacy-sensitive: keep text suppressed until a later refresh.
    let active = current_source_uses_ime();
    context.active.store(active, Ordering::Release);
}

fn current_source_uses_ime() -> bool {
    let Some(source) = OwnedInputSource::current() else {
        return true;
    };
    let source_type = source
        .property(unsafe { kTISPropertyInputSourceType })
        .map(classify_source_type);
    let (id, mode) = if source_type.is_none() {
        (
            source.string_property(unsafe { kTISPropertyInputSourceID }),
            source.string_property(unsafe { kTISPropertyInputModeID }),
        )
    } else {
        (None, None)
    };
    input_source_uses_ime(source_type, id.as_deref(), mode.as_deref())
}

fn classify_source_type(source_type: CfRef) -> InputSourceType {
    if unsafe { CFEqual(source_type, kTISTypeKeyboardInputMode) != 0 } {
        InputSourceType::KeyboardInputMode
    } else if unsafe { CFEqual(source_type, kTISTypeKeyboardLayout) != 0 } {
        InputSourceType::KeyboardLayout
    } else {
        InputSourceType::Other
    }
}

struct OwnedInputSource(NonNull<c_void>);

impl OwnedInputSource {
    fn current() -> Option<Self> {
        NonNull::new(unsafe { TISCopyCurrentKeyboardInputSource() }.cast_mut()).map(Self)
    }

    fn property(&self, key: CfRef) -> Option<CfRef> {
        let value = unsafe { TISGetInputSourceProperty(self.0.as_ptr(), key) };
        (!value.is_null()).then_some(value)
    }

    fn string_property(&self, key: CfRef) -> Option<String> {
        self.property(key).and_then(cf_string)
    }
}

impl Drop for OwnedInputSource {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0.as_ptr()) };
    }
}

fn cf_string(value: CfRef) -> Option<String> {
    if unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
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
    (success != 0)
        .then(|| unsafe { CStr::from_ptr(buffer.as_ptr().cast::<c_char>()) })
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyInputSourceID: CfRef;
    static kTISPropertyInputModeID: CfRef;
    static kTISPropertyInputSourceType: CfRef;
    static kTISTypeKeyboardInputMode: CfRef;
    static kTISTypeKeyboardLayout: CfRef;
    static kTISNotifySelectedKeyboardInputSourceChanged: CfRef;
    fn TISCopyCurrentKeyboardInputSource() -> CfRef;
    fn TISGetInputSourceProperty(input_source: CfRef, property_key: CfRef) -> CfRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFNotificationCenterGetDistributedCenter() -> CfRef;
    fn CFRunLoopGetCurrent() -> CfRef;
    fn CFRunLoopGetMain() -> CfRef;
    fn CFNotificationCenterAddObserver(
        center: CfRef,
        observer: *mut c_void,
        callback: NotificationCallback,
        name: CfRef,
        object: CfRef,
        suspension_behavior: isize,
    );
    fn CFNotificationCenterRemoveObserver(
        center: CfRef,
        observer: *mut c_void,
        name: CfRef,
        object: CfRef,
    );
    fn CFEqual(left: CfRef, right: CfRef) -> u8;
    fn CFRelease(value: CfRef);
    fn CFGetTypeID(value: CfRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetLength(value: CfRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        value: CfRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
}
