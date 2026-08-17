use std::time::{Duration, Instant};

use zanei_core::schema::FieldKind;

use super::{
    AUTHORIZATION_QUEUE_CAPACITY, FocusChangeCapture, FocusedTarget, INPUT_WINDOW,
    InputAuthorizations, VALUE_DEBOUNCE, VALUE_MAX_HOLD, ValueCapture, ValueEmission,
    ValueObservation, input_authorization_channel,
};
use crate::focused_field::FieldClass;

fn observation(pid: i32, generation: u64, at: Instant, value: &str) -> ValueObservation {
    ValueObservation {
        pid,
        target_generation: generation,
        notification_at: at,
        value: Some(value.to_owned()),
        value_len: u64::try_from(value.chars().count()).ok(),
        field_class: FieldClass::KnownText(FieldKind::Text),
    }
}

fn authorized_capture(
    pid: i32,
    generation: u64,
    input_at: Instant,
    baseline: &str,
) -> (ValueCapture, InputAuthorizations) {
    let (publisher, authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(pid, generation, input_at)
        .expect("authorization channel should accept the reservation");
    authorization.confirm();
    (
        ValueCapture::new(
            true,
            Some(baseline.to_owned()),
            FieldClass::KnownText(FieldKind::Text),
        ),
        authorizations,
    )
}

#[test]
fn registration_failure_clears_target_and_advances_generation() {
    let mut target = FocusedTarget::new();
    let previous = target
        .transition::<()>(Ok(Some("old")))
        .expect("initial registration should succeed");
    assert_eq!(previous, None);
    assert_eq!(target.generation(), 1);

    let result = target.transition::<&str>(Err("registration failed"));

    assert_eq!(result, Err((Some("old"), "registration failed")));
    assert_eq!(target.current(), None);
    assert_eq!(target.generation(), 2);
}

#[test]
fn unauthorized_transition_advances_baseline_inside_debounce_window() {
    let now = Instant::now();
    let (_unused_publisher, mut authorizations) = input_authorization_channel();
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    assert_eq!(
        capture.observe(observation(7, 1, now, "A secret"), &mut authorizations),
        None
    );

    let (publisher, mut next_authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(7, 1, now + Duration::from_millis(10))
        .expect("authorization channel should accept the reservation");
    authorization.confirm();
    assert_eq!(
        capture.observe(
            observation(7, 1, now + Duration::from_millis(20), "A secretx"),
            &mut next_authorizations,
        ),
        None
    );
    assert_eq!(
        capture.take_due(
            now + VALUE_DEBOUNCE + Duration::from_millis(20),
            &mut next_authorizations
        ),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(9),
            text: Some("x".to_owned()),
        })
    );
}

#[test]
fn rejected_pending_value_is_not_backfilled_by_the_next_authorization() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let first = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the first reservation");
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(observation(7, 1, now, "A secret"), &mut authorizations);
    first.reject();
    let second = publisher
        .prepare(7, 1, now + Duration::from_millis(10))
        .expect("authorization channel should accept the second reservation");
    second.confirm();

    let _ = capture.observe(
        observation(7, 1, now + Duration::from_millis(20), "A secretx"),
        &mut authorizations,
    );

    assert_eq!(
        capture.take_due(
            now + VALUE_DEBOUNCE + Duration::from_millis(20),
            &mut authorizations,
        ),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(9),
            text: Some("x".to_owned()),
        })
    );
}

#[test]
fn late_confirmation_before_debounce_preserves_the_complete_delta() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let first = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the first reservation");
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(observation(7, 1, now, "A secret"), &mut authorizations);
    let second = publisher
        .prepare(7, 1, now + Duration::from_millis(10))
        .expect("authorization channel should accept the second reservation");
    second.confirm();

    let _ = capture.observe(
        observation(7, 1, now + Duration::from_millis(20), "A secretx"),
        &mut authorizations,
    );
    first.confirm();

    assert_eq!(
        capture.take_due(
            now + VALUE_DEBOUNCE + Duration::from_millis(20),
            &mut authorizations,
        ),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(9),
            text: Some(" secretx".to_owned()),
        })
    );
}

#[test]
fn confirmed_windows_coalesce_and_remain_available_until_expiry() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let first = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the first reservation");
    first.confirm();
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(observation(7, 1, now, "A secret"), &mut authorizations);
    let second = publisher
        .prepare(7, 1, now + Duration::from_millis(10))
        .expect("authorization channel should accept the second reservation");
    second.confirm();
    let _ = capture.observe(
        observation(7, 1, now + Duration::from_millis(20), "A secretx"),
        &mut authorizations,
    );

    assert_eq!(
        capture.take_due(
            now + VALUE_DEBOUNCE + Duration::from_millis(20),
            &mut authorizations,
        ),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(9),
            text: Some(" secretx".to_owned()),
        })
    );

    let third_at = now + VALUE_DEBOUNCE + Duration::from_millis(30);
    let _ = capture.observe(
        observation(7, 1, third_at, "A secretxy"),
        &mut authorizations,
    );
    assert_eq!(
        capture.take_due(third_at + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(10),
            text: Some("y".to_owned()),
        })
    );
}

#[test]
fn rejected_keystroke_does_not_close_a_confirmed_window() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let first = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the first reservation");
    first.confirm();
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(observation(7, 1, now, "Ax"), &mut authorizations);
    let second = publisher
        .prepare(7, 1, now + Duration::from_millis(10))
        .expect("authorization channel should accept the second reservation");
    second.reject();
    let _ = capture.observe(
        observation(7, 1, now + Duration::from_millis(20), "Ax secret"),
        &mut authorizations,
    );

    assert_eq!(
        capture.take_due(
            now + VALUE_DEBOUNCE + Duration::from_millis(20),
            &mut authorizations,
        ),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(9),
            text: Some("x secret".to_owned()),
        })
    );
}

#[test]
fn one_confirmed_keystroke_authorizes_multiple_observations_within_its_window() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the reservation");
    authorization.confirm();
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(observation(7, 1, now, "Ab"), &mut authorizations);

    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: Some("b".to_owned()),
        })
    );

    let second_at = now + VALUE_DEBOUNCE + Duration::from_millis(10);
    let _ = capture.observe(observation(7, 1, second_at, "Abc"), &mut authorizations);

    assert_eq!(
        capture.take_due(second_at + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(3),
            text: Some("c".to_owned()),
        })
    );
}

#[test]
fn one_confirmed_keystroke_authorizes_multiple_observations_in_one_batch() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the reservation");
    authorization.confirm();
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(observation(7, 1, now, "Ab"), &mut authorizations);
    let second_at = now + Duration::from_millis(10);
    let _ = capture.observe(observation(7, 1, second_at, "Abc"), &mut authorizations);

    assert_eq!(
        capture.take_due(second_at + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(3),
            text: Some("bc".to_owned()),
        })
    );
}

#[test]
fn rejected_attempt_does_not_close_a_confirmed_window() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let first = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the reservation");
    first.confirm();
    publisher
        .reject_attempt(Some(7), now)
        .expect("authorization channel should accept the barrier");

    assert!(authorizations.matching_for_test(7, 1, now));
}

#[test]
fn matching_does_not_retire_confirmed_windows() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let first = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the first reservation");
    first.confirm();
    let second = publisher
        .prepare(7, 1, now + Duration::from_millis(10))
        .expect("authorization channel should accept the second reservation");
    second.confirm();
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(
        observation(7, 1, now + Duration::from_millis(20), "Ax"),
        &mut authorizations,
    );
    assert_eq!(
        capture.take_due(
            now + VALUE_DEBOUNCE + Duration::from_millis(20),
            &mut authorizations,
        ),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: Some("x".to_owned()),
        })
    );

    let next_at = now + VALUE_DEBOUNCE + Duration::from_millis(30);
    let _ = capture.observe(observation(7, 1, next_at, "Axy"), &mut authorizations);
    assert_eq!(
        capture.take_due(next_at + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(3),
            text: Some("y".to_owned()),
        })
    );
}

#[test]
fn failed_queue_publish_clears_previous_reservations() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let first = publisher
        .prepare(7, 1, now)
        .expect("authorization queue should accept the first keystroke");
    first.confirm();
    for sequence in 1..AUTHORIZATION_QUEUE_CAPACITY {
        publisher
            .prepare(7, 1, now + Duration::from_nanos(sequence as u64))
            .expect("authorization queue should have capacity");
    }
    assert!(publisher.reject_attempt(Some(7), now).is_err());

    assert!(!authorizations.matching_for_test(7, 1, now + Duration::from_millis(1)));
}

#[test]
fn queue_integrity_loss_before_flush_invalidates_a_matched_window() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let first = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the first keystroke");
    first.confirm();
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    assert_eq!(
        capture.observe(observation(7, 1, now, "Ax"), &mut authorizations),
        None
    );

    for sequence in 0..AUTHORIZATION_QUEUE_CAPACITY {
        publisher
            .prepare(7, 1, now + Duration::from_nanos(sequence as u64 + 1))
            .expect("authorization queue should have capacity");
    }
    assert!(publisher.reject_attempt(Some(7), now).is_err());

    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: None,
        })
    );
}

#[test]
fn authorization_requires_matching_pid() {
    let now = Instant::now();
    let (mut capture, mut authorizations) = authorized_capture(8, 1, now, "A");
    let _ = capture.observe(observation(7, 1, now, "Ax"), &mut authorizations);
    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: None,
        })
    );
}

#[test]
fn authorization_requires_matching_target_generation() {
    let now = Instant::now();
    let (mut capture, mut authorizations) = authorized_capture(7, 1, now, "A");
    let _ = capture.observe(observation(7, 2, now, "Ax"), &mut authorizations);
    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: None,
        })
    );
}

#[test]
fn authorization_expires_after_three_seconds() {
    let now = Instant::now();
    let (mut capture, mut authorizations) = authorized_capture(7, 1, now, "A");
    let _ = capture.observe(
        observation(7, 1, now + INPUT_WINDOW + Duration::from_nanos(1), "Ax"),
        &mut authorizations,
    );
    assert_eq!(
        capture.take_due(
            now + INPUT_WINDOW + VALUE_DEBOUNCE + Duration::from_nanos(1),
            &mut authorizations,
        ),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: None,
        })
    );
}

#[test]
fn authorization_is_valid_at_three_second_boundary() {
    let now = Instant::now();
    let (mut capture, mut authorizations) = authorized_capture(7, 1, now, "A");
    let _ = capture.observe(
        observation(7, 1, now + INPUT_WINDOW, "Ax"),
        &mut authorizations,
    );

    assert_eq!(
        capture.take_due(now + INPUT_WINDOW + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: Some("x".to_owned()),
        })
    );
}

#[test]
fn confirmed_window_survives_an_emission() {
    let now = Instant::now();
    let (mut capture, mut authorizations) = authorized_capture(7, 1, now, "A");
    let _ = capture.observe(observation(7, 1, now, "Ax"), &mut authorizations);
    assert!(
        capture
            .take_due(now + VALUE_DEBOUNCE, &mut authorizations)
            .is_some()
    );
    let _ = capture.observe(
        observation(7, 1, now + VALUE_DEBOUNCE, "Axy"),
        &mut authorizations,
    );
    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE * 2, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(3),
            text: Some("y".to_owned()),
        })
    );
}

#[test]
fn late_worker_confirmation_authorizes_the_observed_value() {
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
        observation(7, 1, now + Duration::from_millis(10), "Ax"),
        &mut authorizations,
    );
    authorization.confirm();

    assert_eq!(
        capture.take_due(
            now + VALUE_DEBOUNCE + Duration::from_millis(10),
            &mut authorizations,
        ),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: Some("x".to_owned()),
        })
    );
}

#[test]
fn focus_change_defers_duplicate_value_until_late_confirmation() {
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
    let observed_at = now + Duration::from_millis(10);
    let _ = capture.observe(observation(7, 1, observed_at, "Ax"), &mut authorizations);

    assert!(matches!(
        capture.resolve_focus_change(
            observation(7, 1, observed_at + Duration::from_millis(10), "Ax"),
            &mut authorizations,
        ),
        FocusChangeCapture::Defer
    ));
    authorization.confirm();

    assert_eq!(
        capture.take_due(observed_at + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: Some("x".to_owned()),
        })
    );
}

#[test]
fn deferred_focus_change_fails_closed_after_worker_rejection() {
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
    let _ = capture.observe(observation(7, 1, now, "Ax"), &mut authorizations);
    assert!(matches!(
        capture.resolve_focus_change(observation(7, 1, now, "Ax"), &mut authorizations),
        FocusChangeCapture::Defer
    ));
    authorization.reject();

    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: None,
        })
    );
}

#[test]
fn confirmed_focus_change_flushes_duplicate_value_immediately() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the reservation");
    authorization.confirm();
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(observation(7, 1, now, "Ax"), &mut authorizations);

    let FocusChangeCapture::Emit(emission) =
        capture.resolve_focus_change(observation(7, 1, now, "Ax"), &mut authorizations)
    else {
        panic!("confirmed authorization should flush immediately");
    };
    assert_eq!(
        emission,
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: Some("x".to_owned()),
        })
    );
}

#[test]
fn changed_focus_snapshot_defers_the_latest_pending_reservation() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let first = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the first reservation");
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(observation(7, 1, now, "Ax"), &mut authorizations);
    let second = publisher
        .prepare(7, 1, now + Duration::from_millis(10))
        .expect("authorization channel should accept the second reservation");

    assert!(matches!(
        capture.resolve_focus_change(
            observation(7, 1, now + Duration::from_millis(20), "Axy"),
            &mut authorizations,
        ),
        FocusChangeCapture::Defer
    ));
    first.confirm();
    second.confirm();

    assert_eq!(
        capture.take_due(
            now + VALUE_DEBOUNCE + Duration::from_millis(20),
            &mut authorizations,
        ),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(3),
            text: Some("xy".to_owned()),
        })
    );
}

#[test]
fn window_matching_ignores_inputs_newer_than_the_notification() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let first = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the first reservation");
    first.confirm();
    let second_at = now + Duration::from_millis(20);
    let second = publisher
        .prepare(7, 1, second_at)
        .expect("authorization channel should accept the second reservation");
    second.confirm();

    assert!(authorizations.matching_for_test(7, 1, now + Duration::from_millis(10)));
    assert!(authorizations.matching_for_test(7, 1, now + Duration::from_millis(30)));
}

#[test]
fn confirmed_keys_without_ax_observations_cannot_create_ui_value_events() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the reservation");
    authorization.confirm();
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );

    assert_eq!(
        capture.take_due(now + Duration::from_millis(3_700), &mut authorizations),
        None
    );
}

#[test]
fn rejected_attempts_do_not_close_a_confirmed_window() {
    let now = Instant::now();
    let cases = [(Some(7), 20_u64), (Some(8), 5_u64)];

    for (rejected_pid, rejected_at_ms) in cases {
        let (publisher, mut authorizations) = input_authorization_channel();
        let authorization = publisher
            .prepare(7, 1, now)
            .expect("authorization channel should accept the reservation");
        authorization.confirm();
        publisher
            .reject_attempt(rejected_pid, now + Duration::from_millis(rejected_at_ms))
            .expect("authorization channel should accept the rejected input");

        assert!(
            authorizations.matching_for_test(7, 1, now + Duration::from_millis(10)),
            "rejected_pid={rejected_pid:?} rejected_at_ms={rejected_at_ms}"
        );
    }
}

#[test]
fn trace_shaped_window_cases_preserve_only_authorized_deltas() {
    enum Step {
        Reserve {
            at_ms: u64,
            generation: u64,
        },
        Confirm(usize),
        Reject(usize),
        RejectAttempt {
            at_ms: u64,
            pid: Option<i32>,
        },
        Observe {
            at_ms: u64,
            generation: u64,
            value: &'static str,
        },
    }

    struct Case {
        name: &'static str,
        baseline: Option<&'static str>,
        steps: Vec<Step>,
        flush_at_ms: u64,
        expected: ValueEmission,
    }

    let cases = [
        Case {
            name: "multiple_observations_share_each_confirmed_window",
            baseline: Some("A"),
            steps: vec![
                Step::Reserve {
                    at_ms: 0,
                    generation: 1,
                },
                Step::Confirm(0),
                Step::Observe {
                    at_ms: 50,
                    generation: 1,
                    value: "Ab",
                },
                Step::Observe {
                    at_ms: 100,
                    generation: 1,
                    value: "Abc",
                },
                Step::Reserve {
                    at_ms: 200,
                    generation: 1,
                },
                Step::Confirm(1),
                Step::Observe {
                    at_ms: 150,
                    generation: 1,
                    value: "Abcd",
                },
                Step::Observe {
                    at_ms: 250,
                    generation: 1,
                    value: "Abcde",
                },
                Step::Observe {
                    at_ms: 300,
                    generation: 1,
                    value: "Abcdef",
                },
            ],
            flush_at_ms: 1_300,
            expected: ValueEmission {
                element_value: None,
                value_len: Some(6),
                text: Some("bcdef".to_owned()),
            },
        },
        Case {
            name: "worker_confirms_after_ax_observations",
            baseline: Some("A"),
            steps: vec![
                Step::Reserve {
                    at_ms: 0,
                    generation: 1,
                },
                Step::Observe {
                    at_ms: 10,
                    generation: 1,
                    value: "Ab",
                },
                Step::Reserve {
                    at_ms: 20,
                    generation: 1,
                },
                Step::Observe {
                    at_ms: 30,
                    generation: 1,
                    value: "Abc",
                },
                Step::Confirm(0),
                Step::Confirm(1),
            ],
            flush_at_ms: 1_030,
            expected: ValueEmission {
                element_value: None,
                value_len: Some(3),
                text: Some("bc".to_owned()),
            },
        },
        Case {
            name: "rejected_worker_keystroke_is_a_privacy_boundary",
            baseline: Some("A"),
            steps: vec![
                Step::Reserve {
                    at_ms: 0,
                    generation: 1,
                },
                Step::Observe {
                    at_ms: 10,
                    generation: 1,
                    value: "A private",
                },
                Step::Reject(0),
                Step::Reserve {
                    at_ms: 20,
                    generation: 1,
                },
                Step::Confirm(1),
                Step::Observe {
                    at_ms: 30,
                    generation: 1,
                    value: "A privatex",
                },
            ],
            flush_at_ms: 1_030,
            expected: ValueEmission {
                element_value: None,
                value_len: Some(10),
                text: Some("x".to_owned()),
            },
        },
        Case {
            name: "invalid_target_attempt_does_not_open_a_window",
            baseline: Some("A"),
            steps: vec![
                Step::RejectAttempt {
                    at_ms: 0,
                    pid: Some(7),
                },
                Step::Observe {
                    at_ms: 100,
                    generation: 1,
                    value: "Ax",
                },
            ],
            flush_at_ms: 1_100,
            expected: ValueEmission {
                element_value: None,
                value_len: Some(2),
                text: None,
            },
        },
        Case {
            name: "outside_window_observation_advances_the_privacy_baseline",
            baseline: Some("A"),
            steps: vec![
                Step::Reserve {
                    at_ms: 0,
                    generation: 1,
                },
                Step::Confirm(0),
                Step::Observe {
                    at_ms: 100,
                    generation: 1,
                    value: "Ab",
                },
                Step::Observe {
                    at_ms: 600,
                    generation: 1,
                    value: "Ab",
                },
                Step::Observe {
                    at_ms: 1_100,
                    generation: 1,
                    value: "Ab",
                },
                Step::Observe {
                    at_ms: 1_600,
                    generation: 1,
                    value: "Ab",
                },
                Step::Observe {
                    at_ms: 2_100,
                    generation: 1,
                    value: "Ab",
                },
                Step::Observe {
                    at_ms: 2_600,
                    generation: 1,
                    value: "Ab",
                },
                Step::Observe {
                    at_ms: 3_001,
                    generation: 1,
                    value: "Ab private",
                },
                Step::Reserve {
                    at_ms: 3_100,
                    generation: 1,
                },
                Step::Confirm(1),
                Step::Observe {
                    at_ms: 3_200,
                    generation: 1,
                    value: "Ab privatez",
                },
            ],
            flush_at_ms: 4_200,
            expected: ValueEmission {
                element_value: None,
                value_len: Some(11),
                text: Some("z".to_owned()),
            },
        },
        Case {
            name: "observation_after_three_seconds_is_unauthorized",
            baseline: Some("A"),
            steps: vec![
                Step::Reserve {
                    at_ms: 0,
                    generation: 1,
                },
                Step::Confirm(0),
                Step::Observe {
                    at_ms: 3_001,
                    generation: 1,
                    value: "Ax",
                },
            ],
            flush_at_ms: 4_001,
            expected: ValueEmission {
                element_value: None,
                value_len: Some(2),
                text: None,
            },
        },
        Case {
            name: "generation_mismatch_still_emits_metadata",
            baseline: Some("A"),
            steps: vec![
                Step::Reserve {
                    at_ms: 0,
                    generation: 1,
                },
                Step::Confirm(0),
                Step::Observe {
                    at_ms: 10,
                    generation: 2,
                    value: "Ab",
                },
            ],
            flush_at_ms: 1_010,
            expected: ValueEmission {
                element_value: None,
                value_len: Some(2),
                text: None,
            },
        },
    ];

    for case in cases {
        let now = Instant::now();
        let (publisher, mut authorizations) = input_authorization_channel();
        let mut capture = ValueCapture::new(
            true,
            case.baseline.map(str::to_owned),
            FieldClass::KnownText(FieldKind::Text),
        );
        let mut reservations = Vec::new();
        for step in case.steps {
            match step {
                Step::Reserve { at_ms, generation } => reservations.push(
                    publisher
                        .prepare(7, generation, now + Duration::from_millis(at_ms))
                        .expect("authorization channel should accept the reservation"),
                ),
                Step::Confirm(index) => reservations[index].confirm(),
                Step::Reject(index) => reservations[index].reject(),
                Step::RejectAttempt { at_ms, pid } => publisher
                    .reject_attempt(pid, now + Duration::from_millis(at_ms))
                    .expect("authorization channel should accept the rejected attempt"),
                Step::Observe {
                    at_ms,
                    generation,
                    value,
                } => {
                    assert_eq!(
                        capture.observe(
                            observation(7, generation, now + Duration::from_millis(at_ms), value),
                            &mut authorizations,
                        ),
                        None,
                        "{} at {at_ms}ms",
                        case.name
                    );
                }
            }
        }

        assert_eq!(
            capture.take_due(
                now + Duration::from_millis(case.flush_at_ms),
                &mut authorizations,
            ),
            Some(case.expected),
            "{}",
            case.name
        );
    }
}

#[test]
fn continuous_trace_shaped_input_flushes_every_five_seconds_without_delta_loss() {
    const TYPING_DURATION_MS: u64 = 30_000;
    const KEY_INTERVAL_MS: u64 = 500;
    const OBSERVATION_OFFSETS_MS: &[&[u64]] = &[&[100, 300], &[100, 200, 400]];

    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let mut current_value = "A".to_owned();
    let mut emitted_text = String::new();
    let mut flush_times_ms = Vec::new();

    for key_index in 0..(TYPING_DURATION_MS / KEY_INTERVAL_MS) {
        let input_at_ms = key_index * KEY_INTERVAL_MS;
        let authorization = publisher
            .prepare(7, 1, now + Duration::from_millis(input_at_ms))
            .expect("authorization channel should accept the reservation");
        authorization.confirm();
        for offset_ms in OBSERVATION_OFFSETS_MS[key_index as usize % 2] {
            let observed_at_ms = input_at_ms + offset_ms;
            current_value.push('x');
            if let Some(emission) = capture.observe(
                observation(
                    7,
                    1,
                    now + Duration::from_millis(observed_at_ms),
                    &current_value,
                ),
                &mut authorizations,
            ) {
                flush_times_ms.push(observed_at_ms);
                emitted_text.push_str(
                    emission
                        .text
                        .as_deref()
                        .expect("every trace-shaped batch should be authorized"),
                );
            }
        }
    }

    let final_flush_at_ms = 30_100;
    let final_emission = capture
        .take_due(
            now + Duration::from_millis(final_flush_at_ms),
            &mut authorizations,
        )
        .expect("the last batch should reach the maximum hold");
    flush_times_ms.push(final_flush_at_ms);
    emitted_text.push_str(
        final_emission
            .text
            .as_deref()
            .expect("the last trace-shaped batch should be authorized"),
    );

    assert_eq!(
        flush_times_ms,
        vec![5_100, 10_100, 15_100, 20_100, 25_100, 30_100]
    );
    assert!(
        flush_times_ms
            .windows(2)
            .all(|times| times[1] - times[0] <= VALUE_MAX_HOLD.as_millis() as u64)
    );
    assert_eq!(emitted_text, "x".repeat(current_value.len() - 1));
}

#[test]
fn unreadable_focus_snapshot_preserves_pending_authorization() {
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
    let _ = capture.observe(observation(7, 1, now, "Ax"), &mut authorizations);

    assert!(matches!(
        capture.resolve_unreadable_focus_change(&mut authorizations),
        FocusChangeCapture::Defer
    ));
    authorization.confirm();

    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: Some("x".to_owned()),
        })
    );
}

#[test]
fn pending_worker_keystroke_fails_closed_without_consuming_its_window() {
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
    let _ = capture.observe(observation(7, 1, now, "Ax"), &mut authorizations);

    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: None,
        })
    );
    authorization.confirm();
    assert!(authorizations.matching_for_test(7, 1, now + VALUE_DEBOUNCE));
}

#[test]
fn rejected_worker_reservation_fails_closed() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the reservation");
    authorization.reject();
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(observation(7, 1, now, "Ax"), &mut authorizations);

    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: None,
        })
    );
}

#[test]
fn input_after_the_notification_does_not_authorize_the_value() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(7, 1, now + Duration::from_millis(10))
        .expect("authorization channel should accept the reservation");
    authorization.confirm();
    let mut capture = ValueCapture::new(
        true,
        Some("A".to_owned()),
        FieldClass::KnownText(FieldKind::Text),
    );
    let _ = capture.observe(observation(7, 1, now, "Ax"), &mut authorizations);

    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(2),
            text: None,
        })
    );
}

#[test]
fn unknown_role_never_emits_a_value() {
    let now = Instant::now();
    let (publisher, mut authorizations) = input_authorization_channel();
    let authorization = publisher
        .prepare(7, 1, now)
        .expect("authorization channel should accept the reservation");
    authorization.confirm();
    let mut capture = ValueCapture::new(true, None, FieldClass::Unknown);
    let mut unknown = observation(7, 1, now, "document contents");
    unknown.field_class = FieldClass::Unknown;

    let _ = capture.observe(unknown, &mut authorizations);
    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE, &mut authorizations),
        None
    );
}

#[test]
fn unknown_transition_invalidates_baseline_and_authorization() {
    let now = Instant::now();
    let (mut capture, mut authorizations) = authorized_capture(7, 1, now, "A");
    let mut unknown = observation(7, 1, now, "A private");
    unknown.field_class = FieldClass::Unknown;
    let _ = capture.observe(unknown, &mut authorizations);

    let _ = capture.observe(
        observation(7, 1, now + Duration::from_millis(100), "A privatez"),
        &mut authorizations,
    );

    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE * 2, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(10),
            text: None,
        })
    );
}

#[test]
fn secure_transition_invalidates_baseline_and_authorization() {
    let now = Instant::now();
    let (mut capture, mut authorizations) = authorized_capture(7, 1, now, "A");
    let _ = capture.observe(observation(7, 1, now, "Ax"), &mut authorizations);
    let mut secure = observation(7, 1, now + Duration::from_millis(100), "Ax secret");
    secure.field_class = FieldClass::SecureText;
    let _ = capture.observe(secure, &mut authorizations);

    let _ = capture.observe(
        observation(7, 1, now + Duration::from_millis(200), "Ax secretz"),
        &mut authorizations,
    );

    assert_eq!(
        capture.take_due(now + VALUE_DEBOUNCE * 2, &mut authorizations),
        Some(ValueEmission {
            element_value: None,
            value_len: Some(10),
            text: None,
        })
    );
}
