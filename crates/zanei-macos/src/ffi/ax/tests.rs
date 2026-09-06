use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use zanei_core::schema::FieldKind;

use super::{
    NativeAxError, NativeAxEvent, NativeAxObservation, NativeElement, TargetKind,
    cf::cf_string,
    element::{ValueFieldSnapshot, focused_element_is_excluded, gated_value, value_length},
    native_error,
    observer::{
        AppObserver,
        value_registration::{NotificationRegistry, RegistrationError},
    },
    runtime::secure_input_active,
    value_context::{
        DeferredResolution, DeferredValueContext, FocusedValueContext, after_target_preparation,
        classified_field_snapshot,
    },
};
use crate::{
    ax::health::{AxFailurePhase, AxFailurePublisher, AxRecoverySite},
    focused_field::{FieldClass, field_class},
    secure_input::secure_input_test_channel,
    text_capture::{
        FocusedTarget, VALUE_DEBOUNCE, ValueCapture, ValueObservation, input_authorization_channel,
    },
};

#[derive(Default)]
struct FakeValueNotifications {
    add_calls: Cell<usize>,
    remove_calls: Cell<usize>,
    add_error: Cell<Option<i32>>,
    remove_error: Cell<Option<i32>>,
}

impl FakeValueNotifications {
    fn add(&self) -> Result<(), NativeAxError> {
        self.add_calls.set(self.add_calls.get() + 1);
        self.add_error
            .get()
            .map_or(Ok(()), |code| Err(native_error("fake add", code)))
    }

    fn remove(&self) -> Result<(), NativeAxError> {
        self.remove_calls.set(self.remove_calls.get() + 1);
        self.remove_error
            .get()
            .map_or(Ok(()), |code| Err(native_error("fake remove", code)))
    }
}

pub(in crate::ffi::ax) fn fake_field_snapshot(role: Option<&str>) -> ValueFieldSnapshot {
    ValueFieldSnapshot {
        role: role.map(str::to_owned),
        subrole: None,
        field_class: field_class(role, None),
        registration_class: Some(field_class(role, None)),
        failure: None,
    }
}

fn refresh_fake_target(
    observer: &mut AppObserver,
    role: Option<&str>,
    authorizations: &mut crate::InputAuthorizations,
    fake_ax: &FakeValueNotifications,
) -> Result<(), RegistrationError> {
    observer.refresh_current_field_class_with(
        fake_field_snapshot(role),
        authorizations,
        || fake_ax.add(),
        || fake_ax.remove(),
    )
}

#[test]
fn same_target_unknown_to_text_area_registers_value_notification_once() {
    let fake_ax = FakeValueNotifications::default();
    let mut observer = AppObserver::fake_with_unknown_focused_target();
    let element = observer.fake_focused_element();
    let (_publisher, mut authorizations) = input_authorization_channel();

    for snapshot in [None, Some("AXTextArea"), Some("AXTextArea")] {
        assert!(
            refresh_fake_target(&mut observer, snapshot, &mut authorizations, &fake_ax).is_ok()
        );
    }

    assert_eq!(fake_ax.add_calls.get(), 1);
    assert_eq!(fake_ax.remove_calls.get(), 0);
    assert_eq!(
        observer.fake_focused_field_class(),
        FieldClass::KnownText(FieldKind::Text)
    );
    assert!(observer.is_current_target(TargetKind::Value, element));
}

#[test]
fn application_role_query_failure_is_degraded_and_reconcile_remains_one_shot() {
    let now = Instant::now();
    let mut observer = AppObserver::fake_attached_with_unavailable_application(now);
    let (_publisher, mut authorizations) = input_authorization_channel();

    assert_eq!(observer.fake_degraded_operations(), 1);
    assert_eq!(
        observer
            .fake_failure_state()
            .current()
            .map(|failure| failure.phase),
        Some(AxFailurePhase::Observer)
    );
    observer.recover(AxRecoverySite::ApplicationRole);
    assert!(observer.fake_failure_state().current().is_none());
    assert!(
        observer
            .reconcile_accessibility_if_due(
                now + Duration::from_millis(999),
                time::OffsetDateTime::UNIX_EPOCH,
                false,
                &mut authorizations,
            )
            .is_empty()
    );
    assert!(matches!(
        observer
            .reconcile_accessibility_if_due(
                now + Duration::from_secs(1),
                time::OffsetDateTime::UNIX_EPOCH,
                false,
                &mut authorizations,
            )
            .as_slice(),
        [NativeAxObservation::FocusedFieldObserved {
            pid: 7,
            focused_field: None,
        }]
    ));
    assert!(
        observer
            .reconcile_accessibility_if_due(
                now + Duration::from_secs(2),
                time::OffsetDateTime::UNIX_EPOCH,
                false,
                &mut authorizations,
            )
            .is_empty()
    );
}

#[test]
fn same_target_known_to_excluded_unregisters_and_blocks_stale_delivery() {
    for excluded_role in [Some("AXSecureTextField"), None] {
        let fake_ax = FakeValueNotifications::default();
        let mut observer = AppObserver::fake_with_unknown_focused_target();
        let element = observer.fake_focused_element();
        let (_publisher, mut authorizations) = input_authorization_channel();
        assert!(
            refresh_fake_target(
                &mut observer,
                Some("AXTextArea"),
                &mut authorizations,
                &fake_ax,
            )
            .is_ok()
        );

        for _ in 0..2 {
            assert!(
                refresh_fake_target(&mut observer, excluded_role, &mut authorizations, &fake_ax,)
                    .is_ok()
            );
        }

        assert_eq!(fake_ax.add_calls.get(), 1);
        assert_eq!(fake_ax.remove_calls.get(), 1);
        assert!(!observer.is_current_target(TargetKind::Value, element));
    }
}

#[test]
fn notification_not_registered_is_readded_before_delivery_resumes() {
    let fake_ax = FakeValueNotifications::default();
    let mut observer = AppObserver::fake_with_unknown_focused_target();
    let element = observer.fake_focused_element();
    let (_publisher, mut authorizations) = input_authorization_channel();
    assert!(
        refresh_fake_target(
            &mut observer,
            Some("AXTextArea"),
            &mut authorizations,
            &fake_ax,
        )
        .is_ok()
    );

    fake_ax.remove_error.set(Some(-25_210));
    assert!(
        refresh_fake_target(
            &mut observer,
            Some("AXSecureTextField"),
            &mut authorizations,
            &fake_ax,
        )
        .is_ok()
    );
    assert!(!observer.is_current_target(TargetKind::Value, element));
    assert_eq!(observer.fake_focused_field_class(), FieldClass::SecureText);

    fake_ax.remove_error.set(None);
    assert!(
        refresh_fake_target(
            &mut observer,
            Some("AXTextArea"),
            &mut authorizations,
            &fake_ax,
        )
        .is_ok()
    );
    assert_eq!(fake_ax.add_calls.get(), 2);
    assert!(observer.is_current_target(TargetKind::Value, element));
}

#[test]
fn stale_registration_requires_an_idempotent_add_before_delivery_resumes() {
    let fake_ax = FakeValueNotifications::default();
    let mut observer = AppObserver::fake_with_unknown_focused_target();
    let element = observer.fake_focused_element();
    let (_publisher, mut authorizations) = input_authorization_channel();
    assert!(
        refresh_fake_target(
            &mut observer,
            Some("AXTextArea"),
            &mut authorizations,
            &fake_ax,
        )
        .is_ok()
    );

    fake_ax.remove_error.set(Some(-1));
    assert!(matches!(
        refresh_fake_target(
            &mut observer,
            Some("AXSecureTextField"),
            &mut authorizations,
            &fake_ax,
        ),
        Err(RegistrationError::Unregister(_))
    ));
    assert!(!observer.is_current_target(TargetKind::Value, element));

    fake_ax.add_error.set(Some(-25_209));
    assert!(
        refresh_fake_target(
            &mut observer,
            Some("AXTextArea"),
            &mut authorizations,
            &fake_ax,
        )
        .is_ok()
    );
    assert_eq!(fake_ax.add_calls.get(), 2);
    assert!(observer.is_current_target(TargetKind::Value, element));
}

#[test]
fn stale_target_is_reclaimed_when_focus_returns_from_another_target() {
    let first_a = cf_string("target A").expect("target A");
    let second_a = cf_string("target A").expect("equal target A");
    let target_b = cf_string("target B").expect("target B");
    let mut registry = NotificationRegistry::default();

    assert!(
        registry
            .register(&first_a, "AXValueChanged", || Ok(()))
            .is_ok()
    );
    assert!(matches!(
        registry.unregister(first_a.as_ptr(), "AXValueChanged", || Err(native_error(
            "fake remove",
            -1,
        ))),
        Err(RegistrationError::Unregister(_))
    ));
    assert!(
        registry
            .register(&target_b, "AXValueChanged", || Ok(()))
            .is_ok()
    );
    assert!(
        registry
            .register(&second_a, "AXValueChanged", || Err(native_error(
                "fake add", -25_209,
            )))
            .is_ok()
    );
    assert!(
        registry
            .unregister(target_b.as_ptr(), "AXValueChanged", || Ok(()))
            .is_ok()
    );

    assert!(registry.accepts_delivery(second_a.as_ptr(), "AXValueChanged"));
    assert!(!registry.accepts_delivery(target_b.as_ptr(), "AXValueChanged"));
}

#[test]
fn secure_input_suppression_preserves_registration_until_role_recovery() {
    let fake_ax = FakeValueNotifications::default();
    let mut observer = AppObserver::fake_with_unknown_focused_target();
    let element = observer.fake_focused_element();
    let (_publisher, mut authorizations) = input_authorization_channel();
    assert!(
        refresh_fake_target(
            &mut observer,
            Some("AXTextArea"),
            &mut authorizations,
            &fake_ax,
        )
        .is_ok()
    );

    assert!(
        observer
            .take_due_value_events(Instant::now(), true, &mut authorizations)
            .is_empty()
    );
    assert!(observer.flush_pending(true, &mut authorizations).is_empty());
    assert!(observer.is_current_target(TargetKind::Value, element));
    assert_eq!(fake_ax.remove_calls.get(), 0);

    assert!(
        refresh_fake_target(
            &mut observer,
            Some("AXTextArea"),
            &mut authorizations,
            &fake_ax,
        )
        .is_ok()
    );
    assert_eq!(fake_ax.add_calls.get(), 1);
    assert_eq!(
        observer.fake_focused_field_class(),
        FieldClass::KnownText(FieldKind::Text)
    );
}

#[test]
fn failed_registration_does_not_enable_value_delivery() {
    let fake_ax = FakeValueNotifications::default();
    fake_ax.add_error.set(Some(-1));
    let mut observer = AppObserver::fake_with_unknown_focused_target();
    let element = observer.fake_focused_element();
    let (_publisher, mut authorizations) = input_authorization_channel();
    let result = refresh_fake_target(
        &mut observer,
        Some("AXTextArea"),
        &mut authorizations,
        &fake_ax,
    );

    assert!(matches!(result, Err(RegistrationError::Register(_))));
    assert_eq!(observer.fake_focused_field_class(), FieldClass::Unknown);
    assert!(!observer.is_current_target(TargetKind::Value, element));
}

#[test]
fn disabled_text_capture_does_not_read_the_ax_value() {
    let called = Cell::new(false);
    let value = gated_value::<()>(
        false,
        FieldClass::KnownSafeNonText,
        Some("AXButton"),
        || {
            called.set(true);
            Ok(Some("secret".to_owned()))
        },
    )
    .unwrap();

    assert_eq!(value, None);
    assert!(!called.get());
}

#[test]
fn free_text_field_values_are_not_read() {
    let field_kinds = [
        FieldKind::Text,
        FieldKind::Search,
        FieldKind::Url,
        FieldKind::Email,
        FieldKind::Number,
        FieldKind::Other,
    ];

    for field_kind in field_kinds {
        let called = Cell::new(false);
        let value = gated_value::<()>(true, FieldClass::KnownText(field_kind), None, || {
            called.set(true);
            Ok(Some("document contents".to_owned()))
        })
        .unwrap();

        assert_eq!(value, None);
        assert!(!called.get());
    }
}

#[test]
fn opted_in_non_text_element_values_are_preserved() {
    let value = gated_value::<()>(
        true,
        FieldClass::KnownSafeNonText,
        Some("AXCheckBox"),
        || Ok(Some("checked".to_owned())),
    )
    .unwrap();

    assert_eq!(value.as_deref(), Some("checked"));
}

#[test]
fn character_count_is_available_without_captured_content() {
    assert_eq!(value_length(Some(12), None), Some(12));
    assert_eq!(value_length(None, Some("日本語")), Some(3));
}

#[test]
fn secure_input_or_probe_failure_is_fail_closed() {
    let (probe, responder) = secure_input_test_channel();
    let degraded = AtomicU64::new(0);
    let failures = AxFailurePublisher::default();
    let response = thread::spawn(move || responder.respond_next(true));
    assert!(secure_input_active(
        true,
        Some(&probe),
        &degraded,
        &failures,
        AxRecoverySite::SecureInputTest
    ));
    response.join().expect("Secure Input response thread");

    let (probe, responder) = secure_input_test_channel();
    drop(responder);
    assert!(secure_input_active(
        true,
        Some(&probe),
        &degraded,
        &failures,
        AxRecoverySite::SecureInputTest
    ));
    assert_eq!(degraded.load(Ordering::Relaxed), 1);
    assert!(failures.state().current().is_some());

    let (probe, responder) = secure_input_test_channel();
    let response = thread::spawn(move || responder.respond_next(false));
    assert!(!secure_input_active(
        true,
        Some(&probe),
        &degraded,
        &failures,
        AxRecoverySite::SecureInputTest
    ));
    response.join().expect("Secure Input recovery response");
    assert!(failures.state().current().is_none());
}

#[test]
fn actual_secure_text_field_is_excluded_from_focus_snapshot() {
    assert!(focused_element_is_excluded(FieldClass::SecureText));
    assert!(!focused_element_is_excluded(FieldClass::KnownText(
        FieldKind::Text
    )));
}

#[test]
fn secure_input_disabled_allows_authorized_text_capture() {
    let (probe, responder) = secure_input_test_channel();
    let degraded = AtomicU64::new(0);
    let failures = AxFailurePublisher::default();
    let response = thread::spawn(move || responder.respond_next(false));

    assert!(!secure_input_active(
        true,
        Some(&probe),
        &degraded,
        &failures,
        AxRecoverySite::SecureInputTest
    ));
    response.join().expect("Secure Input response thread");
}

#[test]
fn unknown_element_value_is_not_read() {
    let called = Cell::new(false);
    let value = gated_value::<()>(true, FieldClass::Unknown, Some("AXDocument"), || {
        called.set(true);
        Ok(Some("document contents".to_owned()))
    })
    .unwrap();

    assert_eq!(value, None);
    assert!(!called.get());
}

#[test]
fn long_static_text_value_is_suppressed() {
    let value = gated_value::<()>(
        true,
        FieldClass::KnownSafeNonText,
        Some("AXStaticText"),
        || Ok(Some("x".repeat(257))),
    )
    .unwrap();

    assert_eq!(value, None);
}

#[test]
fn detached_value_context_resolves_after_late_confirmation() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the reservation");
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(
        ValueObservation {
            pid: 7,
            target_generation: 1,
            notification_at: now + Duration::from_millis(10),
            value: Some("Ax".to_owned()),
            value_len: Some(2),
            field_class: FieldClass::KnownText(FieldKind::Text),
            capture_decision: None,
        },
        &mut authorizations,
    );
    let context = FocusedValueContext {
        window: None,
        element: NativeElement {
            role: Some("AXTextArea".to_owned()),
            subrole: None,
            title: None,
            value: None,
            value_len: Some(2),
        },
        capture,
        generation: 1,
        field_class: FieldClass::KnownText(FieldKind::Text),
        observed_at: None,
    };
    let mut detached = DeferredValueContext::new(7, context);

    authorization.confirm();
    let DeferredResolution::Complete(Some(NativeAxEvent::UiValueChanged(event))) = detached
        .take_due(
            now + VALUE_DEBOUNCE + Duration::from_millis(10),
            false,
            &mut authorizations,
        )
    else {
        panic!("detached value should resolve into a value event");
    };
    assert_eq!(event.text.as_deref(), Some("x"));
}

#[test]
fn detached_context_without_pending_value_is_cleaned_up() {
    let (_publisher, mut authorizations) = input_authorization_channel();
    let context = FocusedValueContext {
        window: None,
        element: NativeElement {
            role: Some("AXTextArea".to_owned()),
            subrole: None,
            title: None,
            value: None,
            value_len: Some(1),
        },
        capture: ValueCapture::new(
            true,
            Some("A".to_owned()),
            FieldClass::KnownText(FieldKind::Text),
        ),
        generation: 1,
        field_class: FieldClass::KnownText(FieldKind::Text),
        observed_at: None,
    };
    let mut detached = DeferredValueContext::new(7, context);

    assert!(matches!(
        detached.take_due(Instant::now(), false, &mut authorizations),
        DeferredResolution::Complete(None)
    ));
}

#[test]
fn degraded_same_target_classification_preserves_pending_value() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the reservation");
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(
        ValueObservation {
            pid: 7,
            target_generation: 1,
            notification_at: now,
            value: Some("Ax".to_owned()),
            value_len: Some(2),
            field_class: FieldClass::KnownText(FieldKind::Text),
            capture_decision: None,
        },
        &mut authorizations,
    );
    let degraded = ValueFieldSnapshot {
        role: None,
        subrole: None,
        field_class: FieldClass::Unknown,
        registration_class: None,
        failure: Some(native_error("AXRole", -25204)),
    };

    assert!(classified_field_snapshot(degraded).is_none());
    authorization.confirm();
    assert_eq!(
        capture
            .take_due(now + VALUE_DEBOUNCE, &mut authorizations)
            .and_then(|emission| emission.text),
        Some("x".to_owned())
    );

    let secure = ValueFieldSnapshot {
        role: None,
        subrole: None,
        field_class: FieldClass::SecureText,
        registration_class: None,
        failure: None,
    };
    assert_eq!(
        classified_field_snapshot(secure).map(|snapshot| snapshot.field_class),
        Some(FieldClass::SecureText)
    );
}

#[test]
fn failed_target_preparation_does_not_consume_previous_value() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the reservation");
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(
        ValueObservation {
            pid: 7,
            target_generation: 1,
            notification_at: now,
            value: Some("Ax".to_owned()),
            value_len: Some(2),
            field_class: FieldClass::KnownText(FieldKind::Text),
            capture_decision: None,
        },
        &mut authorizations,
    );

    let resolutions = Cell::new(0);
    let result = after_target_preparation(Err::<(), _>("target snapshot failed"), || {
        resolutions.set(resolutions.get() + 1);
        capture.resolve_unreadable_focus_change(&mut authorizations)
    });
    assert!(result.is_err());
    assert_eq!(resolutions.get(), 0);
    authorization.confirm();
    assert_eq!(
        capture
            .take_due(now + VALUE_DEBOUNCE, &mut authorizations)
            .and_then(|emission| emission.text),
        Some("x".to_owned())
    );

    assert_eq!(
        after_target_preparation(Ok::<_, &str>("prepared target"), || {
            resolutions.set(resolutions.get() + 1);
            "resolved previous"
        }),
        Ok(("prepared target", "resolved previous"))
    );
    assert_eq!(resolutions.get(), 1);
}

#[test]
fn failed_focus_clears_current_and_defers_previous_value() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the reservation");
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(
        ValueObservation {
            pid: 7,
            target_generation: 1,
            notification_at: now,
            value: Some("Ax".to_owned()),
            value_len: Some(2),
            field_class: FieldClass::KnownText(FieldKind::Text),
            capture_decision: None,
        },
        &mut authorizations,
    );
    let context = FocusedValueContext {
        window: None,
        element: NativeElement {
            role: Some("AXTextArea".to_owned()),
            subrole: None,
            title: None,
            value: None,
            value_len: Some(2),
        },
        capture,
        generation: 1,
        field_class: FieldClass::KnownText(FieldKind::Text),
        observed_at: None,
    };
    let mut target = FocusedTarget::new();
    assert!(matches!(
        target.transition::<()>(Ok(Some(context))),
        Ok(None)
    ));
    let previous = match target.transition::<()>(Ok(None)) {
        Ok(Some(previous)) => previous,
        _ => panic!("focus failure should return the previous target for deferral"),
    };

    assert_eq!(target.generation(), 2);
    assert!(target.current().is_none());
    let focused_element = target.current().map(|context| context.element.clone());
    assert_eq!(focused_element, None);

    let mut deferred = DeferredValueContext::new(7, previous);
    authorization.confirm();
    let DeferredResolution::Complete(Some(NativeAxEvent::UiValueChanged(event))) =
        deferred.take_due(now + VALUE_DEBOUNCE, false, &mut authorizations)
    else {
        panic!("deferred previous value should resolve after late confirmation");
    };
    assert_eq!(event.text.as_deref(), Some("x"));
}

pub(in crate::ffi::ax) fn ide_policy(missing: &str) -> crate::CapturePolicy {
    use zanei_core::config::capture_policy::{
        BrowserMode, BrowserPolicy, CapturePolicyConfig, IdePolicy, PolicyAction,
    };
    let mut filter = zanei_core::config::FilterConfig {
        capture_policy: Some(CapturePolicyConfig {
            allowed_apps: vec!["Cursor".to_owned()],
            browser: BrowserPolicy {
                mode: BrowserMode::Off,
                default_policy: PolicyAction::Block,
                on_url_unavailable: PolicyAction::Block,
                block_auth: true,
                block_payments: true,
                allow_list: vec![],
                block_list: vec![],
            },
            ide: IdePolicy {
                block_env_files: missing != "disabled",
                on_file_name_unavailable: if missing == "allow" {
                    PolicyAction::Allow
                } else {
                    PolicyAction::Block
                },
            },
        }),
        ..Default::default()
    };
    if missing == "standalone" {
        filter.capture_policy = None;
    }
    let (_, chrome) = crate::chrome::chrome_eligibility_channel(filter.clone());
    crate::CapturePolicy::new(chrome, filter, None)
}

pub(in crate::ffi::ax) fn ide_app() -> zanei_core::schema::App {
    zanei_core::schema::App {
        name: "Cursor".to_owned(),
        bundle_id: Some("com.todesktop.230313mzl4w4u92".to_owned()),
        pid: Some(7),
    }
}

pub(in crate::ffi::ax) fn titled_window(title: Option<&str>) -> Option<super::NativeWindow> {
    Some(super::NativeWindow {
        id: Some(11),
        title: title.map(str::to_owned),
    })
}

#[test]
fn value_and_focus_out_reads_bind_actual_title_and_discard_previous_surface() {
    use super::element::{capture_value_snapshot_with, value_snapshot_with};
    for focus_out in [false, true] {
        for (old, actual, missing, allowed) in [
            (Some("main.rs"), Some(".env"), "block", false),
            (Some(".env"), Some("main.rs"), "block", true),
            (Some(".env"), Some(".env.example"), "block", true),
            (Some("main.rs"), None, "block", false),
            (Some("main.rs"), None, "allow", true),
            (Some("main.rs"), Some("next.rs"), "block", true),
            (Some("main.rs"), Some("next.rs"), "standalone", true),
            (Some("main.rs"), None, "standalone", true),
            (Some("main.rs"), Some(".env"), "standalone", true),
            (Some(".env"), Some("main.rs"), "standalone", true),
            (Some("main.rs"), Some("next.rs"), "disabled", true),
            (Some("main.rs"), None, "disabled", true),
            (Some("main.rs"), Some(".env"), "disabled", true),
            (Some(".env"), Some("main.rs"), "disabled", true),
            (Some("main.rs"), Some("main.rs"), "post_changed", true),
            (Some("main.rs"), Some("main.rs"), "post_error", true),
        ] {
            let now = Instant::now();
            let (publisher, mut authorizations) = input_authorization_channel();
            let had_prior_body =
                old != Some(".env") || matches!(missing, "standalone" | "disabled");
            let mut context = FocusedValueContext::new(
                titled_window(old),
                NativeElement {
                    role: Some("AXTextArea".to_owned()),
                    subrole: None,
                    title: None,
                    value: None,
                    value_len: None,
                },
                true,
                had_prior_body.then(|| "old".to_owned()),
                1,
                FieldClass::KnownText(FieldKind::Text),
            );
            if had_prior_body {
                publisher.prepare(7, 1, now).expect("reservation").confirm();
                context.capture.observe(
                    ValueObservation {
                        pid: 7,
                        target_generation: 1,
                        notification_at: now,
                        value: Some("old secret".to_owned()),
                        value_len: Some(10),
                        field_class: FieldClass::KnownText(FieldKind::Text),
                        capture_decision: None,
                    },
                    &mut authorizations,
                );
            }
            let reads = Cell::new(0);
            let window_reads = Cell::new(0);
            let post_rejected = matches!(missing, "post_changed" | "post_error");
            let policy = ide_policy(missing);
            let (snapshot, decision) = capture_value_snapshot_with(
                &mut context.window,
                &policy,
                &ide_app(),
                true,
                || {
                    context
                        .capture
                        .transition_class(7, 1, FieldClass::Unknown, &mut authorizations)
                },
                || {
                    window_reads.set(window_reads.get() + 1);
                    match (reads.get(), missing) {
                        (1, "post_changed") => Ok(titled_window(Some(".env"))),
                        (1, "post_error") => Err(native_error("AXWindow", -25_204)),
                        _ => Ok(titled_window(actual)),
                    }
                },
                |allowed| {
                    value_snapshot_with(
                        fake_field_snapshot(Some("AXTextArea")),
                        allowed,
                        || {
                            reads.set(reads.get() + 1);
                            Ok(Some("current body".to_owned()))
                        },
                        || Ok(Some(12)),
                    )
                },
            );
            assert_eq!(reads.get(), usize::from(allowed));
            assert_eq!(window_reads.get(), 1 + usize::from(allowed));
            assert_eq!(
                snapshot.value.as_deref(),
                (allowed && !post_rejected).then_some("current body")
            );
            let expected_window = match missing {
                "post_changed" => titled_window(Some(".env")),
                "post_error" => None,
                _ => titled_window(actual),
            };
            assert_eq!(context.window, expected_window);
            if post_rejected {
                assert_eq!(snapshot.value_len, None);
                assert_eq!(snapshot.failure.is_some(), missing == "post_error");
                assert!(!authorizations.matching_for_test(7, 1, now));
            }
            assert_eq!(
                decision.as_ref().map(crate::CaptureDecision::is_allowed),
                (!post_rejected).then_some(allowed)
            );
            assert!(
                context.capture.flush_pending(&mut authorizations).is_none(),
                "old body must not be rebound to current title"
            );
            if focus_out && snapshot.failure.is_some() {
                assert!(matches!(
                    context
                        .capture
                        .resolve_unreadable_focus_change(&mut authorizations),
                    crate::text_capture::FocusChangeCapture::Emit(None)
                ));
                continue;
            }
            let observation = context.observation(
                7,
                now + VALUE_DEBOUNCE,
                time::OffsetDateTime::UNIX_EPOCH,
                snapshot,
                decision.clone(),
            );
            if focus_out {
                assert!(matches!(
                    context
                        .capture
                        .resolve_focus_change(observation, &mut authorizations),
                    crate::text_capture::FocusChangeCapture::Emit(None)
                ));
            } else {
                assert!(
                    context
                        .capture
                        .observe(observation, &mut authorizations)
                        .is_none()
                );
            }
            if post_rejected {
                assert!(context.capture.flush_pending(&mut authorizations).is_none());
                continue;
            }
            let emission = context
                .capture
                .flush_pending(&mut authorizations)
                .expect("current metadata");
            let NativeAxEvent::UiValueChanged(event) = context.value_event(7, emission) else {
                panic!("value event")
            };
            assert_eq!(event.window, titled_window(actual));
            assert_eq!(event.capture_decision, decision);
            assert_eq!(
                event.text, None,
                "old authorization must not authorize another surface"
            );
            if allowed {
                let typed_at = now + VALUE_DEBOUNCE + Duration::from_millis(10);
                publisher
                    .prepare(7, 1, typed_at)
                    .expect("new surface input")
                    .confirm();
                let (snapshot, decision) = capture_value_snapshot_with(
                    &mut context.window,
                    &policy,
                    &ide_app(),
                    true,
                    || panic!("same surface must keep new authorization"),
                    || Ok(titled_window(actual)),
                    |allowed| {
                        value_snapshot_with(
                            fake_field_snapshot(Some("AXTextArea")),
                            allowed,
                            || Ok(Some("current body!".to_owned())),
                            || Ok(Some(13)),
                        )
                    },
                );
                let observation = context.observation(
                    7,
                    typed_at,
                    time::OffsetDateTime::UNIX_EPOCH,
                    snapshot,
                    decision,
                );
                context.capture.observe(observation, &mut authorizations);
                let emission = context
                    .capture
                    .flush_pending(&mut authorizations)
                    .expect("authorized new body");
                let NativeAxEvent::UiValueChanged(event) = context.value_event(7, emission) else {
                    panic!("value event")
                };
                assert_eq!(event.text.as_deref(), Some("!"));
                assert_eq!(event.window, titled_window(actual));
            }
        }
    }
}

#[test]
fn value_read_preserves_secure_unknown_and_disabled_boundaries() {
    use super::element::{capture_value_snapshot_with, value_snapshot_with};
    for class in [
        FieldClass::SecureText,
        FieldClass::Unknown,
        FieldClass::KnownText(FieldKind::Text),
    ] {
        for enabled in [false, true] {
            let mut window = titled_window(Some(".env"));
            let reads = Cell::new(0);
            let (snapshot, _) = capture_value_snapshot_with(
                &mut window,
                &ide_policy("block"),
                &ide_app(),
                enabled,
                || {},
                || Ok(titled_window(Some("main.rs"))),
                |allowed| {
                    let mut field = fake_field_snapshot(Some("AXTextArea"));
                    field.field_class = class;
                    value_snapshot_with(
                        field,
                        allowed,
                        || {
                            reads.set(reads.get() + 1);
                            Ok(Some("body".to_owned()))
                        },
                        || {
                            assert!(
                                class.is_known_text(),
                                "secure/unknown must not read character count"
                            );
                            Ok(Some(4))
                        },
                    )
                },
            );
            assert_eq!(reads.get(), usize::from(enabled && class.is_known_text()));
            assert_eq!(snapshot.value.is_some(), enabled && class.is_known_text());
        }
    }
}

#[test]
fn unavailable_ax_window_does_not_reuse_title_and_native_errors_do_not_read() {
    use super::element::{capture_value_snapshot_with, value_snapshot_with};
    for missing in ["allow", "block"] {
        let mut window = titled_window(Some("main.rs"));
        let (snapshot, decision) = capture_value_snapshot_with(
            &mut window,
            &ide_policy(missing),
            &ide_app(),
            true,
            || {},
            || Ok(None),
            |allowed| {
                value_snapshot_with(
                    fake_field_snapshot(Some("AXTextArea")),
                    allowed,
                    || Ok(Some("body".to_owned())),
                    || Ok(Some(4)),
                )
            },
        );
        assert!(window.is_none());
        assert_eq!(snapshot.value.is_some(), missing == "allow");
        assert_eq!(decision.expect("decision").is_allowed(), missing == "allow");
        let (snapshot, decision) = capture_value_snapshot_with(
            &mut window,
            &ide_policy(missing),
            &ide_app(),
            true,
            || {},
            || Err(native_error("AXWindow", -25_204)),
            |_| panic!("native failure must prevent value reads"),
        );
        assert!(snapshot.failure.is_some());
        assert!(decision.is_none());
    }
}

#[test]
fn disallowed_app_does_not_acquire_value_body() {
    use super::element::{capture_value_snapshot_with, value_snapshot_with};
    let mut app = ide_app();
    app.name = "Terminal".to_owned();
    let mut window = titled_window(Some("main.rs"));
    let (snapshot, decision) = capture_value_snapshot_with(
        &mut window,
        &ide_policy("block"),
        &app,
        true,
        || {},
        || panic!("non-IDE app does not require title acquisition"),
        |allowed| {
            value_snapshot_with(
                fake_field_snapshot(Some("AXTextArea")),
                allowed,
                || panic!("disallowed app must not read body"),
                || Ok(Some(12)),
            )
        },
    );
    assert!(snapshot.value.is_none());
    assert!(!decision.expect("decision").is_allowed());
}

#[test]
fn dirty_title_changes_preserve_input_but_other_surfaces_reset_it() {
    use super::element::{capture_value_snapshot_with, value_snapshot_with};
    for (old, current, old_id, current_id, kept) in [
        ("main.rs - A", "● main.rs - A", Some(11), Some(11), true),
        ("● main.rs - A", "main.rs - A", Some(11), Some(11), true),
        ("main.rs - A", "• main.rs - A", Some(11), Some(11), true),
        ("• main.rs - A", "main.rs - A", Some(11), Some(11), true),
        ("main.rs - A", "● other.rs - A", Some(11), Some(11), false),
        ("main.rs - A", "● main.rs - B", Some(11), Some(11), false),
        ("main.rs - A", "● main.rs - A", Some(11), Some(12), false),
        ("main.rs - A", "● main.rs - A", None, None, false),
        ("main.rs - A", "main.rs - A", None, None, true),
        ("main.rs - A", "● ● main.rs - A", Some(11), Some(11), false),
        (" main.rs - A", "● main.rs - A", Some(11), Some(11), false),
    ] {
        let now = Instant::now();
        let (publisher, mut authorizations) = input_authorization_channel();
        let mut context = FocusedValueContext::new(
            Some(super::NativeWindow {
                id: old_id,
                title: Some(old.to_owned()),
            }),
            NativeElement {
                role: Some("AXTextArea".to_owned()),
                subrole: None,
                title: None,
                value: None,
                value_len: None,
            },
            true,
            Some("A".to_owned()),
            1,
            FieldClass::KnownText(FieldKind::Text),
        );
        publisher.prepare(7, 1, now).expect("typed input").confirm();
        let acquired = Some(super::NativeWindow {
            id: current_id,
            title: Some(current.to_owned()),
        });
        let (snapshot, decision) = capture_value_snapshot_with(
            &mut context.window,
            &ide_policy("block"),
            &ide_app(),
            true,
            || {
                context
                    .capture
                    .transition_class(7, 1, FieldClass::Unknown, &mut authorizations)
            },
            || Ok(acquired.clone()),
            |allowed| {
                value_snapshot_with(
                    fake_field_snapshot(Some("AXTextArea")),
                    allowed,
                    || Ok(Some("Ax".to_owned())),
                    || Ok(Some(2)),
                )
            },
        );
        let observation =
            context.observation(7, now, time::OffsetDateTime::UNIX_EPOCH, snapshot, decision);
        context.capture.observe(observation, &mut authorizations);
        let emission = context
            .capture
            .flush_pending(&mut authorizations)
            .expect("value metadata");
        let NativeAxEvent::UiValueChanged(event) = context.value_event(7, emission) else {
            panic!("value event")
        };
        assert_eq!(
            event.text.as_deref(),
            kept.then_some("x"),
            "{old:?} -> {current:?}"
        );
        assert_eq!(event.window, acquired);
    }
}

#[test]
fn post_read_dirty_marker_keeps_body_and_read_decision_with_latest_raw_title() {
    use super::element::{capture_value_snapshot_with, value_snapshot_with};
    for (before, after) in [("main.rs", "● main.rs"), ("• main.rs", "main.rs")] {
        let policy = ide_policy("block");
        let app = ide_app();
        let mut window = titled_window(Some(before));
        let expected_decision = policy.decision(
            zanei_core::privacy::PrivacyScope::TextContent,
            &app,
            Some(11),
            Some(before),
        );
        let read = Cell::new(false);
        let (snapshot, decision) = capture_value_snapshot_with(
            &mut window,
            &policy,
            &app,
            true,
            || panic!("dirty marker must preserve input authorization"),
            || Ok(titled_window(Some(if read.get() { after } else { before }))),
            |allowed| {
                value_snapshot_with(
                    fake_field_snapshot(Some("AXTextArea")),
                    allowed,
                    || {
                        read.set(true);
                        Ok(Some("edited".to_owned()))
                    },
                    || Ok(Some(6)),
                )
            },
        );
        assert_eq!(snapshot.value.as_deref(), Some("edited"));
        assert_eq!(decision, Some(expected_decision));
        assert_eq!(window, titled_window(Some(after)));
    }
}
