//! Application accessibility activation and delayed focused-field reconciliation.

use std::time::Instant;

use time::OffsetDateTime;

use crate::{
    focused_field::{FocusedField, field_class},
    text_capture::InputAuthorizations,
};

use super::AppObserver;
use crate::ffi::ax::{NativeAxEvent, NativeAxObservation, element::element_role};

impl AppObserver {
    pub(in crate::ffi::ax) fn activate_accessibility_tree(&mut self, now: Instant) {
        self.accessibility_activation.schedule_reconcile(now);
        if let Err(error) = element_role(self.application.as_ptr()) {
            crate::trace::trace!(
                "component=ax phase=attach action=application_role pid={} operation={} code={}",
                self.context.pid,
                error.operation(),
                error.code()
            );
            self.record_degraded();
        }
    }

    pub(in crate::ffi::ax) fn reconcile_accessibility_if_due(
        &mut self,
        now: Instant,
        observed_at: OffsetDateTime,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> Vec<NativeAxObservation> {
        if !self.accessibility_activation.take_due(now) {
            return Vec::new();
        }
        self.focused_element_or_clear(now, observed_at, secure_input, authorizations)
            .into_iter()
            .map(internalize_focus)
            .collect()
    }
}

pub(in crate::ffi::ax) fn internalize_focus(event: NativeAxEvent) -> NativeAxObservation {
    let NativeAxEvent::UiFocused {
        pid,
        generation,
        element,
        ..
    } = event
    else {
        return event.into();
    };
    NativeAxObservation::FocusedFieldObserved {
        pid,
        focused_field: element.as_ref().map(|element| FocusedField {
            generation,
            class: field_class(element.role.as_deref(), element.subrole.as_deref()),
        }),
    }
}
