use std::{
    cell::Cell,
    time::{Duration, Instant},
};

use time::OffsetDateTime;
use zanei_core::schema::FieldKind;

use super::{AppObserver, FocusSource, ValueRead};
use crate::{
    ffi::ax::{
        NativeAxEvent, NativeAxObservation, NativeElement,
        cf::cf_string,
        element::{capture_value_snapshot_with, value_snapshot_with},
        native_error,
        observer::RegisteredFocusedTarget,
        tests::fake_field_snapshot,
        value_context::FocusedValueContext,
    },
    focused_field::FieldClass,
    text_capture::{InputAuthorizations, ValueCapture, input_authorization_channel},
};

const AX_ERROR_CANNOT_COMPLETE: i32 = -25_204;
const TEXT: FieldClass = FieldClass::KnownText(FieldKind::Text);

fn pull_same_target(observer: &mut AppObserver) -> Vec<NativeAxObservation> {
    let (_publisher, mut authorizations) = input_authorization_channel();
    pull(observer, None, Instant::now(), &mut authorizations).0
}

/// Pulls the unchanged fake target. `body` is what a value read returns; `None` fails the test
/// if the value is read. Returns the observations and whether the value was read.
fn pull(
    observer: &mut AppObserver,
    body: Option<&str>,
    at: Instant,
    authorizations: &mut InputAuthorizations,
) -> (Vec<NativeAxObservation>, bool) {
    let read = Cell::new(false);
    let observations = observer.pull_focused_field_with(
        "edit",
        || Ok(cf_string("fake focused element")),
        |_, window, policy, app, enabled, _, changed| {
            read.set(true);
            let body = body.expect("this pull must not read the value");
            capture_value_snapshot_with(
                window,
                policy,
                app,
                enabled,
                changed,
                || panic!("only IDE surfaces read the window"),
                |allowed| {
                    value_snapshot_with(
                        fake_field_snapshot(Some("AXTextArea")),
                        allowed,
                        || Ok(Some(body.to_owned())),
                        || Ok(i64::try_from(body.chars().count()).ok()),
                    )
                },
            )
        },
        at,
        OffsetDateTime::UNIX_EPOCH,
        false,
        authorizations,
    );
    (observations, read.get())
}

/// Turns the fake target into a registered text field with capture enabled; returns its
/// generation.
fn text_target(observer: &mut AppObserver, class: FieldClass) -> u64 {
    observer.capture_text_content = true;
    let context = &mut observer
        .focused_target
        .current_mut()
        .expect("fake focused target")
        .context;
    context.field_class = class;
    context.capture = ValueCapture::new(true, Some(String::new()), TEXT);
    let generation = context.generation;
    assert!(
        observer
            .reconcile_current_value_notification_with(TEXT, || Ok(()), || Ok(()))
            .is_ok()
    );
    generation
}

/// Installs the same element again as a new generation, as when focus leaves and returns.
fn refocus(observer: &mut AppObserver) {
    let generation = observer.focused_target.next_generation();
    let element = NativeElement {
        role: None,
        subrole: None,
        title: None,
        value: None,
        value_len: None,
        capture_decision: None,
    };
    let target = RegisteredFocusedTarget {
        element: cf_string("fake focused element").expect("element CFString"),
        context: FocusedValueContext::new(
            None,
            element,
            true,
            Some(String::new()),
            generation,
            TEXT,
        ),
    };
    assert!(
        observer
            .focused_target
            .transition::<()>(Ok(Some(target)))
            .is_ok()
    );
}

fn texts(events: Vec<NativeAxEvent>) -> Vec<String> {
    events
        .into_iter()
        .filter_map(|event| match event {
            NativeAxEvent::UiValueChanged(event) => event.text,
            _ => None,
        })
        .collect()
}

fn is_public_focus(observations: &[NativeAxObservation]) -> bool {
    matches!(
        observations,
        [NativeAxObservation::Event(NativeAxEvent::UiFocused {
            pid: 7,
            generation: 1,
            element: Some(_),
            ..
        })]
    )
}

fn is_internal_focus(observations: &[NativeAxObservation]) -> bool {
    matches!(
        observations,
        [NativeAxObservation::FocusedFieldObserved {
            pid: 7,
            focused_field: Some(field),
        }] if field.generation == 1
    )
}

// Bug: a field found at attach is never announced, is announced again on every input, or
// attach-time re-resolution consumes the announcement the first pull owes.
#[test]
fn a_field_found_without_announcement_is_announced_by_the_first_pull_only() {
    let mut observer = AppObserver::fake_with_unknown_focused_target();
    let generation = observer.focused_target.generation();
    assert!(
        observer
            .focus_pull
            .observed(generation, FocusSource::Internal, false)
    );

    assert!(is_public_focus(&pull_same_target(&mut observer)));
    assert!(is_internal_focus(&pull_same_target(&mut observer)));
    assert!(is_internal_focus(&pull_same_target(&mut observer)));
}

// Bug: a native focus notification queued behind a pull duplicates `ui.focus`, drops a class
// change it reports, or suppresses genuine later native re-posts.
#[test]
fn a_native_echo_of_a_pulled_announcement_is_announced_only_with_a_class_change() {
    for class_changed in [false, true] {
        let mut observer = AppObserver::fake_with_unknown_focused_target();
        assert!(is_public_focus(&pull_same_target(&mut observer)));
        let generation = observer.focused_target.generation();
        let pull = &mut observer.focus_pull;

        assert_eq!(
            pull.observed(generation, FocusSource::Notification, class_changed),
            class_changed
        );
        assert!(pull.observed(generation, FocusSource::Notification, false));
        assert!(is_internal_focus(&pull_same_target(&mut observer)));
    }
}

// Bug: an AX timeout on the discovery read is treated as focus loss and drops the field.
#[test]
fn a_failed_discovery_read_keeps_the_target_and_records_the_failure() {
    let mut observer = AppObserver::fake_with_unknown_focused_target();
    let element = observer.fake_focused_element();
    let generation = observer.focused_target.generation();
    let degraded = observer.fake_degraded_operations();
    let (_publisher, mut authorizations) = input_authorization_channel();

    let observations = observer.pull_focused_field_with(
        "edit",
        || {
            Err(native_error(
                "AXUIElementCopyAttributeValue",
                AX_ERROR_CANNOT_COMPLETE,
            ))
        },
        |_, _, _, _, _, _, _| panic!("a failed discovery read is not followed by a value read"),
        Instant::now(),
        OffsetDateTime::UNIX_EPOCH,
        false,
        &mut authorizations,
    );

    assert!(observations.is_empty());
    assert_eq!(observer.focused_target.generation(), generation);
    assert_eq!(observer.fake_focused_element(), element);
    assert_eq!(observer.fake_degraded_operations(), degraded + 1);
    assert!(is_public_focus(&pull_same_target(&mut observer)));
}

// Bug: a field that never posts `AXValueChanged` stays invisible, or text committed by an input
// method is not taken from the field's value.
#[test]
fn a_sampled_silent_field_records_confirmed_text() {
    let mut observer = AppObserver::fake_with_unknown_focused_target();
    let generation = text_target(&mut observer, TEXT);
    let (publisher, mut authorizations) = input_authorization_channel();
    let now = Instant::now();
    publisher
        .prepare(7, generation, now)
        .expect("reservation")
        .confirm();

    let (observations, read) = pull(
        &mut observer,
        Some("こんにちは"),
        now + Duration::from_millis(150),
        &mut authorizations,
    );

    assert!(read);
    assert!(is_public_focus(&observations));
    assert_eq!(
        texts(observer.flush_pending(false, &mut authorizations)),
        ["こんにちは"]
    );
}

// Bug: a notifying field keeps paying for value reads after its first native delivery, a read
// marks the field as notifying, or one delivery disables sampling for later focus generations.
#[test]
fn a_native_value_delivery_stops_samples_until_the_next_generation() {
    let mut observer = AppObserver::fake_with_unknown_focused_target();
    text_target(&mut observer, TEXT);
    let (_publisher, mut authorizations) = input_authorization_channel();
    let now = Instant::now();
    assert!(pull(&mut observer, Some("a"), now, &mut authorizations).1);
    assert!(pull(&mut observer, Some("a"), now, &mut authorizations).1);

    observer.value_changed_events_with(
        ValueRead::Notification,
        now,
        OffsetDateTime::UNIX_EPOCH,
        false,
        &mut authorizations,
        |_, window, policy, app, enabled, _, changed| {
            capture_value_snapshot_with(
                window,
                policy,
                app,
                enabled,
                changed,
                || panic!("only IDE surfaces read the window"),
                |allowed| {
                    value_snapshot_with(
                        fake_field_snapshot(Some("AXTextArea")),
                        allowed,
                        || Ok(Some("ab".to_owned())),
                        || Ok(Some(2)),
                    )
                },
            )
        },
        |observer, class| {
            observer
                .reconcile_current_value_notification_with(class, || Ok(()), || Ok(()))
                .is_ok()
        },
    );
    assert!(!pull(&mut observer, None, now, &mut authorizations).1);

    refocus(&mut observer);
    assert!(pull(&mut observer, Some("ab"), now, &mut authorizations).1);
}

// Bug: focused buttons or sliders of a silent app emit `ui.value` on every input, or a single
// Secure Input poll or failed read stops sampling a text field for the rest of its focus.
#[test]
fn samples_skip_safe_non_text_targets_but_retry_degraded_text_targets() {
    for (class, reads) in [
        (FieldClass::KnownSafeNonText, false),
        (FieldClass::SecureText, true),
        (FieldClass::Unknown, true),
    ] {
        let mut observer = AppObserver::fake_with_unknown_focused_target();
        text_target(&mut observer, class);
        let (_publisher, mut authorizations) = input_authorization_channel();
        let body = reads.then_some("a");
        let (_, read) = pull(&mut observer, body, Instant::now(), &mut authorizations);
        assert_eq!(read, reads, "{class:?}");
        if reads {
            assert_eq!(observer.fake_focused_field_class(), TEXT, "{class:?}");
        }
    }
}
