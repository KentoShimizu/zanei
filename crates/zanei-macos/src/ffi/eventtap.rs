//! Safe ownership boundary for CGEventTap calls.

mod appkit;
mod authorization;

use std::{
    ffi::{c_double, c_ulong, c_void},
    ptr::{self, NonNull},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    time::{Duration, Instant},
};

#[cfg(test)]
pub(crate) use appkit::{NativeApp, NativeWindow};
pub(crate) use appkit::{NativeContext, Pasteboard, WakeObserver, current_context};

use crate::{
    eventtap::{
        EventTapMode,
        logic::{KeyModifiers, KeyObservation},
    },
    focused_field::{FocusedField, FocusedFieldTracker},
    input_source::ImeState,
    secure_input::{SecureInputProbe, SecureInputProbeError},
    text_capture::{InputAuthorization, InputAuthorizationPublisher},
};

use authorization::{input_target, prepare_input_authorization, trace_target};

type CfRef = *const c_void;
type CfMutableRef = *mut c_void;

const EVENT_KEY_DOWN: u32 = 10;
const EVENT_SCROLL_WHEEL: u32 = 22;
const EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const EVENT_DISABLED_TIMEOUT: u32 = 0xffff_fffe;
const EVENT_DISABLED_USER_INPUT: u32 = 0xffff_ffff;
const ANNOTATED_SESSION_EVENT_TAP: u32 = 2;
const HEAD_INSERT_EVENT_TAP: u32 = 0;
const LISTEN_ONLY: u32 = 1;
const KEYBOARD_KEYCODE_FIELD: u32 = 9;
const EVENT_TARGET_UNIX_PROCESS_ID_FIELD: u32 = 40;
const SCROLL_FIXED_AXIS_1_FIELD: u32 = 93;
const SCROLL_FIXED_AXIS_2_FIELD: u32 = 94;
const MOUSE_CLICK_STATE_FIELD: u32 = 1;
const MOUSE_BUTTON_NUMBER_FIELD: u32 = 3;
const FLAG_SHIFT: u64 = 1 << 17;
const FLAG_CONTROL: u64 = 1 << 18;
const FLAG_ALTERNATE: u64 = 1 << 19;
const FLAG_COMMAND: u64 = 1 << 20;
const FLAG_SECONDARY_FN: u64 = 1 << 23;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeDisableReason {
    Timeout,
    UserInput,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NativeEvent {
    Key {
        observation: KeyObservation,
        target: Option<NativeInputTarget>,
        authorization: Option<InputAuthorization>,
        secure_input: bool,
        ime_active: bool,
    },
    Scroll {
        vertical: f64,
        horizontal: f64,
    },
    MouseDown {
        x: f64,
        y: f64,
        button: u32,
        click_count: i64,
    },
    Disabled(NativeDisableReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NativeInputTarget {
    pub(crate) pid: i32,
    pub(crate) focused_field: Option<FocusedField>,
}

#[derive(Debug)]
struct RetainedEvent(NonNull<c_void>);

impl Drop for RetainedEvent {
    fn drop(&mut self) {
        // SAFETY: The callback retained this valid CGEventRef exactly once.
        unsafe { CFRelease(self.0.as_ptr().cast_const()) };
    }
}

enum QueuedEvent {
    Event {
        event: RetainedEvent,
        input_target: Option<NativeInputTarget>,
        authorization: Option<InputAuthorization>,
        secure_input: bool,
        ime_active: bool,
    },
    Disabled(NativeDisableReason),
}

struct CallbackContext {
    sender: SyncSender<QueuedEvent>,
    dropped_events: Arc<AtomicU64>,
    degraded_operations: Arc<AtomicU64>,
    focused_fields: Option<FocusedFieldTracker>,
    input_authorizations: Option<InputAuthorizationPublisher>,
    ime_state: ImeState,
    secure_input_probe: Option<SecureInputProbe>,
    capture_text_content: bool,
}

pub(crate) struct EventTap {
    tap: NonNull<c_void>,
    source: NonNull<c_void>,
    run_loop: NonNull<c_void>,
    receiver: Receiver<QueuedEvent>,
    _callback: Box<CallbackContext>,
    capture_text_content: bool,
}

pub(crate) struct EventTapConfig {
    pub(crate) queue_capacity: usize,
    pub(crate) mode: EventTapMode,
    pub(crate) dropped_events: Arc<AtomicU64>,
    pub(crate) degraded_operations: Arc<AtomicU64>,
    pub(crate) focused_fields: Option<FocusedFieldTracker>,
    pub(crate) input_authorizations: Option<InputAuthorizationPublisher>,
    pub(crate) ime_state: ImeState,
    pub(crate) secure_input_probe: Option<SecureInputProbe>,
}

impl EventTap {
    pub(crate) fn create(config: EventTapConfig) -> Result<Self, &'static str> {
        let (sender, receiver) = sync_channel(config.queue_capacity);
        let mut callback = Box::new(CallbackContext {
            sender,
            dropped_events: config.dropped_events,
            degraded_operations: config.degraded_operations,
            focused_fields: config.focused_fields,
            input_authorizations: config.input_authorizations,
            ime_state: config.ime_state,
            secure_input_probe: config.secure_input_probe,
            capture_text_content: config.mode.captures_text_content(),
        });
        let mask = event_mask_for(config.mode);
        // SAFETY: callback remains boxed for the complete tap lifetime.
        let tap = unsafe {
            CGEventTapCreate(
                ANNOTATED_SESSION_EVENT_TAP,
                HEAD_INSERT_EVENT_TAP,
                LISTEN_ONLY,
                mask,
                event_callback,
                (&mut *callback as *mut CallbackContext).cast(),
            )
        };
        let tap = NonNull::new(tap).ok_or("CGEventTapCreate returned null")?;
        // SAFETY: tap is a valid +1 CFMachPort.
        let source = unsafe { CFMachPortCreateRunLoopSource(ptr::null(), tap.as_ptr(), 0) };
        let Some(source) = NonNull::new(source) else {
            // SAFETY: balances the owned tap.
            unsafe { CFRelease(tap.as_ptr().cast_const()) };
            return Err("CFMachPortCreateRunLoopSource returned null");
        };
        // SAFETY: creation occurs on the dedicated EventTap thread.
        let run_loop = unsafe { CFRunLoopGetCurrent() };
        let Some(run_loop) = NonNull::new(run_loop.cast_mut()) else {
            // SAFETY: balances both owned objects.
            unsafe {
                CFRelease(source.as_ptr().cast_const());
                CFRelease(tap.as_ptr().cast_const());
            }
            return Err("CFRunLoopGetCurrent returned null");
        };
        // SAFETY: references are valid and the mode is process-static.
        unsafe { CFRunLoopAddSource(run_loop.as_ptr(), source.as_ptr(), kCFRunLoopCommonModes) };
        Ok(Self {
            tap,
            source,
            run_loop,
            receiver,
            _callback: callback,
            capture_text_content: config.mode.captures_text_content(),
        })
    }

    pub(crate) fn enable(&self) {
        // SAFETY: tap is owned by self.
        unsafe { CGEventTapEnable(self.tap.as_ptr(), true) };
    }

    pub(crate) fn is_enabled(&self) -> bool {
        // SAFETY: tap is owned by self.
        unsafe { CGEventTapIsEnabled(self.tap.as_ptr()) }
    }

    pub(crate) fn run_once(&self, timeout: Duration) {
        run_loop_once(timeout);
    }

    pub(crate) fn try_next_event(
        &self,
        text_read_allowed: impl FnOnce(Option<NativeInputTarget>) -> bool,
    ) -> Option<NativeEvent> {
        match self.receiver.try_recv() {
            Ok(QueuedEvent::Disabled(reason)) => Some(NativeEvent::Disabled(reason)),
            Ok(QueuedEvent::Event {
                event,
                input_target,
                authorization,
                secure_input,
                ime_active,
            }) => Some(decode_event(
                &event,
                input_target,
                authorization,
                secure_input,
                ime_active,
                self.capture_text_content,
                text_read_allowed,
            )),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        // SAFETY: drop runs on the owning thread and releases each +1 object once.
        unsafe {
            CFRunLoopRemoveSource(
                self.run_loop.as_ptr(),
                self.source.as_ptr(),
                kCFRunLoopCommonModes,
            );
            CFRunLoopSourceInvalidate(self.source.as_ptr());
            CFMachPortInvalidate(self.tap.as_ptr());
            CFRelease(self.source.as_ptr().cast_const());
            CFRelease(self.tap.as_ptr().cast_const());
        }
    }
}

pub(crate) fn run_loop_once(timeout: Duration) {
    // SAFETY: The mode is process-static and this runs the current thread's RunLoop.
    unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, timeout.as_secs_f64(), 1) };
}

extern "C" fn event_callback(
    _proxy: *mut c_void,
    event_type: u32,
    event: *mut c_void,
    user_info: *mut c_void,
) -> *mut c_void {
    // SAFETY: user_info points to CallbackContext for the tap lifetime.
    let context = unsafe { &*user_info.cast::<CallbackContext>() };
    let queued = match event_type {
        EVENT_DISABLED_TIMEOUT => QueuedEvent::Disabled(NativeDisableReason::Timeout),
        EVENT_DISABLED_USER_INPUT => QueuedEvent::Disabled(NativeDisableReason::UserInput),
        _ => {
            let Some(event) = NonNull::new(event) else {
                return event;
            };
            let occurred_at = Instant::now();
            let secure_input = event_type == EVENT_KEY_DOWN
                && secure_input_active(
                    context.capture_text_content,
                    context.secure_input_probe.as_ref(),
                    &context.degraded_operations,
                );
            let ime_active = event_type == EVENT_KEY_DOWN && context.ime_state.active();
            let input_target = (event_type == EVENT_KEY_DOWN)
                .then(|| input_target(event.as_ptr(), context.focused_fields.as_ref()))
                .flatten();
            if event_type == EVENT_KEY_DOWN {
                trace_target(input_target, secure_input, ime_active);
            }
            let authorization = (event_type == EVENT_KEY_DOWN)
                .then(|| {
                    prepare_input_authorization(context, input_target, occurred_at, secure_input)
                })
                .flatten();
            // SAFETY: retain transfers one reference before the borrowed callback event expires.
            unsafe { CFRetain(event.as_ptr().cast_const()) };
            QueuedEvent::Event {
                input_target,
                authorization,
                event: RetainedEvent(event),
                secure_input,
                ime_active,
            }
        }
    };
    if let Err(error) = context.sender.try_send(queued) {
        let queued = match error {
            TrySendError::Full(queued) | TrySendError::Disconnected(queued) => queued,
        };
        if let QueuedEvent::Event {
            authorization: Some(authorization),
            ..
        } = queued
        {
            authorization.reject();
        }
        crate::trace::trace!(
            "component=eventtap event=callback_queue authorization=rejected reason=queue_full_or_disconnected"
        );
        context.dropped_events.fetch_add(1, Ordering::Relaxed);
    }
    event
}

fn secure_input_active(
    capture_text_content: bool,
    probe: Option<&SecureInputProbe>,
    degraded_operations: &AtomicU64,
) -> bool {
    if !capture_text_content {
        return false;
    }
    let Some(probe) = probe else {
        crate::trace::trace!(
            "component=eventtap event=secure_input_probe enabled=true reason=probe_missing"
        );
        degraded_operations.fetch_add(1, Ordering::Relaxed);
        return true;
    };
    match probe.enabled() {
        Ok(enabled) => enabled,
        Err(error) => {
            crate::trace::trace!(
                "component=eventtap event=secure_input_probe enabled=true error={}",
                secure_input_error(error)
            );
            degraded_operations.fetch_add(1, Ordering::Relaxed);
            true
        }
    }
}

const fn secure_input_error(error: SecureInputProbeError) -> &'static str {
    match error {
        SecureInputProbeError::Disconnected => "disconnected",
        SecureInputProbeError::Timeout => "timeout",
    }
}

fn decode_event(
    event: &RetainedEvent,
    input_target: Option<NativeInputTarget>,
    authorization: Option<InputAuthorization>,
    secure_input: bool,
    ime_active: bool,
    capture_text_content: bool,
    text_read_allowed: impl FnOnce(Option<NativeInputTarget>) -> bool,
) -> NativeEvent {
    // SAFETY: event owns a valid CGEventRef while all fields are read.
    unsafe {
        let raw = event.0.as_ptr();
        match CGEventGetType(raw) {
            EVENT_KEY_DOWN => {
                let flags = CGEventGetFlags(raw);
                let known_text_field = input_target
                    .and_then(|target| target.focused_field)
                    .is_some_and(|field| field.class.is_known_text());
                NativeEvent::Key {
                    observation: KeyObservation {
                        key_code: u16::try_from(CGEventGetIntegerValueField(
                            raw,
                            KEYBOARD_KEYCODE_FIELD,
                        ))
                        .unwrap_or_default(),
                        modifiers: KeyModifiers {
                            cmd: flags & FLAG_COMMAND != 0,
                            shift: flags & FLAG_SHIFT != 0,
                            opt: flags & FLAG_ALTERNATE != 0,
                            ctrl: flags & FLAG_CONTROL != 0,
                            function: flags & FLAG_SECONDARY_FN != 0,
                        },
                        text: should_read_key_text(
                            capture_text_content,
                            secure_input,
                            ime_active,
                            known_text_field,
                            text_read_allowed(input_target),
                        )
                        .then(|| event_text(raw)),
                    },
                    target: input_target,
                    authorization,
                    secure_input,
                    ime_active,
                }
            }
            EVENT_SCROLL_WHEEL => {
                if let Some(authorization) = authorization {
                    authorization.reject();
                }
                NativeEvent::Scroll {
                    vertical: CGEventGetDoubleValueField(raw, SCROLL_FIXED_AXIS_1_FIELD),
                    horizontal: CGEventGetDoubleValueField(raw, SCROLL_FIXED_AXIS_2_FIELD),
                }
            }
            _ => {
                if let Some(authorization) = authorization {
                    authorization.reject();
                }
                let point = CGEventGetLocation(raw);
                NativeEvent::MouseDown {
                    x: point.x,
                    y: point.y,
                    button: u32::try_from(CGEventGetIntegerValueField(
                        raw,
                        MOUSE_BUTTON_NUMBER_FIELD,
                    ))
                    .unwrap_or(u32::MAX),
                    click_count: CGEventGetIntegerValueField(raw, MOUSE_CLICK_STATE_FIELD),
                }
            }
        }
    }
}

const fn should_read_key_text(
    capture_text_content: bool,
    secure_input: bool,
    ime_active: bool,
    known_text_field: bool,
    text_scope_allowed: bool,
) -> bool {
    capture_text_content && !secure_input && !ime_active && known_text_field && text_scope_allowed
}

unsafe fn event_text(event: CfMutableRef) -> String {
    let mut length: c_ulong = 0;
    // SAFETY: null-buffer query is supported for a valid CGEventRef.
    unsafe { CGEventKeyboardGetUnicodeString(event, 0, &mut length, ptr::null_mut()) };
    let Ok(capacity) = usize::try_from(length) else {
        return String::new();
    };
    let mut units = vec![0_u16; capacity];
    if length > 0 {
        // SAFETY: units contains length writable UniChar entries.
        unsafe {
            CGEventKeyboardGetUnicodeString(event, length, &mut length, units.as_mut_ptr());
        }
    }
    units.truncate(usize::try_from(length).unwrap_or(0));
    String::from_utf16_lossy(&units)
}

const fn event_mask(event_type: u32) -> u64 {
    1_u64 << event_type
}

const fn event_mask_for(mode: EventTapMode) -> u64 {
    let input = if mode.captures_input() {
        event_mask(EVENT_KEY_DOWN) | event_mask(EVENT_SCROLL_WHEEL)
    } else {
        0
    };
    let clicks = if mode.captures_clicks() {
        event_mask(EVENT_LEFT_MOUSE_DOWN)
            | event_mask(EVENT_RIGHT_MOUSE_DOWN)
            | event_mask(EVENT_OTHER_MOUSE_DOWN)
    } else {
        0
    };
    input | clicks
}

#[cfg(test)]
mod tests {
    use super::{
        ANNOTATED_SESSION_EVENT_TAP, EVENT_KEY_DOWN, EVENT_LEFT_MOUSE_DOWN, EVENT_OTHER_MOUSE_DOWN,
        EVENT_RIGHT_MOUSE_DOWN, EVENT_SCROLL_WHEEL, event_mask, event_mask_for,
        should_read_key_text,
    };
    use crate::eventtap::EventTapMode;

    #[test]
    fn event_tap_uses_annotated_session_location() {
        assert_eq!(ANNOTATED_SESSION_EVENT_TAP, 2);
    }

    #[test]
    fn ime_active_at_callback_blocks_text_after_layout_switch() {
        let ime_active_at_callback = true;
        let ime_active_during_processing = false;

        let text_was_read = should_read_key_text(true, false, ime_active_at_callback, true, true);

        assert!(!(text_was_read && !ime_active_during_processing));
    }

    #[test]
    fn missing_ax_tracking_blocks_key_text_at_the_callback() {
        assert!(!should_read_key_text(true, false, false, false, true));
        assert!(!should_read_key_text(true, false, false, true, false));
        assert!(should_read_key_text(true, false, false, true, true));
    }

    #[test]
    fn event_tap_modes_select_only_the_required_native_events() {
        let input_mask = event_mask(EVENT_KEY_DOWN) | event_mask(EVENT_SCROLL_WHEEL);
        let click_mask = event_mask(EVENT_LEFT_MOUSE_DOWN)
            | event_mask(EVENT_RIGHT_MOUSE_DOWN)
            | event_mask(EVENT_OTHER_MOUSE_DOWN);

        assert_eq!(
            event_mask_for(EventTapMode::InputOnly {
                capture_text_content: false,
            }),
            input_mask
        );
        assert_eq!(event_mask_for(EventTapMode::ClickOnly), click_mask);
        assert_eq!(
            event_mask_for(EventTapMode::InputAndClicks {
                capture_text_content: true,
            }),
            input_mask | click_mask
        );
    }
}

#[repr(C)]
struct Point {
    x: c_double,
    y: c_double,
}

type EventCallback = extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        mask: u64,
        callback: EventCallback,
        user_info: *mut c_void,
    ) -> CfMutableRef;
    fn CGEventTapEnable(tap: CfMutableRef, enable: bool);
    fn CGEventTapIsEnabled(tap: CfMutableRef) -> bool;
    fn CGEventGetType(event: CfMutableRef) -> u32;
    fn CGEventGetFlags(event: CfMutableRef) -> u64;
    fn CGEventGetLocation(event: CfMutableRef) -> Point;
    fn CGEventGetIntegerValueField(event: CfMutableRef, field: u32) -> i64;
    fn CGEventGetDoubleValueField(event: CfMutableRef, field: u32) -> c_double;
    fn CGEventKeyboardGetUnicodeString(
        event: CfMutableRef,
        maximum_length: c_ulong,
        actual_length: *mut c_ulong,
        string: *mut u16,
    );
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: CfRef;
    static kCFRunLoopCommonModes: CfRef;
    fn CFRetain(value: CfRef) -> CfRef;
    fn CFRelease(value: CfRef);
    fn CFMachPortCreateRunLoopSource(
        allocator: CfRef,
        port: CfMutableRef,
        order: isize,
    ) -> CfMutableRef;
    fn CFMachPortInvalidate(port: CfMutableRef);
    fn CFRunLoopGetCurrent() -> CfRef;
    fn CFRunLoopAddSource(run_loop: CfRef, source: CfRef, mode: CfRef);
    fn CFRunLoopRemoveSource(run_loop: CfRef, source: CfRef, mode: CfRef);
    fn CFRunLoopRunInMode(mode: CfRef, seconds: c_double, return_after_handled: u8) -> i32;
    fn CFRunLoopSourceInvalidate(source: CfMutableRef);
}
