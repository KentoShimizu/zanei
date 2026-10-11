use std::cell::Cell;

use super::*;
use crate::ffi::{
    ax::{
        element::{capture_value_snapshot_verified, value_snapshot_with},
        tests::{fake_field_snapshot, ide_app, ide_policy},
    },
    geometry::AxSize,
};

fn rect(x: f64, y: f64, width: f64, height: f64) -> AxFrame {
    AxFrame {
        origin: AxPoint { x, y },
        size: AxSize { width, height },
    }
}

fn on_screen(id: i64, layer: i64, bounds: AxFrame) -> OnScreenWindow {
    OnScreenWindow {
        id,
        layer,
        title: None,
        bounds,
    }
}

fn titled(title: &str) -> Option<NativeWindow> {
    Some(NativeWindow {
        title: Some(title.to_owned()),
        id: Some(11),
    })
}

fn clicked_id(on_screen: &[OnScreenWindow], x: f64, y: f64) -> Option<i64> {
    clicked_window(on_screen, AxPoint { x, y }).map(|window| window.id)
}

#[test]
fn click_resolves_to_the_frontmost_overlapping_window() {
    // Picking a back window would apply the wrong window's capture policy.
    let windows = [
        on_screen(1, 0, rect(100.0, 100.0, 200.0, 200.0)),
        on_screen(2, 0, rect(0.0, 0.0, 800.0, 600.0)),
    ];

    assert_eq!(clicked_id(&windows, 150.0, 150.0), Some(1));
    assert_eq!(clicked_id(&windows, 50.0, 50.0), Some(2));
    assert_eq!(clicked_id(&windows, 900.0, 50.0), None);
}

#[test]
fn click_on_a_panel_is_not_attributed_to_the_window_beneath() {
    let windows = [
        on_screen(1, 3, rect(100.0, 100.0, 200.0, 200.0)),
        on_screen(2, 0, rect(0.0, 0.0, 800.0, 600.0)),
    ];

    assert_eq!(clicked_id(&windows, 150.0, 150.0), None);
    assert_eq!(clicked_id(&windows, 50.0, 50.0), Some(2));
}

#[test]
fn same_frame_windows_resolve_by_window_number() {
    let frame = rect(0.0, 0.0, 800.0, 600.0);
    let identities = [AxWindowIdentity::Number(1), AxWindowIdentity::Number(2)];

    assert_eq!(
        matching_window(&identities, &on_screen(2, 0, frame)),
        Some((1, 2))
    );
    assert_eq!(matching_window(&identities, &on_screen(3, 0, frame)), None);
}

#[test]
fn same_frame_windows_without_numbers_stay_unresolved() {
    let frame = rect(0.0, 0.0, 800.0, 600.0);
    let identities = [
        AxWindowIdentity::Frame(Some(frame)),
        AxWindowIdentity::Frame(Some(frame)),
    ];

    assert_eq!(matching_window(&identities, &on_screen(2, 0, frame)), None);
}

#[test]
fn unique_frame_without_a_number_resolves() {
    let frame = rect(0.0, 0.0, 800.0, 600.0);
    let identities = [
        AxWindowIdentity::Number(1),
        AxWindowIdentity::Frame(Some(rect(10.0, 10.0, 300.0, 200.0))),
        AxWindowIdentity::Frame(None),
        AxWindowIdentity::Frame(Some(frame)),
    ];

    assert_eq!(
        matching_window(&identities, &on_screen(2, 0, frame)),
        Some((3, 2))
    );
}

#[test]
fn click_keeps_the_matched_id_when_same_frame_windows_reorder() {
    let frame = rect(0.0, 0.0, 800.0, 600.0);
    let identities = [
        AxWindowIdentity::Number(2),
        AxWindowIdentity::Frame(Some(frame)),
    ];
    let matched = matching_window(&identities, &on_screen(1, 0, frame));
    // By the time the window is snapshotted, the other window is listed first;
    // re-deriving the id from that list would pair this title with window 2.
    let reordered = [on_screen(2, 0, frame), on_screen(1, 0, frame)];

    assert_eq!(window_id_for_frame_in_windows(&reordered, frame), Some(2));
    assert_eq!(matched, Some((1, 1)));
}

#[test]
fn focused_window_id_from_a_frame_must_be_unique() {
    let frame = rect(0.0, 0.0, 800.0, 600.0);
    let stacked = [on_screen(1, 0, frame), on_screen(2, 0, frame)];
    let single = [
        on_screen(1, 0, rect(10.0, 10.0, 300.0, 200.0)),
        on_screen(2, 0, frame),
    ];

    assert_eq!(unique_window_id_for_frame(&stacked, frame), None);
    assert_eq!(unique_window_id_for_frame(&single, frame), Some(2));
}

#[test]
fn missing_own_window_resolves_through_the_application() {
    let resolved = own_or_application_window::<&str>(
        Ok(None),
        |_| Ok(titled("own")),
        || Ok(ElementWindow::Verified(titled("app"))),
    );

    assert_eq!(resolved.ok(), Some(ElementWindow::Verified(titled("app"))));
}

#[test]
fn own_window_is_kept_without_reading_the_application() {
    let resolved = own_or_application_window(
        Ok(Some("own")),
        |_| Ok(titled("own")),
        || panic!("an element that names its window must not cost an application read"),
    );

    assert_eq!(resolved.ok(), Some(ElementWindow::Verified(titled("own"))));
}

#[test]
fn failed_own_window_read_is_not_replaced_by_a_guess() {
    let resolved = own_or_application_window::<&str>(
        Err(native_error("AXWindow", -25_204)),
        |_| panic!("no window to snapshot"),
        || panic!("a failed read must keep its failure handling"),
    );

    assert_eq!(resolved.map_err(|error| error.code()).err(), Some(-25_204));
}

#[test]
fn focused_element_is_verified_in_the_focused_window() {
    let resolved = focused_element_window(|| Ok(true), || Ok(titled("main.rs")));

    assert_eq!(
        resolved.ok(),
        Some(ElementWindow::Verified(titled("main.rs")))
    );
}

#[test]
fn element_without_focus_is_not_bound_to_the_focused_window() {
    // Focus install and click attribution bind fresh reads to this result.
    let resolved = focused_element_window(
        || Ok(false),
        || panic!("the focused window holds another element"),
    );

    assert_eq!(resolved.ok().map(ElementWindow::into_verified), Some(None));
}

#[test]
fn focus_change_during_the_window_read_does_not_bind_the_new_window() {
    let checks = Cell::new(0);
    let resolved = focused_element_window(
        || {
            checks.set(checks.get() + 1);
            Ok(checks.get() == 1)
        },
        || Ok(titled("main.rs")),
    );

    assert_eq!(checks.get(), 2);
    assert_eq!(resolved.ok(), Some(ElementWindow::Unverified));
}

struct IdeRead {
    snapshot: crate::ffi::ax::element::ValueSnapshot,
    decision: Option<crate::CaptureDecision>,
    window: Option<NativeWindow>,
    body_reads: usize,
    surface_changes: usize,
}

/// Runs the IDE value read for a field recorded under `recorded`, with the
/// element's window resolved before and after the body read as given.
fn ide_value_read(recorded: &str, resolutions: [ElementWindow; 2]) -> IdeRead {
    let mut window = titled(recorded);
    let mut resolutions = resolutions.into_iter();
    let body_reads = Cell::new(0);
    let surface_changes = Cell::new(0);
    let (snapshot, decision) = capture_value_snapshot_verified(
        &mut window,
        &ide_policy("block"),
        &ide_app(),
        true,
        || surface_changes.set(surface_changes.get() + 1),
        || Ok(resolutions.next().expect("at most two window reads")),
        |allowed| {
            value_snapshot_with(
                fake_field_snapshot(Some("AXTextArea")),
                allowed,
                || {
                    body_reads.set(body_reads.get() + 1);
                    Ok(Some("body".to_owned()))
                },
                || Ok(Some(4)),
            )
        },
    );
    IdeRead {
        snapshot,
        decision,
        window,
        body_reads: body_reads.get(),
        surface_changes: surface_changes.get(),
    }
}

#[test]
fn unfocused_field_with_a_stale_recorded_title_reads_no_body() {
    // The recorded title allows capture, but the window may have switched to a
    // blocked file since; with nothing verified, the body must not be read.
    let read = ide_value_read(
        "main.rs",
        [ElementWindow::Unverified, ElementWindow::Unverified],
    );

    assert_eq!(read.body_reads, 0);
    assert_eq!(read.snapshot.value, None);
    assert!(read.snapshot.failure.is_some());
    assert_eq!(read.decision, None);
    assert_eq!(read.window, titled("main.rs"), "kept for attribution");
    assert_eq!(read.surface_changes, 0);
}

#[test]
fn body_read_without_a_verified_window_after_it_is_discarded() {
    let read = ide_value_read(
        "main.rs",
        [
            ElementWindow::Verified(titled("main.rs")),
            ElementWindow::Unverified,
        ],
    );

    assert_eq!(read.snapshot.value, None);
    assert!(read.snapshot.failure.is_some());
    assert_eq!(read.decision, None);
}

#[test]
fn verified_window_switch_is_decided_on_the_current_title() {
    let blocked = ide_value_read(
        "main.rs",
        [
            ElementWindow::Verified(titled(".env")),
            ElementWindow::Verified(titled(".env")),
        ],
    );
    assert_eq!(blocked.body_reads, 0);
    assert_eq!(blocked.window, titled(".env"));
    assert_eq!(blocked.surface_changes, 1);

    let allowed = ide_value_read(
        "main.rs",
        [
            ElementWindow::Verified(titled("main.rs")),
            ElementWindow::Verified(titled("main.rs")),
        ],
    );
    assert_eq!(allowed.snapshot.value.as_deref(), Some("body"));
    assert!(allowed.snapshot.failure.is_none());
}
