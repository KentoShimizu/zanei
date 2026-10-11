//! Resolution of the window that owns an AX element.
//!
//! An element that exposes `AXWindow` names its own window, and that window is used
//! as read. Some toolkits expose no `AXWindow` on any element; their windows are then
//! resolved through the owning application, and such a window is *verified* only
//! when it was read in the same operation under a check that ties it to the element:
//! the focused window between two reads confirming the element still has focus, or
//! the clicked on-screen window matched by window number or by a unique frame. A
//! verified window's id comes from that same operation: the matched on-screen window,
//! the window's own number, or a frame no other on-screen window shares.
//!
//! A body read whose capture decision depends on the current window needs a verified
//! window. When the window cannot be verified now ([`ElementWindow::Unverified`]),
//! a window recorded earlier may still attribute an event, but it never authorizes
//! a new body read: that read is suppressed, as for an unknown window.

use std::slice;

use super::{
    AX_ERROR_SUCCESS, AXUIElementGetTypeID, CFGetTypeID, cf_equal, copy_attribute, copy_element,
    copy_string, create_application,
};
use crate::ffi::{
    ax::{
        NativeAxError, NativeWindow,
        cf::{CfRef, OwnedCf, i64_value},
        native_error,
        types::{decode_point, decode_size},
    },
    geometry::{AxFrame, AxPoint},
    window_list::{
        OnScreenWindow, on_screen_windows, window_id_for_frame, window_id_for_frame_in_windows,
    },
};

/// The application window that holds an element which does not name its own window.
pub(super) enum OwningWindow {
    /// The element has focus, so it lives in the application's focused window.
    Focused,
    /// The element was hit-tested at this screen point.
    UnderPoint(AxPoint),
}

#[derive(Debug, PartialEq)]
pub(super) enum ElementWindow {
    /// The element's own window, or an application window verified to hold it.
    Verified(Option<NativeWindow>),
    /// The element names no window and the application cannot verify one now.
    Unverified,
}

impl ElementWindow {
    /// The window to bind a fresh read to; an unverified window is unknown.
    pub(super) fn into_verified(self) -> Option<NativeWindow> {
        match self {
            Self::Verified(window) => window,
            Self::Unverified => None,
        }
    }
}

/// Resolves the window that owns `element`.
///
/// A failed `AXWindow` read is returned as-is rather than guessed around.
pub(super) fn element_window(
    element: CfRef,
    owning: OwningWindow,
) -> Result<ElementWindow, NativeAxError> {
    own_or_application_window(
        copy_element(element, "AXWindow"),
        |window| window_snapshot(window.as_ptr()),
        || application_window(element, owning),
    )
}

fn own_or_application_window<T>(
    own: Result<Option<T>, NativeAxError>,
    snapshot: impl FnOnce(T) -> Result<Option<NativeWindow>, NativeAxError>,
    application: impl FnOnce() -> Result<ElementWindow, NativeAxError>,
) -> Result<ElementWindow, NativeAxError> {
    match own? {
        Some(window) => snapshot(window).map(ElementWindow::Verified),
        None => application(),
    }
}

fn application_window(
    element: CfRef,
    owning: OwningWindow,
) -> Result<ElementWindow, NativeAxError> {
    let Some(pid) = element_pid(element)? else {
        return Ok(ElementWindow::Unverified);
    };
    let application = create_application(pid)?;
    match owning {
        OwningWindow::Focused => focused_element_window(
            || {
                Ok(copy_element(application.as_ptr(), "AXFocusedUIElement")?
                    .is_some_and(|focused| cf_equal(focused.as_ptr(), element)))
            },
            || {
                let Some(window) = copy_element(application.as_ptr(), "AXFocusedWindow")? else {
                    return Ok(None);
                };
                identified_window(window.as_ptr(), || {
                    Ok(match window_number(window.as_ptr())? {
                        Some(number) => Some(number),
                        None => window_frame(window.as_ptr())?.and_then(|frame| {
                            unique_window_id_for_frame(&on_screen_windows(i64::from(pid)), frame)
                        }),
                    })
                })
            },
        ),
        OwningWindow::UnderPoint(point) => {
            let on_screen = on_screen_windows(i64::from(pid));
            let Some(clicked) = clicked_window(&on_screen, point) else {
                return Ok(ElementWindow::Unverified);
            };
            match clicked_ax_window(application.as_ptr(), clicked)? {
                Some((window, id)) => {
                    identified_window(window.as_ptr(), || Ok(Some(id))).map(ElementWindow::Verified)
                }
                None => Ok(ElementWindow::Unverified),
            }
        }
    }
}

/// The application's focused window holds only its focused element.
///
/// Focus is checked before and after the window read, so a focus change in between
/// cannot bind the element to the newly focused window.
fn focused_element_window(
    mut is_focused: impl FnMut() -> Result<bool, NativeAxError>,
    focused_window: impl FnOnce() -> Result<Option<NativeWindow>, NativeAxError>,
) -> Result<ElementWindow, NativeAxError> {
    if !is_focused()? {
        return Ok(ElementWindow::Unverified);
    }
    let window = focused_window()?;
    if !is_focused()? {
        return Ok(ElementWindow::Unverified);
    }
    Ok(ElementWindow::Verified(window))
}

/// Returns the normal-layer window that received a click at `point`.
///
/// The on-screen list is ordered front to back, so the first window under the point
/// is the one clicked. A click on a menu or panel above the normal layer is not
/// attributed to the window beneath it.
fn clicked_window(on_screen: &[OnScreenWindow], point: AxPoint) -> Option<&OnScreenWindow> {
    on_screen
        .iter()
        .find(|window| frame_contains(window.bounds, point))
        .filter(|window| window.layer == 0)
}

fn frame_contains(frame: AxFrame, point: AxPoint) -> bool {
    point.x >= frame.origin.x
        && point.x < frame.origin.x + frame.size.width
        && point.y >= frame.origin.y
        && point.y < frame.origin.y + frame.size.height
}

/// What identifies an AX window against the on-screen window list.
enum AxWindowIdentity {
    Number(i64),
    Frame(Option<AxFrame>),
}

fn clicked_ax_window(
    application: CfRef,
    clicked: &OnScreenWindow,
) -> Result<Option<(OwnedCf, i64)>, NativeAxError> {
    let Some(windows) = copy_attribute(application, "AXWindows")? else {
        return Ok(None);
    };
    if unsafe { CFGetTypeID(windows.as_ptr()) != CFArrayGetTypeID() } {
        return Err(native_error("AXWindows attribute type", -1));
    }
    let count = unsafe { CFArrayGetCount(windows.as_ptr()) };
    let mut elements = Vec::new();
    let mut identities = Vec::new();
    for index in 0..count {
        // SAFETY: index is inside this live CFArray, which owns the returned value.
        let window = unsafe { CFArrayGetValueAtIndex(windows.as_ptr(), index) };
        if unsafe { CFGetTypeID(window) != AXUIElementGetTypeID() } {
            return Err(native_error("AXWindows element type", -1));
        }
        identities.push(match window_number(window)? {
            Some(number) => AxWindowIdentity::Number(number),
            None => AxWindowIdentity::Frame(window_frame(window)?),
        });
        elements.push(window);
    }
    Ok(
        matching_window(&identities, clicked).and_then(|(index, id)| {
            unsafe { OwnedCf::retain(elements[index]) }.map(|window| (window, id))
        }),
    )
}

/// Picks the AX window that is the clicked on-screen window, with the clicked window's
/// id. The id is carried from the match so it is never re-derived from a later window
/// list, where same-frame windows may have been reordered.
///
/// A window number identifies a window exactly. A frame does not: windows can share
/// one (tabs, stacked documents), so a frame match counts only when it is unique.
fn matching_window(
    identities: &[AxWindowIdentity],
    clicked: &OnScreenWindow,
) -> Option<(usize, i64)> {
    if let Some(index) = identities.iter().position(
        |identity| matches!(identity, AxWindowIdentity::Number(number) if *number == clicked.id),
    ) {
        return Some((index, clicked.id));
    }
    let mut frame_matches = identities.iter().enumerate().filter(|(_, identity)| {
        matches!(identity, AxWindowIdentity::Frame(Some(frame))
            if window_id_for_frame_in_windows(slice::from_ref(clicked), *frame).is_some())
    });
    match (frame_matches.next(), frame_matches.next()) {
        (Some((index, _)), None) => Some((index, clicked.id)),
        _ => None,
    }
}

/// The id of the only on-screen window with `frame`; windows sharing a frame leave
/// the id unknown rather than taking whichever is listed first.
fn unique_window_id_for_frame(on_screen: &[OnScreenWindow], frame: AxFrame) -> Option<i64> {
    let mut matches = on_screen
        .iter()
        .filter(|window| window_id_for_frame_in_windows(slice::from_ref(*window), frame).is_some());
    match (matches.next(), matches.next()) {
        (Some(window), None) => Some(window.id),
        _ => None,
    }
}

pub(in crate::ffi::ax) fn window_snapshot(
    window: CfRef,
) -> Result<Option<NativeWindow>, NativeAxError> {
    identified_window(window, || {
        Ok(match window_number(window)? {
            Some(id) => Some(id),
            None => match (element_pid(window)?, window_frame(window)?) {
                (Some(pid), Some(frame)) => window_id_for_frame(i64::from(pid), frame),
                _ => None,
            },
        })
    })
}

fn identified_window(
    window: CfRef,
    id: impl FnOnce() -> Result<Option<i64>, NativeAxError>,
) -> Result<Option<NativeWindow>, NativeAxError> {
    if copy_string(window, "AXRole")?.as_deref() != Some("AXWindow") {
        return Ok(None);
    }
    let id = id()?;
    Ok(Some(NativeWindow {
        title: copy_string(window, "AXTitle")?,
        id,
    }))
}

fn window_number(window: CfRef) -> Result<Option<i64>, NativeAxError> {
    Ok(copy_attribute(window, "AXWindowNumber")?.and_then(|value| i64_value(value.as_ptr())))
}

fn element_pid(element: CfRef) -> Result<Option<i32>, NativeAxError> {
    let mut pid = 0;
    let status = unsafe { AXUIElementGetPid(element, &raw mut pid) };
    if status == AX_ERROR_SUCCESS {
        Ok((pid > 0).then_some(pid))
    } else {
        Err(native_error("AXUIElementGetPid", status))
    }
}

fn window_frame(window: CfRef) -> Result<Option<AxFrame>, NativeAxError> {
    let Some(position) = copy_attribute(window, "AXPosition")? else {
        return Ok(None);
    };
    let Some(size) = copy_attribute(window, "AXSize")? else {
        return Ok(None);
    };
    let origin = decode_point(position.as_ptr())
        .ok_or_else(|| native_error("AXPosition attribute type", -1))?;
    let size =
        decode_size(size.as_ptr()).ok_or_else(|| native_error("AXSize attribute type", -1))?;
    Ok(Some(AxFrame { origin, size }))
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementGetPid(element: CfRef, pid: *mut i32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(array: CfRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
}

#[cfg(test)]
mod tests {
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
}
