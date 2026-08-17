//! Debounced value capture with fail-closed authorization aggregation.

use std::time::{Duration, Instant};

use zanei_core::text_delta::text_delta;

use super::authorization::{AUTHORIZATION_QUEUE_CAPACITY, InputAuthorizations, InputWindowMatch};
use crate::{focused_field::FieldClass, trace};

pub(crate) const VALUE_DEBOUNCE: Duration = Duration::from_secs(1);
pub(crate) const VALUE_MAX_HOLD: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValueEmission {
    pub(crate) element_value: Option<String>,
    pub(crate) value_len: Option<u64>,
    pub(crate) text: Option<String>,
}

pub(crate) struct ValueObservation {
    pub(crate) pid: i32,
    pub(crate) target_generation: u64,
    pub(crate) notification_at: Instant,
    pub(crate) value: Option<String>,
    pub(crate) value_len: Option<u64>,
    pub(crate) field_class: FieldClass,
}

pub(crate) enum FocusChangeCapture {
    Emit(Option<ValueEmission>),
    Defer,
}

pub(crate) struct ValueCapture {
    capture_text_content: bool,
    baseline: Option<String>,
    pending: Option<PendingValue>,
    requires_rebaseline: bool,
}

struct PendingValue {
    first_observed_at: Instant,
    last_observed_at: Instant,
    value: Option<String>,
    value_len: Option<u64>,
    field_class: FieldClass,
    text_steps: Vec<PendingTextStep>,
}

struct PendingTextStep {
    value: Option<String>,
    window_match: Option<InputWindowMatch>,
}

impl ValueCapture {
    pub(crate) fn new(
        capture_text_content: bool,
        baseline: Option<String>,
        field_class: FieldClass,
    ) -> Self {
        let requires_rebaseline =
            capture_text_content && (!field_class.is_known_text() || baseline.is_none());
        Self {
            capture_text_content,
            baseline,
            pending: None,
            requires_rebaseline,
        }
    }

    pub(crate) fn observe(
        &mut self,
        observation: ValueObservation,
        authorizations: &mut InputAuthorizations,
    ) -> Option<ValueEmission> {
        trace::trace!(
            "component=text_capture event=observe pid={} field_class={} value_len={} target_generation={} requires_rebaseline={} pending={}",
            observation.pid,
            trace::field_class_name(observation.field_class),
            optional_u64(observation.value_len),
            observation.target_generation,
            self.requires_rebaseline,
            self.pending.is_some()
        );
        if !observation.field_class.is_known_text() || self.requires_rebaseline {
            self.stage(observation, authorizations);
            return None;
        }
        let due = self.take_due(observation.notification_at, authorizations);
        self.stage(observation, authorizations);
        due
    }

    pub(crate) fn transition_class(
        &mut self,
        pid: i32,
        target_generation: u64,
        field_class: FieldClass,
        authorizations: &mut InputAuthorizations,
    ) {
        if !field_class.is_known_text() {
            trace::trace!(
                "component=text_capture event=boundary pid={} field_class={} target_generation={} reason=class_transition",
                pid,
                trace::field_class_name(field_class),
                target_generation
            );
            self.discard_text_boundary(pid, target_generation, authorizations);
        }
    }

    pub(crate) fn take_due(
        &mut self,
        now: Instant,
        authorizations: &mut InputAuthorizations,
    ) -> Option<ValueEmission> {
        let pending = self.pending.as_ref()?;
        let reason = pending.due_reason(now)?;
        trace::trace!(
            "component=text_capture event=pending_due field_class={} value_len={} observation_count={} reason={}",
            trace::field_class_name(pending.field_class),
            optional_u64(pending.value_len),
            pending.text_steps.len(),
            reason
        );
        self.flush_pending(authorizations)
    }

    pub(crate) fn resolve_focus_change(
        &mut self,
        observation: ValueObservation,
        authorizations: &mut InputAuthorizations,
    ) -> FocusChangeCapture {
        if observation.field_class.is_known_text()
            && self.pending.as_ref().is_some_and(|pending| {
                pending.field_class.is_known_text() && pending.value == observation.value
            })
        {
            return if self.pending_authorization_is_pending() {
                trace::trace!(
                    "component=text_capture event=focus_resolution pid={} target_generation={} pending=defer reason=authorization_pending",
                    observation.pid,
                    observation.target_generation
                );
                FocusChangeCapture::Defer
            } else {
                FocusChangeCapture::Emit(self.flush_pending(authorizations))
            };
        }
        if !observation.field_class.is_known_text() || self.requires_rebaseline {
            self.stage(observation, authorizations);
            return FocusChangeCapture::Emit(None);
        }
        let had_pending = self.pending.is_some();
        let value_changed = self.capture_text_content
            && self
                .baseline
                .as_deref()
                .zip(observation.value.as_deref())
                .is_some_and(|(baseline, current)| baseline != current);
        if !had_pending && !value_changed {
            return FocusChangeCapture::Emit(None);
        }
        let pid = observation.pid;
        let target_generation = observation.target_generation;
        self.stage(observation, authorizations);
        if self.pending_authorization_is_pending() {
            trace::trace!(
                "component=text_capture event=focus_resolution pid={} target_generation={} pending=defer reason=authorization_pending",
                pid,
                target_generation
            );
            FocusChangeCapture::Defer
        } else {
            FocusChangeCapture::Emit(self.flush_pending(authorizations))
        }
    }

    pub(crate) fn resolve_unreadable_focus_change(
        &mut self,
        authorizations: &mut InputAuthorizations,
    ) -> FocusChangeCapture {
        if self.pending_authorization_is_pending() {
            trace::trace!(
                "component=text_capture event=focus_resolution pending=defer reason=focus_snapshot_unreadable"
            );
            FocusChangeCapture::Defer
        } else {
            FocusChangeCapture::Emit(self.flush_pending(authorizations))
        }
    }

    pub(crate) fn flush_pending(
        &mut self,
        authorizations: &mut InputAuthorizations,
    ) -> Option<ValueEmission> {
        authorizations.receive_pending();
        let pending = self.pending.take()?;
        let emission = self.emit(pending);
        trace::trace!(
            "component=text_capture event=flush field_class={} value_len={} text_present={} baseline_advanced=true",
            trace::field_class_name(emission.0),
            optional_u64(emission.1.value_len),
            emission.1.text.is_some()
        );
        Some(emission.1)
    }

    pub(crate) fn pending_authorization_is_pending(&self) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            pending.text_steps.iter().any(|step| {
                step.window_match
                    .as_ref()
                    .is_some_and(InputWindowMatch::is_pending)
            })
        })
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn stage(&mut self, observation: ValueObservation, authorizations: &mut InputAuthorizations) {
        if !observation.field_class.is_known_text() {
            self.discard_text_boundary(
                observation.pid,
                observation.target_generation,
                authorizations,
            );
            if matches!(
                observation.field_class,
                FieldClass::SecureText | FieldClass::Unknown
            ) {
                trace::trace!(
                    "component=text_capture event=stage pid={} field_class={} value_len={} target_generation={} pending=discarded reason=private_or_unknown_boundary",
                    observation.pid,
                    trace::field_class_name(observation.field_class),
                    optional_u64(observation.value_len),
                    observation.target_generation
                );
                return;
            }
            self.pending = Some(PendingValue::new(observation, None));
            return;
        }
        if self.requires_rebaseline {
            self.discard_pending();
            authorizations.invalidate(observation.pid, observation.target_generation);
            self.baseline = observation.value.clone();
            self.requires_rebaseline = false;
            trace::trace!(
                "component=text_capture event=stage pid={} field_class={} value_len={} target_generation={} authorization=invalidated baseline_advanced=true reason=rebaseline",
                observation.pid,
                trace::field_class_name(observation.field_class),
                optional_u64(observation.value_len),
                observation.target_generation
            );
            self.pending = Some(PendingValue::new(observation, None));
            return;
        }
        let window_match = self
            .capture_text_content
            .then(|| {
                authorizations.matching(
                    observation.pid,
                    observation.target_generation,
                    observation.notification_at,
                )
            })
            .flatten();
        if let Some(pending) = self
            .pending
            .as_mut()
            .filter(|pending| pending.field_class.is_known_text())
        {
            pending.coalesce(observation, window_match);
            trace::trace!(
                "component=text_capture event=stage pending=replaced observation_count={} baseline_advanced=false reason=debounce_coalesce",
                pending.text_steps.len()
            );
        } else {
            let pending = PendingValue::new(observation, window_match);
            trace::trace!(
                "component=text_capture event=stage pending=created observation_count={} baseline_advanced=false reason=value_observed",
                pending.text_steps.len()
            );
            self.pending = Some(pending);
        }
    }

    fn discard_text_boundary(
        &mut self,
        pid: i32,
        target_generation: u64,
        authorizations: &mut InputAuthorizations,
    ) {
        self.discard_pending();
        self.baseline = None;
        self.requires_rebaseline = self.capture_text_content;
        authorizations.invalidate(pid, target_generation);
    }

    fn discard_pending(&mut self) {
        self.pending = None;
    }

    fn emit(&mut self, mut pending: PendingValue) -> (FieldClass, ValueEmission) {
        let mut authorized_baseline = self.baseline.clone();
        let mut authorized_suffix = false;
        for step in &mut pending.text_steps {
            let authorized = step
                .window_match
                .take()
                .is_some_and(|window_match| window_match.resolve_for_flush());
            if authorized {
                authorized_suffix = true;
            } else {
                authorized_baseline = step.value.clone();
                authorized_suffix = false;
            }
        }
        let text =
            (self.capture_text_content && pending.field_class.is_known_text() && authorized_suffix)
                .then(|| text_delta(authorized_baseline.as_deref()?, pending.value.as_deref()?))
                .flatten();
        let element_value = (self.capture_text_content
            && pending.field_class == FieldClass::KnownSafeNonText)
            .then(|| pending.value.clone())
            .flatten();
        if pending.field_class.is_known_text() {
            self.baseline = pending.value;
        }
        (
            pending.field_class,
            ValueEmission {
                element_value,
                value_len: pending.value_len,
                text,
            },
        )
    }
}

impl PendingValue {
    fn new(observation: ValueObservation, window_match: Option<InputWindowMatch>) -> Self {
        let text_steps = if observation.field_class.is_known_text() {
            vec![PendingTextStep {
                value: observation.value.clone(),
                window_match,
            }]
        } else {
            Vec::new()
        };
        Self {
            first_observed_at: observation.notification_at,
            last_observed_at: observation.notification_at,
            value: observation.value,
            value_len: observation.value_len,
            field_class: observation.field_class,
            text_steps,
        }
    }

    fn coalesce(&mut self, observation: ValueObservation, window_match: Option<InputWindowMatch>) {
        self.last_observed_at = observation.notification_at;
        self.value = observation.value.clone();
        self.value_len = observation.value_len;
        self.field_class = observation.field_class;
        if self.text_steps.len() == AUTHORIZATION_QUEUE_CAPACITY {
            self.text_steps.clear();
            self.text_steps.push(PendingTextStep {
                value: self.value.clone(),
                window_match: None,
            });
            crate::trace::trace!(
                "component=text_capture event=stage pending=reset observation_count={} reason=pending_capacity",
                AUTHORIZATION_QUEUE_CAPACITY
            );
        }
        self.text_steps.push(PendingTextStep {
            value: observation.value,
            window_match,
        });
    }

    fn due_reason(&self, now: Instant) -> Option<&'static str> {
        let idle_due_at = self.last_observed_at + VALUE_DEBOUNCE;
        let max_hold_due_at = self.first_observed_at + VALUE_MAX_HOLD;
        let (due_at, reason) = if idle_due_at <= max_hold_due_at {
            (idle_due_at, "debounce_elapsed")
        } else {
            (max_hold_due_at, "max_hold_elapsed")
        };
        (now >= due_at).then_some(reason)
    }
}

fn optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| value.to_string())
}
