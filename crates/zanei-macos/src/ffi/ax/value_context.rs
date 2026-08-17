//! Focused and detached AX value-capture contexts.

use std::time::Instant;

use crate::{
    focused_field::FieldClass,
    text_capture::{InputAuthorizations, ValueCapture, ValueEmission, ValueObservation},
};

use super::{
    NativeAxEvent, NativeElement, NativeWindow,
    element::{ValueFieldSnapshot, ValueSnapshot},
};

pub(super) fn after_target_preparation<T, E, R>(
    prepared: Result<T, E>,
    resolve_previous: impl FnOnce() -> R,
) -> Result<(T, R), E> {
    let prepared = prepared?;
    Ok((prepared, resolve_previous()))
}

pub(super) fn classified_field_snapshot(
    snapshot: ValueFieldSnapshot,
) -> Option<ValueFieldSnapshot> {
    (!snapshot.degraded).then_some(snapshot)
}

pub(super) struct FocusedValueContext {
    pub(super) window: Option<NativeWindow>,
    pub(super) element: NativeElement,
    pub(super) capture: ValueCapture,
    pub(super) generation: u64,
    pub(super) field_class: FieldClass,
}

impl FocusedValueContext {
    pub(super) fn new(
        window: Option<NativeWindow>,
        element: NativeElement,
        capture_text_content: bool,
        text_baseline: Option<String>,
        generation: u64,
        field_class: FieldClass,
    ) -> Self {
        Self {
            window,
            element,
            capture: ValueCapture::new(capture_text_content, text_baseline, field_class),
            generation,
            field_class,
        }
    }

    pub(super) fn suppress(
        &mut self,
        pid: i32,
        field_class: FieldClass,
        authorizations: &mut InputAuthorizations,
    ) {
        if self.field_class != field_class || self.capture.has_pending() {
            crate::trace::trace!(
                "component=ax phase=value action=suppress pid={} target_generation={} field_class={} pending={}",
                pid,
                self.generation,
                crate::trace::field_class_name(field_class),
                self.capture.has_pending()
            );
        }
        self.element.role = None;
        self.element.subrole = None;
        self.element.value = None;
        self.element.value_len = None;
        self.field_class = field_class;
        self.capture
            .transition_class(pid, self.generation, field_class, authorizations);
    }

    pub(super) fn observation(
        &mut self,
        pid: i32,
        notification_at: Instant,
        snapshot: ValueSnapshot,
    ) -> ValueObservation {
        self.element.role = snapshot.role;
        self.element.subrole = snapshot.subrole;
        self.element.value = (snapshot.field_class == FieldClass::KnownSafeNonText)
            .then(|| snapshot.value.clone())
            .flatten();
        self.element.value_len = snapshot.value_len;
        self.field_class = snapshot.field_class;
        ValueObservation {
            pid,
            target_generation: self.generation,
            notification_at,
            value: snapshot.value,
            value_len: snapshot.value_len,
            field_class: snapshot.field_class,
        }
    }

    pub(super) fn value_event(&self, pid: i32, emission: ValueEmission) -> NativeAxEvent {
        let mut element = self.element.clone();
        element.value = emission.element_value;
        element.value_len = emission.value_len;
        NativeAxEvent::UiValueChanged {
            pid,
            window: self.window.clone(),
            element,
            text: emission.text,
        }
    }
}

pub(super) struct DeferredValueContext {
    pid: i32,
    context: FocusedValueContext,
}

pub(super) enum DeferredResolution {
    Pending,
    Complete(Option<NativeAxEvent>),
}

impl DeferredValueContext {
    pub(super) fn new(pid: i32, context: FocusedValueContext) -> Self {
        Self { pid, context }
    }

    pub(super) fn into_context(self) -> FocusedValueContext {
        self.context
    }

    pub(super) fn authorization_is_pending(&self) -> bool {
        self.context.capture.pending_authorization_is_pending()
    }

    pub(super) fn take_due(
        &mut self,
        now: Instant,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> DeferredResolution {
        if secure_input {
            self.context
                .suppress(self.pid, FieldClass::SecureText, authorizations);
            crate::trace::trace!(
                "component=ax phase=value action=take_due pid={} target_generation={} result=suppressed",
                self.pid,
                self.context.generation
            );
            return DeferredResolution::Complete(None);
        }
        if let Some(emission) = self.context.capture.take_due(now, authorizations) {
            crate::trace::trace!(
                "component=ax phase=value action=take_due pid={} target_generation={} result=emitted_retired",
                self.pid,
                self.context.generation
            );
            return DeferredResolution::Complete(Some(
                self.context.value_event(self.pid, emission),
            ));
        }
        if self.context.capture.has_pending() {
            crate::trace::trace!(
                "component=ax phase=value action=take_due pid={} target_generation={} result=defer",
                self.pid,
                self.context.generation
            );
            DeferredResolution::Pending
        } else {
            crate::trace::trace!(
                "component=ax phase=value action=take_due pid={} target_generation={} result=retired",
                self.pid,
                self.context.generation
            );
            DeferredResolution::Complete(None)
        }
    }

    pub(super) fn flush(
        &mut self,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> Option<NativeAxEvent> {
        if secure_input {
            self.context
                .suppress(self.pid, FieldClass::SecureText, authorizations);
            crate::trace::trace!(
                "component=ax phase=value action=flush pid={} target_generation={} result=suppressed",
                self.pid,
                self.context.generation
            );
            return None;
        }
        let event = self
            .context
            .capture
            .flush_pending(authorizations)
            .map(|emission| self.context.value_event(self.pid, emission));
        crate::trace::trace!(
            "component=ax phase=value action=flush pid={} target_generation={} result={}",
            self.pid,
            self.context.generation,
            if event.is_some() {
                "emitted_retired"
            } else {
                "retired"
            }
        );
        event
    }
}
