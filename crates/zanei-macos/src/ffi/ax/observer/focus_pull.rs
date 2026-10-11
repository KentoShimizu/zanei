//! Input-driven re-resolution and value sampling of the focused field.
//!
//! Some applications expose a readable focused text field but post neither
//! `AXFocusedUIElementChanged` nor `AXValueChanged` for it. After user input the runtime asks the
//! observer to re-read the application's focused element, and the value of a field that has not
//! proven that it notifies, so such a field enters the same focus and value lifecycle as a
//! notifying one.

use std::time::Instant;

use time::OffsetDateTime;

use super::AppObserver;
use crate::{
    CaptureDecision, CapturePolicy,
    ax::health::AxRecoverySite,
    ffi::ax::{
        NativeAxError, NativeAxEvent, NativeAxObservation, NativeWindow,
        cf::{CfRef, OwnedCf},
        element::{ValueSnapshot, capture_value_snapshot, copy_element},
    },
    focused_field::FieldClass,
    text_capture::InputAuthorizations,
};
use zanei_core::schema::App;

/// The read that produced a value observation of the focused target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ValueRead {
    /// A matched native `AXValueChanged` delivery.
    Notification,
    /// An input-driven read of a field that has not delivered `AXValueChanged`.
    Sample,
}

/// Origin of a focus resolution that is not an input-driven pull.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ffi::ax) enum FocusSource {
    /// Attach or activation reconcile; callers keep the focus event internal.
    Internal,
    /// A native `AXFocusedUIElementChanged` notification.
    Notification,
}

/// How the installed focused target was first announced as public `ui.focus`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Announcement {
    #[default]
    Internal,
    Native,
    Sampled,
}

impl From<FocusSource> for Announcement {
    fn from(source: FocusSource) -> Self {
        match source {
            FocusSource::Internal => Self::Internal,
            FocusSource::Notification => Self::Native,
        }
    }
}

/// Pull state of one focused-target generation. A new generation starts as `Internal` unless
/// the install path records how it was announced, and without a native value delivery.
#[derive(Default)]
pub(super) struct FocusPullState {
    generation: u64,
    announcement: Announcement,
    value_delivered: bool,
}

impl FocusPullState {
    /// Starts the state of a newly installed target generation.
    pub(super) fn installed(&mut self, generation: u64, announcement: Announcement) {
        *self = Self {
            generation,
            announcement,
            value_delivered: false,
        };
    }

    /// Records a matched native `AXValueChanged` delivery: the field notifies, so pulls stop
    /// reading its value for the rest of the generation.
    pub(super) fn value_delivered(&mut self, generation: u64) {
        self.at(generation).value_delivered = true;
    }

    /// The current target was resolved again without a pull. Returns whether the focus event is
    /// emitted: a native notification right after a pull announced the same target is a late
    /// echo and is emitted only when the field class changed.
    pub(super) fn observed(
        &mut self,
        generation: u64,
        source: FocusSource,
        class_changed: bool,
    ) -> bool {
        let announcement = &mut self.at(generation).announcement;
        let previous = *announcement;
        if source == FocusSource::Notification {
            *announcement = Announcement::Native;
        }
        source == FocusSource::Internal || previous != Announcement::Sampled || class_changed
    }

    /// The current target was resolved again by a pull. Returns whether the focus is announced
    /// publicly; otherwise it is only re-published internally.
    fn pulled(&mut self, generation: u64, has_target: bool) -> bool {
        let announcement = &mut self.at(generation).announcement;
        let announce = has_target && *announcement == Announcement::Internal;
        if announce {
            *announcement = Announcement::Sampled;
        }
        announce
    }

    fn at(&mut self, generation: u64) -> &mut Self {
        if self.generation != generation {
            self.installed(generation, Announcement::Internal);
        }
        self
    }
}

/// Reads the value snapshot of the focused element; injected so tests avoid native AX reads.
pub(super) trait CaptureValue:
    FnOnce(
    CfRef,
    &mut Option<NativeWindow>,
    &CapturePolicy,
    &App,
    bool,
    bool,
    &mut dyn FnMut(),
) -> (ValueSnapshot, Option<CaptureDecision>)
{
}

impl<F> CaptureValue for F where
    F: FnOnce(
        CfRef,
        &mut Option<NativeWindow>,
        &CapturePolicy,
        &App,
        bool,
        bool,
        &mut dyn FnMut(),
    ) -> (ValueSnapshot, Option<CaptureDecision>)
{
}

impl AppObserver {
    pub(in crate::ffi::ax) fn pull_focused_field(
        &mut self,
        trigger: &'static str,
        now: Instant,
        observed_at: OffsetDateTime,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> Vec<NativeAxObservation> {
        let application = self.application.as_ptr();
        self.pull_focused_field_with(
            trigger,
            || copy_element(application, "AXFocusedUIElement"),
            |element, window, policy, app, enabled, secure, changed| {
                capture_value_snapshot(element, window, policy, app, enabled, secure, changed)
            },
            now,
            observed_at,
            secure_input,
            authorizations,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn pull_focused_field_with(
        &mut self,
        trigger: &'static str,
        resolve: impl FnOnce() -> Result<Option<OwnedCf>, NativeAxError>,
        capture: impl CaptureValue,
        now: Instant,
        observed_at: OffsetDateTime,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> Vec<NativeAxObservation> {
        let pid = self.context.pid;
        let target = match resolve() {
            Ok(target) => target,
            Err(error) => {
                // A failed read is neither focus loss nor an empty field: keep the target, its
                // generation and any pending value, and retry on the next input.
                trace_pull(pid, trigger, "discovery", "error");
                self.record_native(AxRecoverySite::FocusedElement, &error);
                return Vec::new();
            }
        };
        if !self.same_value_target(target.as_ref()) {
            trace_pull(pid, trigger, "discovery", "changed");
            let events = match self.install_focused_target(
                target,
                Announcement::Sampled,
                now,
                observed_at,
                secure_input,
                authorizations,
            ) {
                Ok(events) => {
                    self.recover(AxRecoverySite::FocusedElement);
                    events
                }
                Err(error) => self.focus_read_failed(&error, observed_at),
            };
            return events.into_iter().map(NativeAxObservation::from).collect();
        }
        trace_pull(pid, trigger, "discovery", "same");
        self.recover(AxRecoverySite::FocusedElement);
        let event = self.focus_event(observed_at);
        let generation = self.focused_target.generation();
        let has_target = self.focused_target.current().is_some();
        let mut observations = vec![if self.focus_pull.pulled(generation, has_target) {
            event.into()
        } else {
            event.internalize_focus()
        }];
        if self.samples_value() {
            let events = self.value_changed_events_with(
                ValueRead::Sample,
                now,
                observed_at,
                secure_input,
                authorizations,
                capture,
                Self::reconcile_current_value_notification,
            );
            trace_pull(
                pid,
                trigger,
                "value",
                if events.is_empty() { "none" } else { "emitted" },
            );
            observations.extend(events.into_iter().map(NativeAxObservation::from));
        }
        observations
    }

    /// Whether a pull reads the current target's value: only until the generation proves that
    /// it notifies. A newly installed target is not sampled; its install read is the baseline.
    /// Safe non-text targets are not read; a text target whose class was degraded by Secure
    /// Input or a failed read is, so the next read can restore it.
    fn samples_value(&mut self) -> bool {
        let generation = self.focused_target.generation();
        self.focused_target
            .current()
            .is_some_and(|target| target.context.field_class != FieldClass::KnownSafeNonText)
            && !self.focus_pull.at(generation).value_delivered
    }

    /// A focus read or install failure clears the target, as focus can no longer be attributed.
    pub(super) fn focus_read_failed(
        &mut self,
        error: &NativeAxError,
        observed_at: OffsetDateTime,
    ) -> Vec<NativeAxEvent> {
        crate::trace::trace!(
            "component=ax phase=focus_target action=error pid={} operation={} code={}",
            self.context.pid,
            error.operation(),
            error.code()
        );
        self.record_native(AxRecoverySite::FocusedElement, error);
        self.clear_focused_target();
        vec![self.focus_event(observed_at)]
    }
}

fn trace_pull(pid: i32, trigger: &'static str, read: &'static str, result: &'static str) {
    crate::trace::trace!(
        "component=ax phase=pull action=run pid={} trigger={} read={} result={}",
        pid,
        trigger,
        read,
        result
    );
}

#[cfg(test)]
mod tests;
