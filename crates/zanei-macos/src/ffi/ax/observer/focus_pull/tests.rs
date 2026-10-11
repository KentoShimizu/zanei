use std::{
    cell::Cell,
    time::{Duration, Instant},
};

use time::OffsetDateTime;
use zanei_core::schema::FieldKind;

use super::{AppObserver, CaptureValue, FocusSource, ValueRead};
use crate::{
    ffi::ax::{
        NativeAxEvent, NativeAxObservation,
        cf::cf_string,
        element::{capture_value_snapshot_with, value_snapshot_with},
        native_error,
        tests::fake_field_snapshot,
    },
    focused_field::FieldClass,
    text_capture::{InputAuthorizations, ValueCapture, input_authorization_channel},
};

const AX_ERROR_CANNOT_COMPLETE: i32 = -25_204;
const TEXT: FieldClass = FieldClass::KnownText(FieldKind::Text);

fn pull_same_target(observer: &mut AppObserver) -> Vec<NativeAxObservation> {
    let (_publisher, mut authorizations) = input_authorization_channel();
    let reads = Reads {
        role: Some("AXGroup"),
        value: None,
    };
    pull(observer, reads, Instant::now(), &mut authorizations).observations
}

/// Fake AX reads for one pull of the unchanged target: `role` answers a classification refresh
/// and `value` a value read (`Err` is an AX error code). `None` fails the test if that read
/// happens.
#[derive(Clone, Copy, Default)]
struct Reads<'a> {
    role: Option<&'a str>,
    value: Option<Result<&'a str, i32>>,
}

fn value(body: &str) -> Reads<'_> {
    Reads {
        role: None,
        value: Some(Ok(body)),
    }
}

/// Reads a text area whose value is `value` (`Err` is an AX error code).
fn fake_value_read(value: Result<&str, i32>) -> impl CaptureValue + '_ {
    move |_, window, policy, app, enabled, _, changed| {
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
                    || {
                        value
                            .map(|body| Some(body.to_owned()))
                            .map_err(|code| native_error("AXUIElementCopyAttributeValue", code))
                    },
                    || {
                        Ok(value
                            .ok()
                            .and_then(|body| i64::try_from(body.chars().count()).ok()))
                    },
                )
            },
        )
    }
}

struct Pulled {
    observations: Vec<NativeAxObservation>,
    refreshed: bool,
    read: bool,
}

fn pull(
    observer: &mut AppObserver,
    reads: Reads<'_>,
    at: Instant,
    authorizations: &mut InputAuthorizations,
) -> Pulled {
    let refreshed = Cell::new(false);
    let read = Cell::new(false);
    let observations = observer.pull_focused_field_with(
        "edit",
        || Ok(cf_string("fake focused element")),
        |observer, _, authorizations| {
            refreshed.set(true);
            let role = reads.role.expect("this pull must not classify the target");
            let before = observer.fake_focused_field_class();
            assert!(
                observer
                    .refresh_current_field_class_with(
                        fake_field_snapshot(Some(role)),
                        authorizations,
                        || Ok(()),
                        || Ok(()),
                    )
                    .is_ok()
            );
            before != observer.fake_focused_field_class()
        },
        |element, window, policy, app, enabled, secure, changed| {
            read.set(true);
            let value = reads.value.expect("this pull must not read the value");
            fake_value_read(value)(element, window, policy, app, enabled, secure, changed)
        },
        at,
        OffsetDateTime::UNIX_EPOCH,
        false,
        authorizations,
    );
    Pulled {
        observations,
        refreshed: refreshed.get(),
        read: read.get(),
    }
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

/// Installs the same target again as a new generation, as when focus leaves and returns.
fn refocus(observer: &mut AppObserver) {
    let previous = observer.focused_target.transition::<()>(Ok(None));
    let mut target = previous.ok().flatten().expect("fake focused target");
    target.context.generation = observer.focused_target.next_generation();
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
        |_, _, _| panic!("a failed discovery read is not followed by a classification read"),
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

    let pulled = pull(
        &mut observer,
        value("こんにちは"),
        now + Duration::from_millis(150),
        &mut authorizations,
    );

    assert!(pulled.read);
    assert!(!pulled.refreshed);
    assert!(is_public_focus(&pulled.observations));
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
    assert!(pull(&mut observer, value("a"), now, &mut authorizations).read);
    assert!(pull(&mut observer, value("a"), now, &mut authorizations).read);

    observer.value_changed_events_with(
        ValueRead::Notification,
        now,
        OffsetDateTime::UNIX_EPOCH,
        false,
        &mut authorizations,
        fake_value_read(Ok("ab")),
        |observer, class| {
            observer
                .reconcile_current_value_notification_with(class, || Ok(()), || Ok(()))
                .is_ok()
        },
    );
    let no_reads = Reads::default();
    assert!(!pull(&mut observer, no_reads, now, &mut authorizations).read);

    refocus(&mut observer);
    assert!(pull(&mut observer, value("ab"), now, &mut authorizations).read);
}

// Bug: focused buttons or sliders of a silent app emit `ui.value` on every input, a healthy text
// field pays for a classification read on every pull, or a Secure Input poll or failed read
// stops sampling a text field for the rest of its focus.
#[test]
fn pulls_reclassify_only_degraded_targets_and_sample_only_text() {
    for (class, refreshes, reads) in [
        (FieldClass::KnownSafeNonText, false, false),
        (TEXT, false, true),
        (FieldClass::SecureText, true, true),
    ] {
        let mut observer = AppObserver::fake_with_unknown_focused_target();
        text_target(&mut observer, class);
        let (_publisher, mut authorizations) = input_authorization_channel();
        let reads_for_class = Reads {
            role: refreshes.then_some("AXTextArea"),
            value: reads.then_some(Ok("a")),
        };
        let pulled = pull(
            &mut observer,
            reads_for_class,
            Instant::now(),
            &mut authorizations,
        );
        assert_eq!(
            (pulled.refreshed, pulled.read),
            (refreshes, reads),
            "{class:?}"
        );
        if reads {
            assert_eq!(observer.fake_focused_field_class(), TEXT, "{class:?}");
        }
    }
}

// Bug: a silent text field that was unclassified and unregistered when it was found (no AX role
// yet), or demoted by an AX timeout on a value read, is never classified again, so its typed text
// is not captured for the rest of its focus.
#[test]
fn an_unclassified_or_demoted_target_recovers_on_a_later_pull() {
    for demoted_by_failed_read in [false, true] {
        let mut observer = AppObserver::fake_with_unknown_focused_target();
        let (publisher, mut authorizations) = input_authorization_channel();
        let now = Instant::now();
        let generation = if demoted_by_failed_read {
            let generation = text_target(&mut observer, TEXT);
            let failed = Reads {
                role: None,
                value: Some(Err(AX_ERROR_CANNOT_COMPLETE)),
            };
            assert!(pull(&mut observer, failed, now, &mut authorizations).read);
            generation
        } else {
            observer.capture_text_content = true;
            let target = observer.focused_target.current_mut().expect("fake target");
            target.context.capture = ValueCapture::new(true, None, FieldClass::Unknown);
            target.context.generation
        };
        assert_eq!(observer.fake_focused_field_class(), FieldClass::Unknown);

        let recovered = Reads {
            role: Some("AXTextArea"),
            value: Some(Ok("ab")),
        };
        let pulled = pull(&mut observer, recovered, now, &mut authorizations);
        assert!(pulled.refreshed && pulled.read);
        assert_eq!(observer.fake_focused_field_class(), TEXT);

        let typed_at = now + Duration::from_millis(200);
        publisher
            .prepare(7, generation, typed_at)
            .expect("reservation")
            .confirm();
        let at = typed_at + Duration::from_millis(150);
        let pulled = pull(&mut observer, value("abc"), at, &mut authorizations);
        assert!(!pulled.refreshed && pulled.read);
        assert_eq!(
            texts(observer.flush_pending(false, &mut authorizations)),
            ["c"],
            "demoted_by_failed_read={demoted_by_failed_read}"
        );
    }
}
