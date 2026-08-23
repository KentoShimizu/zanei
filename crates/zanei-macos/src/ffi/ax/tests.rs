use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use zanei_core::schema::FieldKind;

use super::{
    NativeAxEvent, NativeElement,
    element::{
        VALUE_CHANGE_READ_SURFACE, ValueFieldSnapshot, focused_element_is_excluded, gated_value,
        value_length,
    },
    runtime::secure_input_active,
    value_context::{
        DeferredResolution, DeferredValueContext, FocusedValueContext, after_target_preparation,
        classified_field_snapshot,
    },
};
use crate::{
    focused_field::FieldClass,
    secure_input::secure_input_test_channel,
    text_capture::{
        FocusedTarget, VALUE_DEBOUNCE, ValueCapture, ValueObservation, input_authorization_channel,
    },
};

#[test]
fn value_change_reclassifies_before_reading_value_and_character_count() {
    assert_eq!(
        VALUE_CHANGE_READ_SURFACE,
        ["AXRole", "AXSubrole", "AXValue", "AXNumberOfCharacters"]
    );
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
    let response = thread::spawn(move || responder.respond_next(true));
    assert!(secure_input_active(true, Some(&probe), &degraded, "test"));
    response.join().expect("Secure Input response thread");

    let (probe, responder) = secure_input_test_channel();
    drop(responder);
    assert!(secure_input_active(true, Some(&probe), &degraded, "test"));
    assert_eq!(degraded.load(Ordering::Relaxed), 1);
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
    let response = thread::spawn(move || responder.respond_next(false));

    assert!(!secure_input_active(true, Some(&probe), &degraded, "test"));
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
    };
    let mut detached = DeferredValueContext::new(7, context);

    authorization.confirm();
    let DeferredResolution::Complete(Some(NativeAxEvent::UiValueChanged { text, .. })) = detached
        .take_due(
            now + VALUE_DEBOUNCE + Duration::from_millis(10),
            false,
            &mut authorizations,
        )
    else {
        panic!("detached value should resolve into a value event");
    };
    assert_eq!(text.as_deref(), Some("x"));
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
        },
        &mut authorizations,
    );
    let degraded = ValueFieldSnapshot {
        role: None,
        subrole: None,
        field_class: FieldClass::Unknown,
        degraded: true,
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
        degraded: false,
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
    let DeferredResolution::Complete(Some(NativeAxEvent::UiValueChanged { text, .. })) =
        deferred.take_due(now + VALUE_DEBOUNCE, false, &mut authorizations)
    else {
        panic!("deferred previous value should resolve after late confirmation");
    };
    assert_eq!(text.as_deref(), Some("x"));
}
