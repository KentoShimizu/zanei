//! Safe ownership boundary around the macOS Accessibility C API.

mod accessibility;
mod cf;
mod element;
mod observer;
mod value_context;

use std::{
    collections::HashMap,
    ffi::c_void,
    fmt, ptr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    time::{Duration, Instant},
};

use crate::{InputAuthorizations, SecureInputProbe, secure_input::SecureInputProbeError};

use cf::{CfRef, OwnedCf, add_current_run_loop_source, cf_string, run_loop_tick, string_value};
use element::{
    create_application, element_at_position, element_role, element_snapshot, set_timeout,
    window_snapshot,
};
use observer::AppObserver;
use value_context::{DeferredResolution, DeferredValueContext};

const CALLBACK_QUEUE_CAPACITY: usize = 1_024;
const MAX_NOTIFICATIONS_PER_POLL: usize = 1;
const AX_ERROR_SUCCESS: i32 = 0;
const AX_ERROR_ATTRIBUTE_UNSUPPORTED: i32 = -25_205;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeWindow {
    pub(crate) title: Option<String>,
    pub(crate) id: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeElement {
    pub(crate) role: Option<String>,
    pub(crate) subrole: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) value_len: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NativeAxEvent {
    WindowFocused {
        pid: i32,
        window: NativeWindow,
    },
    WindowTitleChanged {
        pid: i32,
        window: NativeWindow,
    },
    UiFocused {
        pid: i32,
        generation: u64,
        window: Option<NativeWindow>,
        element: Option<NativeElement>,
    },
    UiValueChanged {
        pid: i32,
        window: Option<NativeWindow>,
        element: NativeElement,
        text: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeHitTest {
    pub(crate) pid: i32,
    pub(crate) window: Option<NativeWindow>,
    pub(crate) element: NativeElement,
}

#[derive(Debug)]
pub(crate) struct NativeAxError {
    operation: &'static str,
    code: i32,
}

impl fmt::Display for NativeAxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failed with AXError {}",
            self.operation, self.code
        )
    }
}

impl NativeAxError {
    pub(super) const fn operation(&self) -> &'static str {
        self.operation
    }

    pub(super) const fn code(&self) -> i32 {
        self.code
    }

    pub(super) const fn is_attribute_unsupported(&self) -> bool {
        self.code == AX_ERROR_ATTRIBUTE_UNSUPPORTED
    }
}

pub(crate) struct NativeAx {
    observers: HashMap<i32, AppObserver>,
    detached_contexts: Vec<DeferredValueContext>,
    sender: SyncSender<QueuedNotification>,
    receiver: Receiver<QueuedNotification>,
    dropped: Arc<AtomicU64>,
    degraded: Arc<AtomicU64>,
    capture_text_content: bool,
    authorizations: InputAuthorizations,
    secure_input_probe: Option<SecureInputProbe>,
}

impl NativeAx {
    pub(crate) fn new(
        capture_text_content: bool,
        authorizations: InputAuthorizations,
        secure_input_probe: Option<SecureInputProbe>,
    ) -> Self {
        let (sender, receiver) = sync_channel(CALLBACK_QUEUE_CAPACITY);
        Self {
            observers: HashMap::new(),
            detached_contexts: Vec::new(),
            sender,
            receiver,
            dropped: Arc::new(AtomicU64::new(0)),
            degraded: Arc::new(AtomicU64::new(0)),
            capture_text_content,
            authorizations,
            secure_input_probe,
        }
    }

    pub(crate) fn attach(&mut self, pid: i32) -> Result<Vec<NativeAxEvent>, NativeAxError> {
        let secure_input = secure_input_active(
            self.capture_text_content,
            self.secure_input_probe.as_ref(),
            &self.degraded,
            "attach",
        );
        if let Some(observer) = self.observers.get_mut(&pid) {
            return Ok(observer.focused_element_or_clear(
                Instant::now(),
                secure_input,
                &mut self.authorizations,
            ));
        }

        let application = create_application(pid)?;
        set_timeout(application.as_ptr())?;

        let context = Box::new(ObserverContext {
            pid,
            sender: self.sender.clone(),
            dropped: Arc::clone(&self.dropped),
        });
        let observer = create_observer(pid)?;
        let context_pointer = (&raw const *context).cast_mut().cast();
        for notification in ["AXFocusedWindowChanged", "AXFocusedUIElementChanged"] {
            add_notification(
                observer.as_ptr(),
                application.as_ptr(),
                notification,
                context_pointer,
            )?;
        }
        // Some applications do not expose window-created notifications. Focus changes still
        // provide the canonical path for tracking the current window.
        if add_notification(
            observer.as_ptr(),
            application.as_ptr(),
            "AXWindowCreated",
            context_pointer,
        )
        .is_err()
        {
            self.degraded.fetch_add(1, Ordering::Relaxed);
        }

        let source = unsafe { AXObserverGetRunLoopSource(observer.as_ptr()) };
        if source.is_null() {
            return Err(native_error("AXObserverGetRunLoopSource", -1));
        }
        let mut app_observer = AppObserver::new(
            application,
            observer,
            source,
            context,
            Arc::clone(&self.degraded),
            self.capture_text_content,
        );
        app_observer.set_manual_accessibility(true);
        app_observer.refresh_window_target();
        let focused = app_observer.focused_element_or_clear(
            Instant::now(),
            secure_input,
            &mut self.authorizations,
        );
        unsafe { add_current_run_loop_source(source) };
        self.observers.insert(pid, app_observer);
        Ok(focused)
    }

    pub(crate) fn detach(&mut self, pid: i32) -> Vec<NativeAxEvent> {
        let secure_input = secure_input_active(
            self.capture_text_content,
            self.secure_input_probe.as_ref(),
            &self.degraded,
            "detach",
        );
        let mut events = Vec::new();
        if let Some(mut observer) = self.observers.remove(&pid) {
            let (immediate, deferred) =
                observer.detach_values(secure_input, &mut self.authorizations);
            events.extend(immediate);
            self.detached_contexts.extend(deferred);
        }
        self.authorizations.remove_pid(pid);
        events
    }

    pub(crate) fn poll(&mut self, timeout: Duration) -> Vec<NativeAxEvent> {
        run_loop_tick(timeout);
        self.authorizations.receive_pending();
        let queued: Vec<_> = self
            .receiver
            .try_iter()
            .take(MAX_NOTIFICATIONS_PER_POLL)
            .collect();
        let drained = queued.len();
        let mut events = Vec::new();
        for notification in queued {
            let pid = notification.pid;
            crate::trace::trace!(
                "component=ax phase=poll action=drain pid={} queue_age_ms={} drained={}",
                pid,
                Instant::now()
                    .saturating_duration_since(notification.notification_at)
                    .as_millis(),
                drained
            );
            match self.decode(notification) {
                Ok(decoded) => events.extend(decoded),
                Err(error) => {
                    crate::trace::trace!(
                        "component=ax phase=decode action=error pid={} operation={} code={}",
                        pid,
                        error.operation(),
                        error.code()
                    );
                    self.degraded.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let now = Instant::now();
        let secure_input = secure_input_active(
            self.capture_text_content,
            self.secure_input_probe.as_ref(),
            &self.degraded,
            "poll",
        );
        for observer in self.observers.values_mut() {
            events.extend(observer.take_due_value_events(
                now,
                secure_input,
                &mut self.authorizations,
            ));
        }
        let mut pending_detached = Vec::with_capacity(self.detached_contexts.len());
        for mut context in self.detached_contexts.drain(..) {
            match context.take_due(now, secure_input, &mut self.authorizations) {
                DeferredResolution::Pending => pending_detached.push(context),
                DeferredResolution::Complete(Some(event)) => events.push(event),
                DeferredResolution::Complete(None) => {}
            }
        }
        self.detached_contexts = pending_detached;
        events
    }

    pub(crate) fn flush_pending(&mut self) -> Vec<NativeAxEvent> {
        let mut events = Vec::new();
        let secure_input = secure_input_active(
            self.capture_text_content,
            self.secure_input_probe.as_ref(),
            &self.degraded,
            "flush",
        );
        for observer in self.observers.values_mut() {
            events.extend(observer.flush_pending(secure_input, &mut self.authorizations));
        }
        for mut context in self.detached_contexts.drain(..) {
            events.extend(context.flush(secure_input, &mut self.authorizations));
        }
        events
    }

    pub(crate) fn into_authorizations(self) -> InputAuthorizations {
        self.authorizations
    }

    pub(crate) fn hit_test(&self, pid: i32, x: f64, y: f64) -> Option<NativeHitTest> {
        let observer = self.observers.get(&pid)?;
        let element = match element_at_position(observer.application.as_ptr(), x, y) {
            Ok(Some(element)) => element,
            Ok(None) => return None,
            Err(_) => {
                self.degraded.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        match element_snapshot(element.as_ptr(), self.capture_text_content) {
            Ok(Some((window, element))) => Some(NativeHitTest {
                pid,
                window,
                element,
            }),
            Ok(None) => None,
            Err(_) => {
                self.degraded.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub(crate) fn take_dropped_events(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }

    pub(crate) fn take_degraded_operations(&self) -> u64 {
        self.degraded.swap(0, Ordering::Relaxed)
    }

    fn decode(&mut self, queued: QueuedNotification) -> Result<Vec<NativeAxEvent>, NativeAxError> {
        let name = string_value(queued.notification.as_ptr())
            .ok_or_else(|| native_error("AX notification decoding", -1))?;
        let secure_input = matches!(
            name.as_str(),
            "AXFocusedUIElementChanged" | "AXValueChanged"
        ) && secure_input_active(
            self.capture_text_content,
            self.secure_input_probe.as_ref(),
            &self.degraded,
            "decode",
        );
        let Some(observer) = self.observers.get_mut(&queued.pid) else {
            crate::trace::trace!(
                "component=ax phase=decode action=drop pid={} notification={} reason=observer_missing",
                queued.pid,
                name
            );
            return Ok(Vec::new());
        };
        match name.as_str() {
            "AXFocusedWindowChanged" => Ok(observer.focused_window_event()?.into_iter().collect()),
            "AXWindowCreated" => {
                observer.refresh_window_target();
                Ok(Vec::new())
            }
            "AXTitleChanged"
                if observer.is_current_target(TargetKind::Window, queued.element.as_ptr()) =>
            {
                Ok(window_snapshot(queued.element.as_ptr())?
                    .map(|window| NativeAxEvent::WindowTitleChanged {
                        pid: queued.pid,
                        window,
                    })
                    .into_iter()
                    .collect())
            }
            "AXFocusedUIElementChanged" => Ok(observer.focused_element_or_clear(
                queued.notification_at,
                secure_input,
                &mut self.authorizations,
            )),
            "AXValueChanged" => {
                let matched =
                    observer.is_current_target(TargetKind::Value, queued.element.as_ptr());
                crate::trace::trace!(
                    "component=ax phase=decode action=value_target pid={} notification={} target={} element_role={}",
                    queued.pid,
                    name,
                    if matched { "matched" } else { "mismatch" },
                    element_role(queued.element.as_ptr())
                        .as_deref()
                        .unwrap_or("unavailable")
                );
                if matched {
                    observer.value_changed_events(
                        queued.notification_at,
                        secure_input,
                        &mut self.authorizations,
                    )
                } else {
                    Ok(Vec::new())
                }
            }
            _ => Ok(Vec::new()),
        }
    }
}

fn secure_input_active(
    capture_text_content: bool,
    probe: Option<&SecureInputProbe>,
    degraded: &AtomicU64,
    phase: &'static str,
) -> bool {
    if !capture_text_content {
        return false;
    }
    let Some(probe) = probe else {
        crate::trace::trace!(
            "component=ax phase={} action=secure_input_probe enabled=true reason=probe_missing",
            phase
        );
        return true;
    };
    match probe.enabled() {
        Ok(enabled) => {
            crate::trace::trace!(
                "component=ax phase={} action=secure_input_probe enabled={}",
                phase,
                enabled
            );
            enabled
        }
        Err(error) => {
            crate::trace::trace!(
                "component=ax phase={} action=secure_input_probe enabled=true error={}",
                phase,
                secure_input_error(error)
            );
            degraded.fetch_add(1, Ordering::Relaxed);
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

struct ObserverContext {
    pid: i32,
    sender: SyncSender<QueuedNotification>,
    dropped: Arc<AtomicU64>,
}

#[derive(Clone, Copy)]
enum TargetKind {
    Window,
    Value,
}

struct QueuedNotification {
    pid: i32,
    element: OwnedCf,
    notification: OwnedCf,
    notification_at: Instant,
}

extern "C" fn observer_callback(
    _observer: CfRef,
    element: CfRef,
    notification: CfRef,
    context: *mut c_void,
) {
    let Some(context) = (unsafe { context.cast::<ObserverContext>().as_ref() }) else {
        crate::trace::trace!(
            "component=ax phase=callback action=drop pid=unknown reason=context_missing"
        );
        return;
    };
    let element = unsafe { OwnedCf::retain(element) };
    let notification = unsafe { OwnedCf::retain(notification) };
    let (Some(element), Some(notification)) = (element, notification) else {
        crate::trace::trace!(
            "component=ax phase=callback action=drop pid={} reason=retain_failed",
            context.pid
        );
        return;
    };
    match context.sender.try_send(QueuedNotification {
        pid: context.pid,
        element,
        notification,
        notification_at: Instant::now(),
    }) {
        Ok(()) => {
            crate::trace::trace!(
                "component=ax phase=callback action=enqueue pid={}",
                context.pid
            );
        }
        Err(TrySendError::Full(_)) => {
            crate::trace::trace!(
                "component=ax phase=callback action=drop pid={} reason=queue_full",
                context.pid
            );
            context.dropped.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {
            crate::trace::trace!(
                "component=ax phase=callback action=drop pid={} reason=queue_disconnected",
                context.pid
            );
            context.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn create_observer(pid: i32) -> Result<OwnedCf, NativeAxError> {
    let mut observer = ptr::null();
    let status = unsafe { AXObserverCreate(pid, observer_callback, &raw mut observer) };
    if status != AX_ERROR_SUCCESS {
        return Err(native_error("AXObserverCreate", status));
    }
    unsafe { OwnedCf::from_create(observer) }.ok_or_else(|| native_error("AXObserverCreate", -1))
}

fn add_notification(
    observer: CfRef,
    element: CfRef,
    notification: &str,
    context: *mut c_void,
) -> Result<(), NativeAxError> {
    let notification =
        cf_string(notification).ok_or_else(|| native_error("CFStringCreateWithCString", -1))?;
    let status =
        unsafe { AXObserverAddNotification(observer, element, notification.as_ptr(), context) };
    if status == AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(native_error("AXObserverAddNotification", status))
    }
}

fn remove_notification(
    observer: CfRef,
    element: CfRef,
    notification: &str,
) -> Result<(), NativeAxError> {
    let notification =
        cf_string(notification).ok_or_else(|| native_error("CFStringCreateWithCString", -1))?;
    let status = unsafe { AXObserverRemoveNotification(observer, element, notification.as_ptr()) };
    if status == AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(native_error("AXObserverRemoveNotification", status))
    }
}

const fn native_error(operation: &'static str, code: i32) -> NativeAxError {
    NativeAxError { operation, code }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXObserverCreate(
        pid: i32,
        callback: extern "C" fn(CfRef, CfRef, CfRef, *mut c_void),
        observer: *mut CfRef,
    ) -> i32;
    fn AXObserverAddNotification(
        observer: CfRef,
        element: CfRef,
        notification: CfRef,
        context: *mut c_void,
    ) -> i32;
    fn AXObserverRemoveNotification(observer: CfRef, element: CfRef, notification: CfRef) -> i32;
    fn AXObserverGetRunLoopSource(observer: CfRef) -> CfRef;
}

#[cfg(test)]
mod tests;
