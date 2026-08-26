//! Per-application AX observer state and focused-value lifecycle.

mod activation;
#[cfg(test)]
mod test_support;
mod value_lifecycle;
pub(in crate::ffi::ax) mod value_registration;

use std::{
    sync::{Arc, atomic::AtomicU64},
    time::Instant,
};
use time::OffsetDateTime;

use crate::{
    ax::health::{AxFailurePhase, AxFailurePublisher, AxRecoverySite},
    capture_policy::{CaptureDecision, CapturePolicy},
    focused_field::{FieldClass, field_class},
    text_capture::{FocusedTarget, InputAuthorizations},
};
use zanei_core::{privacy::PrivacyScope, schema::App};

use super::{
    NativeAxError, NativeAxEvent, ObserverContext, TargetKind,
    accessibility::{AccessibilityActivation, set_manual_accessibility},
    add_notification,
    cf::{CfRef, OwnedCf, remove_current_run_loop_source},
    element::{
        FocusedElementSnapshot, cf_equal, copy_element, focused_element_snapshot, window_snapshot,
    },
    remove_notification,
    value_context::{FocusedValueContext, after_target_preparation},
};
use value_registration::{NotificationRegistry, RegistrationError};

pub(super) struct AppObserver {
    pub(super) application: OwnedCf,
    pub(super) observer: OwnedCf,
    pub(super) source: CfRef,
    pub(super) context: Box<ObserverContext>,
    window_target: Option<OwnedCf>,
    focused_target: FocusedTarget<RegisteredFocusedTarget>,
    notifications: NotificationRegistry,
    retired_contexts: Vec<FocusedValueContext>,
    degraded: Arc<AtomicU64>,
    failures: AxFailurePublisher,
    capture_text_content: bool,
    app: App,
    capture_policy: CapturePolicy,
    manual_accessibility: bool,
    accessibility_activation: AccessibilityActivation,
    #[cfg(test)]
    skip_native_cleanup: bool,
}

struct RegisteredFocusedTarget {
    element: OwnedCf,
    context: FocusedValueContext,
}

impl AppObserver {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_attached(
        application: OwnedCf,
        observer: OwnedCf,
        source: CfRef,
        context: Box<ObserverContext>,
        degraded: Arc<AtomicU64>,
        failures: AxFailurePublisher,
        capture_text_content: bool,
        app: App,
        capture_policy: CapturePolicy,
        manual_accessibility: bool,
        attached_at: Instant,
    ) -> Self {
        let mut observer = Self::new(
            application,
            observer,
            source,
            context,
            degraded,
            failures,
            capture_text_content,
            app,
            capture_policy,
            manual_accessibility,
        );
        observer.set_manual_accessibility(true);
        observer.activate_accessibility_tree(attached_at);
        observer
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        application: OwnedCf,
        observer: OwnedCf,
        source: CfRef,
        context: Box<ObserverContext>,
        degraded: Arc<AtomicU64>,
        failures: AxFailurePublisher,
        capture_text_content: bool,
        app: App,
        capture_policy: CapturePolicy,
        manual_accessibility: bool,
    ) -> Self {
        Self {
            application,
            observer,
            source,
            context,
            window_target: None,
            focused_target: FocusedTarget::new(),
            notifications: NotificationRegistry::default(),
            retired_contexts: Vec::new(),
            degraded,
            failures,
            capture_text_content,
            app,
            capture_policy,
            manual_accessibility,
            accessibility_activation: AccessibilityActivation::default(),
            #[cfg(test)]
            skip_native_cleanup: false,
        }
    }

    pub(super) fn set_manual_accessibility(&self, enabled: bool) {
        set_manual_accessibility(
            self.application.as_ptr(),
            self.context.pid,
            self.manual_accessibility,
            enabled,
        );
    }

    pub(super) fn update_attach(&mut self, app: App, manual_accessibility: bool) {
        self.app = app;
        self.reconcile_manual_accessibility(manual_accessibility);
    }

    pub(super) fn app(&self) -> &App {
        &self.app
    }

    pub(super) fn reconcile_manual_accessibility(&mut self, manual_accessibility: bool) {
        if self.manual_accessibility == manual_accessibility {
            return;
        }
        if self.manual_accessibility {
            self.set_manual_accessibility(false);
        }
        self.manual_accessibility = manual_accessibility;
        if self.manual_accessibility {
            self.set_manual_accessibility(true);
        }
    }

    pub(super) fn is_current_target(&self, kind: TargetKind, element: CfRef) -> bool {
        let target = match kind {
            TargetKind::Window => self.window_target.as_ref(),
            TargetKind::Value => self
                .focused_target
                .current()
                .filter(|target| {
                    self.notifications
                        .accepts_delivery(target.element.as_ptr(), "AXValueChanged")
                })
                .map(|target| &target.element),
        };
        target.is_some_and(|target| cf_equal(target.as_ptr(), element))
    }

    pub(super) fn focused_window_event(
        &mut self,
        observed_at: OffsetDateTime,
    ) -> Result<Option<NativeAxEvent>, NativeAxError> {
        let target = copy_element(self.application.as_ptr(), "AXFocusedWindow")?;
        let window_result = target
            .as_ref()
            .map(|element| window_snapshot(element.as_ptr()))
            .transpose();
        match self.replace_window_target(target) {
            Ok(()) => self.recover(AxRecoverySite::FocusedWindow),
            Err(error) => self.record_native(AxRecoverySite::FocusedWindow, &error),
        }
        let window = window_result?.flatten();
        Ok(window.map(|window| NativeAxEvent::WindowFocused {
            pid: self.context.pid,
            window,
            observed_at,
        }))
    }

    fn focused_element_events(
        &mut self,
        notification_at: Instant,
        observed_at: OffsetDateTime,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> Result<Vec<NativeAxEvent>, NativeAxError> {
        let target = copy_element(self.application.as_ptr(), "AXFocusedUIElement")?;
        if self.same_value_target(target.as_ref()) {
            self.refresh_current_field_class(secure_input, authorizations);
            return Ok(vec![self.focus_event(observed_at)]);
        }

        let snapshot = target
            .as_ref()
            .map(|element| {
                focused_element_snapshot(
                    element.as_ptr(),
                    |window| self.text_content_allowed(window),
                    secure_input,
                )
            })
            .transpose()?
            .flatten();
        let prepared = self.prepare_focused_target(target, snapshot);
        let (prepared, focus_change) = after_target_preparation(prepared, || {
            self.resolve_focus_change(notification_at, observed_at, secure_input, authorizations)
        })?;
        let (event, defer_previous) = focus_change.into_parts();
        let mut events = event.into_iter().collect::<Vec<_>>();
        self.commit_focused_target(prepared, defer_previous);
        events.push(self.focus_event(observed_at));
        Ok(events)
    }

    pub(super) fn focused_element_or_clear(
        &mut self,
        notification_at: Instant,
        observed_at: OffsetDateTime,
        secure_input: bool,
        authorizations: &mut InputAuthorizations,
    ) -> Vec<NativeAxEvent> {
        match self.focused_element_events(
            notification_at,
            observed_at,
            secure_input,
            authorizations,
        ) {
            Ok(events) => {
                self.recover(AxRecoverySite::FocusedElement);
                events
            }
            Err(error) => {
                crate::trace::trace!(
                    "component=ax phase=focus_target action=error pid={} operation={} code={}",
                    self.context.pid,
                    error.operation(),
                    error.code()
                );
                self.record_native(AxRecoverySite::FocusedElement, &error);
                self.clear_focused_target();
                vec![self.focus_event(observed_at)]
            }
        }
    }

    pub(super) fn refresh_window_target(&mut self) {
        let result = copy_element(self.application.as_ptr(), "AXFocusedWindow")
            .and_then(|target| self.replace_window_target(target));
        match result {
            Ok(()) => self.recover(AxRecoverySite::FocusedWindow),
            Err(error) => self.record_native(AxRecoverySite::FocusedWindow, &error),
        }
    }

    fn same_value_target(&self, target: Option<&OwnedCf>) -> bool {
        match (self.focused_target.current(), target) {
            (Some(current), Some(target)) => cf_equal(current.element.as_ptr(), target.as_ptr()),
            (None, None) => true,
            _ => false,
        }
    }

    fn focus_event(&self, observed_at: OffsetDateTime) -> NativeAxEvent {
        NativeAxEvent::UiFocused {
            pid: self.context.pid,
            generation: self.focused_target.generation(),
            window: self
                .focused_target
                .current()
                .and_then(|target| target.context.window.clone()),
            element: self
                .focused_target
                .current()
                .filter(|target| target.context.field_class != FieldClass::SecureText)
                .map(|target| target.context.element.clone()),
            observed_at,
        }
    }

    fn prepare_focused_target(
        &mut self,
        target: Option<OwnedCf>,
        snapshot: Option<FocusedElementSnapshot>,
    ) -> Result<Option<RegisteredFocusedTarget>, NativeAxError> {
        let generation = self.focused_target.next_generation();
        match target.zip(snapshot) {
            Some((element, snapshot)) => {
                let registration_class = field_class(
                    snapshot.element.role.as_deref(),
                    snapshot.element.subrole.as_deref(),
                );
                let registration = self.notifications.reconcile(
                    &element,
                    "AXValueChanged",
                    registration_class,
                    || {
                        add_notification(
                            self.observer.as_ptr(),
                            element.as_ptr(),
                            "AXValueChanged",
                            (&raw const *self.context).cast_mut().cast(),
                        )
                    },
                    || Ok(()),
                );
                match registration {
                    Err(RegistrationError::Register(error)) => {
                        crate::trace::trace!(
                            "component=ax phase=focus_target action=register pid={} target_generation={} registration=error operation={} code={}",
                            self.context.pid,
                            generation,
                            error.operation(),
                            error.code()
                        );
                        return Err(error);
                    }
                    Err(RegistrationError::Unregister(error)) => return Err(error),
                    Ok(()) => {}
                }
                crate::trace::trace!(
                    "component=ax phase=focus_target action=prepare pid={} target_generation={} field_class={} registration={}",
                    self.context.pid,
                    generation,
                    crate::trace::field_class_name(snapshot.field_class),
                    if self
                        .notifications
                        .accepts_delivery(element.as_ptr(), "AXValueChanged")
                    {
                        "registered"
                    } else {
                        "skipped"
                    }
                );
                let capture_text_content = self.text_content_allowed(snapshot.window.as_ref());
                Ok(Some(RegisteredFocusedTarget {
                    element,
                    context: FocusedValueContext::new(
                        snapshot.window,
                        snapshot.element,
                        capture_text_content,
                        snapshot.text_baseline,
                        generation,
                        snapshot.field_class,
                    ),
                }))
            }
            None => {
                crate::trace::trace!(
                    "component=ax phase=focus_target action=prepare pid={} target_generation={} registration=skipped reason=target_or_snapshot_missing",
                    self.context.pid,
                    generation
                );
                Ok(None)
            }
        }
    }

    fn commit_focused_target(
        &mut self,
        registered: Option<RegisteredFocusedTarget>,
        defer_previous: bool,
    ) {
        let installed = registered.is_some();
        let registration = registered.as_ref().is_some_and(|target| {
            self.notifications
                .accepts_delivery(target.element.as_ptr(), "AXValueChanged")
        });
        if let Ok(previous) = self
            .focused_target
            .transition::<NativeAxError>(Ok(registered))
        {
            crate::trace::trace!(
                "component=ax phase=focus_target action={} pid={} target_generation={} registration={} defer_previous={}",
                if installed { "install" } else { "clear" },
                self.context.pid,
                self.focused_target.generation(),
                if registration {
                    "registered"
                } else {
                    "skipped"
                },
                defer_previous
            );
            self.remove_focused_target(previous, defer_previous);
        }
    }

    fn clear_focused_target(&mut self) {
        if let Ok(previous) = self.focused_target.transition::<NativeAxError>(Ok(None)) {
            crate::trace::trace!(
                "component=ax phase=focus_target action=clear pid={} target_generation={} reason=focus_read_error",
                self.context.pid,
                self.focused_target.generation()
            );
            self.remove_focused_target(previous, true);
        }
    }

    fn remove_focused_target(
        &mut self,
        target: Option<RegisteredFocusedTarget>,
        defer_context: bool,
    ) {
        let Some(target) = target else {
            return;
        };
        let generation = target.context.generation;
        let needed_cleanup = self
            .notifications
            .needs_cleanup(target.element.as_ptr(), "AXValueChanged");
        match self
            .notifications
            .unregister(target.element.as_ptr(), "AXValueChanged", || {
                remove_notification(
                    self.observer.as_ptr(),
                    target.element.as_ptr(),
                    "AXValueChanged",
                )
            }) {
            Err(error) => {
                crate::trace::trace!(
                    "component=ax phase=focus_target action=unregister pid={} target_generation={} result=error",
                    self.context.pid,
                    generation
                );
                let error = match error {
                    RegistrationError::Register(error) | RegistrationError::Unregister(error) => {
                        error
                    }
                };
                self.record_native(AxRecoverySite::FocusedValueUnregistration, &error);
            }
            Ok(()) => {
                self.recover(AxRecoverySite::FocusedValueUnregistration);
                if needed_cleanup {
                    crate::trace::trace!(
                        "component=ax phase=focus_target action=unregister pid={} target_generation={} result=removed",
                        self.context.pid,
                        generation
                    );
                }
            }
        }
        if defer_context {
            crate::trace::trace!(
                "component=ax phase=value action=defer pid={} target_generation={} reason=focus_transition",
                self.context.pid,
                generation
            );
            self.retired_contexts.push(target.context);
        }
    }

    fn replace_window_target(&mut self, target: Option<OwnedCf>) -> Result<(), NativeAxError> {
        if self.window_target.as_ref().is_some_and(|current| {
            target
                .as_ref()
                .is_some_and(|target| cf_equal(current.as_ptr(), target.as_ptr()))
        }) {
            return Ok(());
        }
        if let Some(target) = target {
            self.notifications
                .register(&target, "AXTitleChanged", || {
                    add_notification(
                        self.observer.as_ptr(),
                        target.as_ptr(),
                        "AXTitleChanged",
                        (&raw const *self.context).cast_mut().cast(),
                    )
                })
                .map_err(|error| match error {
                    RegistrationError::Register(error) | RegistrationError::Unregister(error) => {
                        error
                    }
                })?;
            if let Some(previous) = self.window_target.take() {
                self.unregister_window_target(&previous);
            }
            self.window_target = Some(target);
        } else if let Some(previous) = self.window_target.take() {
            self.unregister_window_target(&previous);
        }
        Ok(())
    }

    fn unregister_window_target(&mut self, target: &OwnedCf) {
        let result = self
            .notifications
            .unregister(target.as_ptr(), "AXTitleChanged", || {
                remove_notification(self.observer.as_ptr(), target.as_ptr(), "AXTitleChanged")
            });
        match result {
            Ok(()) => self.recover(AxRecoverySite::WindowTitleUnregistration),
            Err(error) => {
                let error = match error {
                    RegistrationError::Register(error) | RegistrationError::Unregister(error) => {
                        error
                    }
                };
                self.record_native(AxRecoverySite::WindowTitleUnregistration, &error);
            }
        }
    }

    pub(in crate::ffi::ax) fn record_native(&self, site: AxRecoverySite, error: &NativeAxError) {
        self.record_native_at(AxFailurePhase::Observer, site, error);
    }

    pub(in crate::ffi::ax) fn record_native_at(
        &self,
        phase: AxFailurePhase,
        site: AxRecoverySite,
        error: &NativeAxError,
    ) {
        self.failures.record_native(
            &self.degraded,
            Some(i64::from(self.context.pid)),
            phase,
            site,
            error.operation(),
            error.code(),
        );
    }

    pub(in crate::ffi::ax) fn recover(&self, site: AxRecoverySite) {
        self.failures
            .recover(Some(i64::from(self.context.pid)), site);
    }

    pub(super) fn text_content_allowed(&self, window: Option<&super::NativeWindow>) -> bool {
        self.text_content_decision(window)
            .is_some_and(|decision| decision.is_allowed())
    }

    pub(super) fn text_content_decision(
        &self,
        window: Option<&super::NativeWindow>,
    ) -> Option<CaptureDecision> {
        self.capture_text_content.then(|| {
            self.capture_policy.decision(
                PrivacyScope::TextContent,
                &self.app,
                window.and_then(|window| window.id),
            )
        })
    }
}

impl Drop for AppObserver {
    fn drop(&mut self) {
        #[cfg(test)]
        if self.skip_native_cleanup {
            return;
        }
        unsafe { remove_current_run_loop_source(self.source) };
        for (element, notification) in self.notifications.registered_notifications() {
            let _ = remove_notification(self.observer.as_ptr(), element, notification);
        }
        self.set_manual_accessibility(false);
    }
}
