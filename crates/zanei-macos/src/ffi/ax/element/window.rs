//! Resolution of the window that owns an AX element.

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
    /// The element was focused. While it keeps focus it lives in the application's
    /// focused window; once focus has moved on, its window is the one recorded while
    /// it was focused, never the window that holds focus now.
    Focused { recorded: Option<NativeWindow> },
    /// The element was hit-tested at this screen point.
    UnderPoint(AxPoint),
}

/// Resolves the window that owns `element`.
///
/// Some toolkits expose no `AXWindow` on any element while the application still
/// lists its windows, so the window is then resolved through the owning application.
/// A failed `AXWindow` read is returned as-is rather than guessed around, and a
/// window the application cannot vouch for stays unresolved (`None`).
pub(super) fn element_window(
    element: CfRef,
    owning: OwningWindow,
) -> Result<Option<NativeWindow>, NativeAxError> {
    own_or_application_window(
        copy_element(element, "AXWindow"),
        |window| window_snapshot(window.as_ptr()),
        || application_window(element, owning),
    )
}

fn own_or_application_window<T>(
    own: Result<Option<T>, NativeAxError>,
    snapshot: impl FnOnce(T) -> Result<Option<NativeWindow>, NativeAxError>,
    application: impl FnOnce() -> Result<Option<NativeWindow>, NativeAxError>,
) -> Result<Option<NativeWindow>, NativeAxError> {
    match own? {
        Some(window) => snapshot(window),
        None => application(),
    }
}

fn application_window(
    element: CfRef,
    owning: OwningWindow,
) -> Result<Option<NativeWindow>, NativeAxError> {
    let Some(pid) = element_pid(element)? else {
        return Ok(None);
    };
    let application = create_application(pid)?;
    match owning {
        OwningWindow::Focused { recorded } => {
            let focused = copy_element(application.as_ptr(), "AXFocusedUIElement")?;
            focused_element_window(
                focused.is_some_and(|focused| cf_equal(focused.as_ptr(), element)),
                || {
                    copy_element(application.as_ptr(), "AXFocusedWindow")?
                        .map(|window| window_snapshot(window.as_ptr()))
                        .transpose()
                        .map(Option::flatten)
                },
                recorded,
            )
        }
        OwningWindow::UnderPoint(point) => {
            let on_screen = on_screen_windows(i64::from(pid));
            let Some(clicked) = clicked_window(&on_screen, point) else {
                return Ok(None);
            };
            clicked_ax_window(application.as_ptr(), clicked)?
                .map(|window| window_snapshot(window.as_ptr()))
                .transpose()
                .map(Option::flatten)
        }
    }
}

/// The application's focused window holds only its focused element.
fn focused_element_window(
    still_focused: bool,
    focused_window: impl FnOnce() -> Result<Option<NativeWindow>, NativeAxError>,
    recorded: Option<NativeWindow>,
) -> Result<Option<NativeWindow>, NativeAxError> {
    if still_focused {
        focused_window()
    } else {
        Ok(recorded)
    }
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
) -> Result<Option<OwnedCf>, NativeAxError> {
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
    Ok(matching_window(&identities, clicked)
        .and_then(|index| unsafe { OwnedCf::retain(elements[index]) }))
}

/// Picks the AX window that is the clicked on-screen window.
///
/// A window number identifies a window exactly. A frame does not: windows can share
/// one (tabs, stacked documents), so a frame match counts only when it is unique.
fn matching_window(identities: &[AxWindowIdentity], clicked: &OnScreenWindow) -> Option<usize> {
    if let Some(index) = identities.iter().position(
        |identity| matches!(identity, AxWindowIdentity::Number(number) if *number == clicked.id),
    ) {
        return Some(index);
    }
    let mut frame_matches = identities.iter().enumerate().filter(|(_, identity)| {
        matches!(identity, AxWindowIdentity::Frame(Some(frame))
            if window_id_for_frame_in_windows(slice::from_ref(clicked), *frame).is_some())
    });
    match (frame_matches.next(), frame_matches.next()) {
        (Some((index, _)), None) => Some(index),
        _ => None,
    }
}

pub(in crate::ffi::ax) fn window_snapshot(
    window: CfRef,
) -> Result<Option<NativeWindow>, NativeAxError> {
    if copy_string(window, "AXRole")?.as_deref() != Some("AXWindow") {
        return Ok(None);
    }
    let id = match window_number(window)? {
        Some(id) => Some(id),
        None => match (element_pid(window)?, window_frame(window)?) {
            (Some(pid), Some(frame)) => window_id_for_frame(i64::from(pid), frame),
            _ => None,
        },
    };
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
            element::{capture_value_snapshot_with, value_snapshot_with},
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
            Some(1)
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
            Some(3)
        );
    }

    #[test]
    fn missing_own_window_resolves_through_the_application() {
        let resolved = own_or_application_window::<&str>(
            Ok(None),
            |_| Ok(titled("own")),
            || Ok(titled("app")),
        );

        assert_eq!(resolved.ok().flatten(), titled("app"));
    }

    #[test]
    fn own_window_is_kept_without_reading_the_application() {
        let resolved = own_or_application_window(
            Ok(Some("own")),
            |_| Ok(titled("own")),
            || panic!("an element that names its window must not cost an application read"),
        );

        assert_eq!(resolved.ok().flatten(), titled("own"));
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
    fn element_that_lost_focus_keeps_its_recorded_window() {
        // Focus moved from a blocked window to an allowed one before the old field's
        // final read; that read must stay under the old window's decision.
        let recorded = titled(".env");
        let mut window = recorded.clone();
        let surface_changes = Cell::new(0);
        let body_reads = Cell::new(0);
        let (snapshot, decision) = capture_value_snapshot_with(
            &mut window,
            &ide_policy("block"),
            &ide_app(),
            true,
            || surface_changes.set(surface_changes.get() + 1),
            || {
                focused_element_window(
                    false,
                    || panic!("the focused window holds another element"),
                    recorded.clone(),
                )
            },
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

        assert_eq!(window, recorded);
        assert_eq!(surface_changes.get(), 0);
        assert_eq!(body_reads.get(), 0);
        assert_eq!(snapshot.value, None);
        assert_eq!(decision.map(|decision| decision.is_allowed()), Some(false));
    }

    #[test]
    fn focused_element_lives_in_the_focused_window() {
        let resolved = focused_element_window(true, || Ok(titled("main.rs")), titled(".env"));

        assert_eq!(resolved.ok().flatten(), titled("main.rs"));
    }
}
