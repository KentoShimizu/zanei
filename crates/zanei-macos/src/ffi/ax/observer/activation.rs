//! Application accessibility activation and delayed focused-field reconciliation.

use std::time::Instant;

use time::OffsetDateTime;

use crate::text_capture::InputAuthorizations;

use super::AppObserver;
use crate::ax::health::AxRecoverySite;
use crate::ffi::ax::{NativeAxEvent, NativeAxObservation, element::element_role};

impl AppObserver {
    pub(in crate::ffi::ax) fn activate_accessibility_tree(&mut self, now: Instant) {
        self.accessibility_activation.schedule_reconcile(now);
        match element_role(self.application.as_ptr()) {
            Ok(_) => self.recover(AxRecoverySite::ApplicationRole),
            Err(error) => {
                crate::trace::trace!(
                    "component=ax phase=attach action=application_role pid={} operation={} code={}",
                    self.context.pid,
                    error.operation(),
                    error.code()
                );
                self.record_native(AxRecoverySite::ApplicationRole, &error);
            }
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
            .map(NativeAxEvent::internalize_focus)
            .collect()
    }
}
