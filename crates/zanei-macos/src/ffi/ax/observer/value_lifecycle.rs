//! Focused and retired AX value lifecycle.

use std::{sync::atomic::Ordering, time::Instant};
use time::OffsetDateTime;

use crate::{
    focused_field::FieldClass,
    text_capture::{FocusChangeCapture, InputAuthorizations},
};

use super::{AppObserver, value_registration::RegistrationError};
use crate::ffi::ax::{
    NativeAxError, NativeAxEvent, add_notification,
    element::{ValueFieldSnapshot, value_field_snapshot, value_snapshot},
    remove_notification,
    value_context::{DeferredResolution, DeferredValueContext, classified_field_snapshot},
};

pub(super) enum FocusChangeResolution {
    Immediate(Option<NativeAxEvent>),
    Deferred,
}

impl FocusChangeResolution {
    pub(super) fn into_parts(self) -> (Option<NativeAxEvent>, bool) {
        match self {
            Self::Immediate(event) => (event, false),
            Self::Deferred => (None, true),
        }
    }
}

impl AppObserver {
    pub(in crate::ffi::ax) fn value_changed_events(
        &mut self,
        notification_at: Instant,
        observed_at: OffsetDateTime,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> Result<Vec<NativeAxEvent>, NativeAxError> {
        let capture_decision = self
            .focused_target
            .current()
            .and_then(|target| self.text_content_decision(target.context.window.as_ref()));
        let capture_text_content = capture_decision
            .as_ref()
            .is_some_and(crate::CaptureDecision::is_allowed);
        let (class_changed, registration_class, value_event) = {
            let Some(target) = self.focused_target.current_mut() else {
                crate::trace::trace!(
                    "component=ax phase=value action=observe pid={} result=target_missing",
                    self.context.pid
                );
                return Ok(Vec::new());
            };
            if !self
                .notifications
                .accepts_delivery(target.element.as_ptr(), "AXValueChanged")
            {
                crate::trace::trace!(
                    "component=ax phase=value action=observe pid={} target_generation={} result=registration_missing",
                    self.context.pid,
                    target.context.generation
                );
                return Ok(Vec::new());
            }
            let context = &mut target.context;
            let previous_class = context.field_class;
            let snapshot =
                value_snapshot(target.element.as_ptr(), capture_text_content, secure_input);
            let registration_class =
                (!secure_input && !snapshot.degraded).then_some(snapshot.field_class);
            crate::trace::trace!(
                "component=ax phase=value action=observe pid={} target_generation={} field_class={} value_len={} degraded={}",
                self.context.pid,
                context.generation,
                crate::trace::field_class_name(snapshot.field_class),
                optional_u64(snapshot.value_len),
                snapshot.degraded
            );
            if snapshot.degraded {
                self.degraded.fetch_add(1, Ordering::Relaxed);
            }
            let observation = context.observation(
                self.context.pid,
                notification_at,
                observed_at,
                snapshot,
                capture_decision,
            );
            let value_event = context
                .capture
                .observe(observation, authorizations)
                .map(|emission| context.value_event(self.context.pid, emission));
            (
                previous_class != context.field_class,
                registration_class,
                value_event,
            )
        };
        let mut events = Vec::new();
        if let Some(field_class) = registration_class {
            self.reconcile_current_value_notification(field_class);
        }
        if class_changed {
            events.push(self.focus_event(observed_at));
        }
        events.extend(value_event);
        Ok(events)
    }

    pub(in crate::ffi::ax) fn take_due_value_events(
        &mut self,
        now: Instant,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> Vec<NativeAxEvent> {
        let pid = self.context.pid;
        let mut events = Vec::new();
        if let Some(context) = self
            .focused_target
            .current_mut()
            .map(|target| &mut target.context)
        {
            if secure_input {
                context.suppress(pid, FieldClass::SecureText, authorizations);
            } else if let Some(emission) = context.capture.take_due(now, authorizations) {
                crate::trace::trace!(
                    "component=ax phase=value action=take_due pid={} target_generation={} result=emitted",
                    pid,
                    context.generation
                );
                events.push(context.value_event(pid, emission));
            }
        }
        let mut pending = Vec::with_capacity(self.retired_contexts.len());
        for context in self.retired_contexts.drain(..) {
            let mut context = DeferredValueContext::new(pid, context);
            match context.take_due(now, secure_input, authorizations) {
                DeferredResolution::Pending => pending.push(context.into_context()),
                DeferredResolution::Complete(Some(event)) => events.push(event),
                DeferredResolution::Complete(None) => {}
            }
        }
        self.retired_contexts = pending;
        events
    }

    pub(in crate::ffi::ax) fn flush_pending(
        &mut self,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> Vec<NativeAxEvent> {
        let pid = self.context.pid;
        let mut events = Vec::new();
        if let Some(context) = self
            .focused_target
            .current_mut()
            .map(|target| &mut target.context)
        {
            let had_pending = context.capture.has_pending();
            if secure_input {
                context.suppress(pid, FieldClass::SecureText, authorizations);
            } else if let Some(emission) = context.capture.flush_pending(authorizations) {
                crate::trace::trace!(
                    "component=ax phase=value action=flush pid={} target_generation={} result=emitted",
                    pid,
                    context.generation
                );
                events.push(context.value_event(pid, emission));
            } else if had_pending {
                crate::trace::trace!(
                    "component=ax phase=value action=flush pid={} target_generation={} result=suppressed",
                    pid,
                    context.generation
                );
            }
        }
        for context in self.retired_contexts.drain(..) {
            let mut context = DeferredValueContext::new(pid, context);
            let event = context.flush(secure_input, authorizations);
            events.extend(event);
        }
        events
    }

    pub(in crate::ffi::ax) fn detach_values(
        &mut self,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> (Vec<NativeAxEvent>, Vec<DeferredValueContext>) {
        if let Ok(previous) = self.focused_target.transition::<NativeAxError>(Ok(None)) {
            crate::trace::trace!(
                "component=ax phase=focus_target action=clear pid={} target_generation={} reason=detach",
                self.context.pid,
                self.focused_target.generation()
            );
            self.remove_focused_target(previous, true);
        }
        let pid = self.context.pid;
        let mut immediate = Vec::new();
        let mut deferred = Vec::new();
        for context in self.retired_contexts.drain(..) {
            let mut context = DeferredValueContext::new(pid, context);
            if !secure_input && context.authorization_is_pending() {
                deferred.push(context);
            } else {
                immediate.extend(context.flush(secure_input, authorizations));
            }
        }
        crate::trace::trace!(
            "component=ax phase=value action=detach pid={} immediate={} deferred={}",
            pid,
            immediate.len(),
            deferred.len()
        );
        (immediate, deferred)
    }

    pub(super) fn refresh_current_field_class(
        &mut self,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> bool {
        let pid = self.context.pid;
        let Some((element, generation, previous_class, was_active)) =
            self.focused_target.current().map(|target| {
                (
                    target.element.as_ptr(),
                    target.context.generation,
                    target.context.field_class,
                    self.notifications
                        .accepts_delivery(target.element.as_ptr(), "AXValueChanged"),
                )
            })
        else {
            return false;
        };
        let snapshot = value_field_snapshot(element, secure_input);
        crate::trace::trace!(
            "component=ax phase=value action=refresh_class pid={} target_generation={} field_class={} degraded={}",
            self.context.pid,
            generation,
            crate::trace::field_class_name(snapshot.field_class),
            snapshot.degraded
        );
        if snapshot.degraded {
            self.degraded.fetch_add(1, Ordering::Relaxed);
        }
        let observer = self.observer.as_ptr();
        let context_pointer = (&raw const *self.context).cast_mut().cast();
        let registration = self.refresh_current_field_class_with(
            snapshot,
            authorizations,
            || add_notification(observer, element, "AXValueChanged", context_pointer),
            || remove_notification(observer, element, "AXValueChanged"),
        );
        let (field_class, is_active) =
            self.focused_target
                .current()
                .map_or((previous_class, false), |target| {
                    (
                        target.context.field_class,
                        self.notifications
                            .accepts_delivery(target.element.as_ptr(), "AXValueChanged"),
                    )
                });
        match registration {
            Err(RegistrationError::Register(error)) => {
                crate::trace::trace!(
                    "component=ax phase=focus_target action=register pid={} target_generation={} registration=error operation={} code={}",
                    pid,
                    generation,
                    error.operation(),
                    error.code()
                );
                self.degraded.fetch_add(1, Ordering::Relaxed);
            }
            Err(RegistrationError::Unregister(error)) => {
                crate::trace::trace!(
                    "component=ax phase=focus_target action=unregister pid={} target_generation={} result=error operation={} code={}",
                    pid,
                    generation,
                    error.operation(),
                    error.code()
                );
                self.degraded.fetch_add(1, Ordering::Relaxed);
            }
            Ok(()) if was_active && !is_active => {
                crate::trace::trace!(
                    "component=ax phase=focus_target action=unregister pid={} target_generation={} result=removed",
                    pid,
                    generation
                );
            }
            Ok(()) if !was_active && is_active => {
                crate::trace::trace!(
                    "component=ax phase=focus_target action=register pid={} target_generation={} registration=registered",
                    pid,
                    generation
                );
            }
            Ok(()) => {}
        }
        previous_class != field_class
    }

    pub(in crate::ffi::ax) fn refresh_current_field_class_with(
        &mut self,
        snapshot: ValueFieldSnapshot,
        authorizations: &mut InputAuthorizations,
        register: impl FnOnce() -> Result<(), NativeAxError>,
        unregister: impl FnOnce() -> Result<(), NativeAxError>,
    ) -> Result<(), RegistrationError> {
        let Some(snapshot) = classified_field_snapshot(snapshot) else {
            return Ok(());
        };
        let field_class = snapshot.field_class;
        let registration = snapshot
            .registration_class
            .map_or(Ok(()), |registration_class| {
                self.reconcile_current_value_notification_with(
                    registration_class,
                    register,
                    unregister,
                )
            });
        if matches!(&registration, Err(RegistrationError::Register(_))) {
            return registration;
        }
        let Some(target) = self.focused_target.current_mut() else {
            return registration;
        };
        target.context.element.role = snapshot.role;
        target.context.element.subrole = snapshot.subrole;
        target.context.field_class = field_class;
        if field_class != FieldClass::KnownSafeNonText {
            target.context.element.value = None;
            target.context.element.value_len = None;
        }
        target.context.capture.transition_class(
            self.context.pid,
            target.context.generation,
            field_class,
            authorizations,
        );
        registration
    }

    fn reconcile_current_value_notification(&mut self, field_class: FieldClass) -> bool {
        let observer = self.observer.as_ptr();
        let context_pointer = (&raw const *self.context).cast_mut().cast();
        let pid = self.context.pid;
        let Some(target) = self.focused_target.current() else {
            return false;
        };
        let was_active = self
            .notifications
            .accepts_delivery(target.element.as_ptr(), "AXValueChanged");
        let element = target.element.as_ptr();
        let generation = target.context.generation;
        let registration = self.reconcile_current_value_notification_with(
            field_class,
            || add_notification(observer, element, "AXValueChanged", context_pointer),
            || remove_notification(observer, element, "AXValueChanged"),
        );
        let is_active = self.focused_target.current().is_some_and(|target| {
            self.notifications
                .accepts_delivery(target.element.as_ptr(), "AXValueChanged")
        });
        match registration {
            Err(RegistrationError::Register(error)) => {
                crate::trace::trace!(
                    "component=ax phase=focus_target action=register pid={} target_generation={} registration=error operation={} code={}",
                    pid,
                    generation,
                    error.operation(),
                    error.code()
                );
                self.degraded.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            Err(RegistrationError::Unregister(error)) => {
                crate::trace::trace!(
                    "component=ax phase=focus_target action=unregister pid={} target_generation={} result=error operation={} code={}",
                    pid,
                    generation,
                    error.operation(),
                    error.code()
                );
                self.degraded.fetch_add(1, Ordering::Relaxed);
            }
            Ok(()) if was_active && !is_active => {
                crate::trace::trace!(
                    "component=ax phase=focus_target action=unregister pid={} target_generation={} result=removed",
                    pid,
                    generation
                );
            }
            Ok(()) if !was_active && is_active => {
                crate::trace::trace!(
                    "component=ax phase=focus_target action=register pid={} target_generation={} registration=registered",
                    pid,
                    generation
                );
            }
            Ok(()) => {}
        }
        true
    }

    pub(in crate::ffi::ax) fn reconcile_current_value_notification_with(
        &mut self,
        field_class: FieldClass,
        register: impl FnOnce() -> Result<(), NativeAxError>,
        unregister: impl FnOnce() -> Result<(), NativeAxError>,
    ) -> Result<(), RegistrationError> {
        let Some(target) = self.focused_target.current() else {
            return Ok(());
        };
        self.notifications.reconcile(
            &target.element,
            "AXValueChanged",
            field_class,
            register,
            unregister,
        )
    }

    pub(super) fn resolve_focus_change(
        &mut self,
        notification_at: Instant,
        observed_at: OffsetDateTime,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> FocusChangeResolution {
        let capture_decision = self
            .focused_target
            .current()
            .and_then(|target| self.text_content_decision(target.context.window.as_ref()));
        let capture_text_content = capture_decision
            .as_ref()
            .is_some_and(crate::CaptureDecision::is_allowed);
        let Some(target) = self.focused_target.current_mut() else {
            return FocusChangeResolution::Immediate(None);
        };
        if !self
            .notifications
            .accepts_delivery(target.element.as_ptr(), "AXValueChanged")
        {
            return FocusChangeResolution::Immediate(None);
        }
        let context = &mut target.context;
        let snapshot = value_snapshot(target.element.as_ptr(), capture_text_content, secure_input);
        if snapshot.degraded {
            self.degraded.fetch_add(1, Ordering::Relaxed);
            return match context
                .capture
                .resolve_unreadable_focus_change(authorizations)
            {
                FocusChangeCapture::Emit(emission) => FocusChangeResolution::Immediate(
                    emission.map(|emission| context.value_event(self.context.pid, emission)),
                ),
                FocusChangeCapture::Defer => {
                    crate::trace::trace!(
                        "component=ax phase=value action=defer pid={} target_generation={} reason=unreadable_focus_change",
                        self.context.pid,
                        context.generation
                    );
                    FocusChangeResolution::Deferred
                }
            };
        }
        let observation = context.observation(
            self.context.pid,
            notification_at,
            observed_at,
            snapshot,
            capture_decision,
        );
        match context
            .capture
            .resolve_focus_change(observation, authorizations)
        {
            FocusChangeCapture::Emit(emission) => FocusChangeResolution::Immediate(
                emission.map(|emission| context.value_event(self.context.pid, emission)),
            ),
            FocusChangeCapture::Defer => {
                crate::trace::trace!(
                    "component=ax phase=value action=defer pid={} target_generation={} reason=pending_authorization",
                    self.context.pid,
                    context.generation
                );
                FocusChangeResolution::Deferred
            }
        }
    }
}

fn optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| value.to_string())
}
