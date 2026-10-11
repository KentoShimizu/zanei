//! Input-driven re-resolution of the focused field.
//!
//! Some applications expose a readable focused text field but post neither
//! `AXFocusedUIElementChanged` nor `AXValueChanged` for it. After user input the runtime asks the
//! observer to re-read the application's focused element so such a field enters the same
//! focus lifecycle as a notifying one.

use std::time::Instant;

use time::OffsetDateTime;

use super::AppObserver;
use crate::{
    ax::health::AxRecoverySite,
    ffi::ax::{
        NativeAxError, NativeAxEvent, NativeAxObservation, cf::OwnedCf, element::copy_element,
    },
    text_capture::InputAuthorizations,
};

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

/// Announcement state of one focused-target generation. A new generation starts as
/// `Internal` unless the install path records how it was announced.
#[derive(Default)]
pub(super) struct FocusPullState {
    generation: u64,
    announcement: Announcement,
}

impl FocusPullState {
    /// Starts the state of a newly installed target generation.
    pub(super) fn installed(&mut self, generation: u64, announcement: Announcement) {
        *self = Self {
            generation,
            announcement,
        };
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
        let announcement = self.announcement(generation);
        let previous = *announcement;
        if source == FocusSource::Notification {
            *announcement = Announcement::Native;
        }
        source == FocusSource::Internal || previous != Announcement::Sampled || class_changed
    }

    /// The current target was resolved again by a pull. Returns whether the focus is announced
    /// publicly; otherwise it is only re-published internally.
    fn pulled(&mut self, generation: u64, has_target: bool) -> bool {
        let announcement = self.announcement(generation);
        let announce = has_target && *announcement == Announcement::Internal;
        if announce {
            *announcement = Announcement::Sampled;
        }
        announce
    }

    fn announcement(&mut self, generation: u64) -> &mut Announcement {
        if self.generation != generation {
            self.installed(generation, Announcement::Internal);
        }
        &mut self.announcement
    }
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
            now,
            observed_at,
            secure_input,
            authorizations,
        )
    }

    pub(super) fn pull_focused_field_with(
        &mut self,
        trigger: &'static str,
        resolve: impl FnOnce() -> Result<Option<OwnedCf>, NativeAxError>,
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
                trace_pull(pid, trigger, "error");
                self.record_native(AxRecoverySite::FocusedElement, &error);
                return Vec::new();
            }
        };
        if !self.same_value_target(target.as_ref()) {
            trace_pull(pid, trigger, "changed");
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
        trace_pull(pid, trigger, "same");
        self.recover(AxRecoverySite::FocusedElement);
        let event = self.focus_event(observed_at);
        let has_target = self.focused_target.current().is_some();
        if self
            .focus_pull
            .pulled(self.focused_target.generation(), has_target)
        {
            vec![event.into()]
        } else {
            vec![event.internalize_focus()]
        }
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

fn trace_pull(pid: i32, trigger: &'static str, result: &'static str) {
    crate::trace::trace!(
        "component=ax phase=pull action=run pid={} trigger={} read=discovery result={}",
        pid,
        trigger,
        result
    );
}

#[cfg(test)]
mod tests;
