use std::{
    sync::{Arc, atomic::AtomicU64, mpsc::sync_channel},
    time::Instant,
};

use zanei_core::{config::FilterConfig, schema::App};

use super::{
    super::{
        NativeElement, ObserverContext,
        cf::{CfRef, OwnedCf, cf_string},
        element::create_application,
        value_context::FocusedValueContext,
    },
    AppObserver, RegisteredFocusedTarget,
};
use crate::{
    capture_policy::CapturePolicy, chrome::chrome_eligibility_channel, focused_field::FieldClass,
};

impl AppObserver {
    pub(in crate::ffi::ax) fn fake_attached_with_unavailable_application(
        attached_at: Instant,
    ) -> Self {
        Self::fake_with_application(
            create_application(i32::MAX).expect("application AX element"),
            Some(attached_at),
        )
    }

    pub(in crate::ffi::ax) fn fake_with_unknown_focused_target() -> Self {
        Self::fake_with_application(
            cf_string("fake application").expect("application CFString"),
            None,
        )
    }

    fn fake_with_application(application: OwnedCf, attached_at: Option<Instant>) -> Self {
        let observer = cf_string("fake observer").expect("observer CFString");
        let source = application.as_ptr();
        let element = cf_string("fake focused element").expect("element CFString");
        let (sender, _receiver) = sync_channel(1);
        let context = Box::new(ObserverContext {
            pid: 7,
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
        });
        let filter = FilterConfig::default();
        let (_, chrome) = chrome_eligibility_channel(filter.clone());
        let degraded = Arc::new(AtomicU64::new(0));
        let app = App {
            name: "Fake".to_owned(),
            bundle_id: Some("dev.zanei.fake".to_owned()),
            pid: Some(7),
        };
        let capture_policy = CapturePolicy::new(chrome, filter, None);
        let mut observer = match attached_at {
            Some(attached_at) => Self::new_attached(
                application,
                observer,
                source,
                context,
                degraded,
                Default::default(),
                false,
                app,
                capture_policy,
                false,
                attached_at,
            ),
            None => Self::new(
                application,
                observer,
                source,
                context,
                degraded,
                Default::default(),
                false,
                app,
                capture_policy,
                false,
            ),
        };
        observer.skip_native_cleanup = true;
        let generation = observer.focused_target.next_generation();
        let installed =
            observer
                .focused_target
                .transition::<()>(Ok(Some(RegisteredFocusedTarget {
                    element,
                    context: FocusedValueContext::new(
                        None,
                        NativeElement {
                            role: None,
                            subrole: None,
                            title: None,
                            value: None,
                            value_len: None,
                            capture_decision: None,
                        },
                        false,
                        None,
                        generation,
                        FieldClass::Unknown,
                    ),
                })));
        assert!(installed.is_ok(), "install fake focused target");
        observer
    }

    pub(in crate::ffi::ax) fn fake_focused_element(&self) -> CfRef {
        self.focused_target
            .current()
            .expect("fake focused target")
            .element
            .as_ptr()
    }

    pub(in crate::ffi::ax) fn fake_focused_field_class(&self) -> FieldClass {
        self.focused_target
            .current()
            .expect("fake focused target")
            .context
            .field_class
    }

    pub(in crate::ffi::ax) fn fake_degraded_operations(&self) -> u64 {
        self.degraded.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(in crate::ffi::ax) fn fake_failure_state(&self) -> crate::ax::AxFailureState {
        self.failures.state()
    }
}

#[test]
fn classification_preserves_static_body_origin_and_drops_it_with_body() {
    use crate::{ffi::ax::element::ValueFieldSnapshot, text_capture::input_authorization_channel};
    let mut observer = AppObserver::fake_with_unknown_focused_target();
    let decision = observer.capture_policy.decision(
        zanei_core::privacy::PrivacyScope::TextContent,
        &observer.app,
        None,
        None,
    );
    let context = &mut observer
        .focused_target
        .current_mut()
        .expect("target")
        .context;
    context.element.role = Some("AXStaticText".to_owned());
    context.element.value = Some("retained body".to_owned());
    context.element.capture_decision = Some(Box::new(decision.clone()));
    context.field_class = FieldClass::KnownSafeNonText;
    let (_, mut authorizations) = input_authorization_channel();
    for role in ["AXStaticText", "AXTextArea"] {
        let class = crate::focused_field::field_class(Some(role), None);
        assert!(
            observer
                .refresh_current_field_class_with(
                    ValueFieldSnapshot {
                        role: Some(role.to_owned()),
                        subrole: None,
                        field_class: class,
                        registration_class: Some(class),
                        failure: None,
                    },
                    &mut authorizations,
                    || Ok(()),
                    || Ok(()),
                )
                .is_ok(),
            "reclassification"
        );
        let super::super::NativeAxEvent::UiFocused {
            element: Some(element),
            ..
        } = observer.focus_event(time::OffsetDateTime::UNIX_EPOCH)
        else {
            panic!("focus event")
        };
        assert_eq!(
            element.value.as_deref(),
            (role == "AXStaticText").then_some("retained body")
        );
        assert_eq!(
            element.capture_decision.as_deref(),
            (role == "AXStaticText").then_some(&decision)
        );
    }
}
