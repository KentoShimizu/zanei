use std::cell::Cell;

use super::*;
use crate::ffi::{
    ax::{
        element::{capture_value_snapshot_with, value_snapshot_with},
        tests::{fake_field_snapshot, ide_app, ide_policy},
    },
    geometry::AxSize,
};
use zanei_core::{privacy::PrivacyScope, schema::App};

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

fn titled(title: &str) -> NativeWindow {
    NativeWindow {
        title: Some(title.to_owned()),
        id: Some(11),
    }
}

fn plain_app() -> App {
    App {
        name: "Editor".to_owned(),
        bundle_id: Some("org.example.editor".to_owned()),
        pid: Some(7),
    }
}

struct ValueRead {
    body: Option<String>,
    body_reads: usize,
    window: Option<NativeWindow>,
}

/// Reads a text field's value the way the value lifecycle does, for a field whose
/// element names no window and whose application resolved `resolved` at focus.
fn read_value(resolved: NativeWindow, policy: &crate::CapturePolicy, app: &App) -> ValueRead {
    let resolved = if attributes_resolved_windows(app) {
        ElementWindow::Attributed(resolved)
    } else {
        ElementWindow::Unknown
    };
    let mut window = resolved.into_attribution();
    let body_reads = Cell::new(0);
    let (snapshot, _) = capture_value_snapshot_with(
        &mut window,
        policy,
        app,
        true,
        || {},
        || Ok(None),
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
    ValueRead {
        body: snapshot.value,
        body_reads: body_reads.get(),
        window,
    }
}

#[test]
fn title_policy_ide_treats_a_resolved_window_as_unknown() {
    // The resolved title would allow the read; an unknown title is blocked.
    let policy = ide_policy("block");
    let decide = |window: &ElementWindow| {
        let window = window.decision_window();
        policy
            .decision(
                PrivacyScope::TextContent,
                &ide_app(),
                window.and_then(|window| window.id),
                window.and_then(|window| window.title.as_deref()),
            )
            .is_allowed()
    };
    assert!(decide(&ElementWindow::Own(titled("main.rs"))));
    assert!(!decide(&ElementWindow::Attributed(titled("main.rs"))));

    let read = read_value(titled("main.rs"), &policy, &ide_app());
    assert_eq!(read.body_reads, 0);
    assert_eq!(read.window, None);
}

#[test]
fn plain_app_captures_with_the_resolved_window_attributed() {
    let read = read_value(titled("Notes"), &ide_policy("standalone"), &plain_app());

    assert_eq!(read.body.as_deref(), Some("body"));
    assert_eq!(read.window, Some(titled("Notes")));
}

#[test]
fn browsers_never_get_a_resolved_window() {
    // Browser capture is decided per window id, so a resolved id must never reach it.
    let browser = App {
        bundle_id: Some(BrowserTarget::Chrome.bundle_id().to_owned()),
        ..plain_app()
    };

    assert!(!attributes_resolved_windows(&browser));
    assert!(attributes_resolved_windows(&plain_app()));
}

#[test]
fn click_resolves_to_the_frontmost_overlapping_window() {
    let windows = [
        on_screen(1, 0, rect(100.0, 100.0, 200.0, 200.0)),
        on_screen(2, 0, rect(0.0, 0.0, 800.0, 600.0)),
    ];
    let clicked = |x, y| clicked_on_screen(&windows, AxPoint { x, y }).map(|window| window.id);

    assert_eq!(clicked(150.0, 150.0), Some(1));
    assert_eq!(clicked(50.0, 50.0), Some(2));
    assert_eq!(clicked(900.0, 50.0), None);
}

#[test]
fn click_on_a_panel_is_not_attributed_to_the_window_beneath() {
    let windows = [
        on_screen(1, 3, rect(100.0, 100.0, 200.0, 200.0)),
        on_screen(2, 0, rect(0.0, 0.0, 800.0, 600.0)),
    ];

    assert_eq!(
        clicked_on_screen(&windows, AxPoint { x: 150.0, y: 150.0 }).map(|window| window.id),
        None
    );
}

#[test]
fn click_matches_by_window_number_even_among_shared_frames() {
    let frame = rect(0.0, 0.0, 800.0, 600.0);
    let on_screen = [on_screen(1, 0, frame), on_screen(2, 0, frame)];
    let identities = [AxWindowIdentity::Number(1), AxWindowIdentity::Number(2)];

    assert_eq!(
        matching_window(&identities, &on_screen, &on_screen[1]),
        Some(1)
    );
}

#[test]
fn click_on_an_ambiguous_frame_is_dropped() {
    let frame = rect(0.0, 0.0, 800.0, 600.0);
    let other = rect(10.0, 10.0, 300.0, 200.0);
    let unique = [AxWindowIdentity::Frame(Some(frame))];
    let shared = [
        AxWindowIdentity::Frame(Some(frame)),
        AxWindowIdentity::Frame(Some(frame)),
    ];
    let one_on_screen = [on_screen(1, 0, frame), on_screen(2, 0, other)];
    let two_on_screen = [on_screen(1, 0, frame), on_screen(2, 0, frame)];

    assert_eq!(
        matching_window(&unique, &one_on_screen, &one_on_screen[0]),
        Some(0)
    );
    assert_eq!(
        matching_window(&shared, &one_on_screen, &one_on_screen[0]),
        None,
        "two AX windows share the frame"
    );
    assert_eq!(
        matching_window(&unique, &two_on_screen, &two_on_screen[0]),
        None,
        "two on-screen windows share the frame"
    );
}
