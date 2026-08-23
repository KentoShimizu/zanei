//! Observer-owning AX runtime.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    time::{Duration, Instant},
};

use crate::{
    InputAuthorizations, SecureInputProbe, secure_input::SecureInputProbeError,
    text_capture::TextContentPolicy,
};
use time::OffsetDateTime;
use zanei_core::schema::App;

use super::{
    AXObserverGetRunLoopSource, ManualAccessibilityPolicy, ObserverContext, QueuedNotification,
    TargetKind, add_notification,
    cf::{add_current_run_loop_source, run_loop_tick, string_value},
    create_observer,
    element::{
        create_application, element_at_position, element_role, element_snapshot, set_timeout,
        window_snapshot,
    },
    native_error,
    observer::AppObserver,
    types::{NativeAxError, NativeAxEvent, NativeHitTest},
    value_context::{DeferredResolution, DeferredValueContext},
};

const CALLBACK_QUEUE_CAPACITY: usize = 1_024;
const MAX_NOTIFICATIONS_PER_POLL: usize = 1;

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
    text_policy: TextContentPolicy,
}

impl NativeAx {
    pub(crate) fn new(
        capture_text_content: bool,
        authorizations: InputAuthorizations,
        secure_input_probe: Option<SecureInputProbe>,
        text_policy: TextContentPolicy,
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
            text_policy,
        }
    }

    pub(crate) fn attach(
        &mut self,
        pid: i32,
        app: App,
        manual_accessibility: bool,
    ) -> Result<Vec<NativeAxEvent>, NativeAxError> {
        let secure_input = secure_input_active(
            self.capture_text_content,
            self.secure_input_probe.as_ref(),
            &self.degraded,
            "attach",
        );
        if let Some(observer) = self.observers.get_mut(&pid) {
            observer.update_attach(app, manual_accessibility);
            return Ok(observer.focused_element_or_clear(
                Instant::now(),
                OffsetDateTime::now_utc(),
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

        // SAFETY: the observer is a live +1 AXObserver owned by this runtime.
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
            app,
            self.text_policy.clone(),
            manual_accessibility,
        );
        app_observer.set_manual_accessibility(true);
        app_observer.refresh_window_target();
        let focused = app_observer.focused_element_or_clear(
            Instant::now(),
            OffsetDateTime::now_utc(),
            secure_input,
            &mut self.authorizations,
        );
        // SAFETY: source remains owned by app_observer until it is detached.
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

    pub(crate) fn focused_window(
        &mut self,
        pid: i32,
    ) -> Result<Option<super::NativeWindow>, NativeAxError> {
        let Some(observer) = self.observers.get_mut(&pid) else {
            return Ok(None);
        };
        Ok(observer
            .focused_window_event(OffsetDateTime::now_utc())?
            .map(|event| match event {
                NativeAxEvent::WindowFocused { window, .. } => window,
                NativeAxEvent::WindowTitleChanged { .. }
                | NativeAxEvent::UiFocused { .. }
                | NativeAxEvent::UiValueChanged { .. } => unreachable!(),
            }))
    }

    pub(crate) fn reconcile_manual_accessibility(&mut self, policy: &ManualAccessibilityPolicy) {
        for observer in self.observers.values_mut() {
            let allowed = policy.allows(observer.app());
            observer.reconcile_manual_accessibility(allowed);
        }
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
        match element_snapshot(element.as_ptr(), |window| {
            observer.text_content_allowed(window)
        }) {
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
            "AXFocusedWindowChanged" => Ok(observer
                .focused_window_event(queued.observed_at)?
                .into_iter()
                .collect()),
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
                        observed_at: queued.observed_at,
                    })
                    .into_iter()
                    .collect())
            }
            "AXFocusedUIElementChanged" => Ok(observer.focused_element_or_clear(
                queued.notification_at,
                queued.observed_at,
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
                        queued.observed_at,
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

pub(super) fn secure_input_active(
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
