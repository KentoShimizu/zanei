use std::time::Instant;

use time::OffsetDateTime;

use super::{AppObserver, FocusSource};
use crate::{
    ffi::ax::{NativeAxEvent, NativeAxObservation, cf::cf_string, native_error},
    text_capture::input_authorization_channel,
};

const AX_ERROR_CANNOT_COMPLETE: i32 = -25_204;

fn pull_same_target(observer: &mut AppObserver) -> Vec<NativeAxObservation> {
    let (_publisher, mut authorizations) = input_authorization_channel();
    observer.pull_focused_field_with(
        "edit",
        || Ok(cf_string("fake focused element")),
        Instant::now(),
        OffsetDateTime::UNIX_EPOCH,
        false,
        &mut authorizations,
    )
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
