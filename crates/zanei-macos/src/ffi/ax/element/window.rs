//! Resolution of the window that owns an AX element.

use std::slice;

use super::{
    AX_ERROR_SUCCESS, AXUIElementGetTypeID, CFGetTypeID, copy_attribute, copy_element, copy_string,
    create_application,
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
#[derive(Clone, Copy)]
pub(super) enum OwningWindow {
    /// The element has focus, so it lives in the application's focused window.
    Focused,
    /// The element was hit-tested at this screen point.
    UnderPoint(AxPoint),
}

/// Resolves the window that owns `element`.
///
/// Some toolkits expose no `AXWindow` on any element while the application still
/// lists its windows, so the window is then resolved through the owning application.
/// A failed `AXWindow` read is returned as-is rather than guessed around.
pub(super) fn element_window(
    element: CfRef,
    owning: OwningWindow,
) -> Result<Option<NativeWindow>, NativeAxError> {
    own_or_application_window(copy_element(element, "AXWindow"), || {
        application_window(element, owning)
    })?
    .map(|window| window_snapshot(window.as_ptr()))
    .transpose()
    .map(Option::flatten)
}

fn own_or_application_window<T>(
    own: Result<Option<T>, NativeAxError>,
    application: impl FnOnce() -> Result<Option<T>, NativeAxError>,
) -> Result<Option<T>, NativeAxError> {
    match own? {
        Some(window) => Ok(Some(window)),
        None => application(),
    }
}

fn application_window(
    element: CfRef,
    owning: OwningWindow,
) -> Result<Option<OwnedCf>, NativeAxError> {
    let Some(pid) = element_pid(element)? else {
        return Ok(None);
    };
    let application = create_application(pid)?;
    match owning {
        OwningWindow::Focused => copy_element(application.as_ptr(), "AXFocusedWindow"),
        OwningWindow::UnderPoint(point) => {
            let on_screen = on_screen_windows(i64::from(pid));
            match clicked_window(&on_screen, point) {
                Some(clicked) => window_with_bounds(application.as_ptr(), clicked),
                None => Ok(None),
            }
        }
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

fn window_with_bounds(
    application: CfRef,
    target: &OnScreenWindow,
) -> Result<Option<OwnedCf>, NativeAxError> {
    let Some(windows) = copy_attribute(application, "AXWindows")? else {
        return Ok(None);
    };
    if unsafe { CFGetTypeID(windows.as_ptr()) != CFArrayGetTypeID() } {
        return Err(native_error("AXWindows attribute type", -1));
    }
    for index in 0..unsafe { CFArrayGetCount(windows.as_ptr()) } {
        // SAFETY: index is inside this live CFArray, which owns the returned value.
        let window = unsafe { CFArrayGetValueAtIndex(windows.as_ptr(), index) };
        if unsafe { CFGetTypeID(window) != AXUIElementGetTypeID() } {
            return Err(native_error("AXWindows element type", -1));
        }
        let matches = window_frame(window)?.is_some_and(|frame| {
            window_id_for_frame_in_windows(slice::from_ref(target), frame).is_some()
        });
        if matches {
            return Ok(unsafe { OwnedCf::retain(window) });
        }
    }
    Ok(None)
}

pub(in crate::ffi::ax) fn window_snapshot(
    window: CfRef,
) -> Result<Option<NativeWindow>, NativeAxError> {
    if copy_string(window, "AXRole")?.as_deref() != Some("AXWindow") {
        return Ok(None);
    }
    let id = copy_attribute(window, "AXWindowNumber")?.and_then(|value| i64_value(value.as_ptr()));
    let id = match id {
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
    use super::*;
    use crate::ffi::geometry::AxSize;

    fn on_screen(id: i64, layer: i64, x: f64, y: f64, width: f64, height: f64) -> OnScreenWindow {
        OnScreenWindow {
            id,
            layer,
            title: None,
            bounds: AxFrame {
                origin: AxPoint { x, y },
                size: AxSize { width, height },
            },
        }
    }

    fn clicked_id(on_screen: &[OnScreenWindow], x: f64, y: f64) -> Option<i64> {
        clicked_window(on_screen, AxPoint { x, y }).map(|window| window.id)
    }

    #[test]
    fn click_resolves_to_the_frontmost_overlapping_window() {
        // Picking a back window would apply the wrong window's capture policy.
        let windows = [
            on_screen(1, 0, 100.0, 100.0, 200.0, 200.0),
            on_screen(2, 0, 0.0, 0.0, 800.0, 600.0),
        ];

        assert_eq!(clicked_id(&windows, 150.0, 150.0), Some(1));
        assert_eq!(clicked_id(&windows, 50.0, 50.0), Some(2));
        assert_eq!(clicked_id(&windows, 900.0, 50.0), None);
    }

    #[test]
    fn click_on_a_panel_is_not_attributed_to_the_window_beneath() {
        let windows = [
            on_screen(1, 3, 100.0, 100.0, 200.0, 200.0),
            on_screen(2, 0, 0.0, 0.0, 800.0, 600.0),
        ];

        assert_eq!(clicked_id(&windows, 150.0, 150.0), None);
        assert_eq!(clicked_id(&windows, 50.0, 50.0), Some(2));
    }

    #[test]
    fn missing_own_window_resolves_through_the_application() {
        let resolved = own_or_application_window(Ok(None), || Ok(Some("focused")));

        assert_eq!(resolved.ok().flatten(), Some("focused"));
    }

    #[test]
    fn own_window_is_kept_without_reading_the_application() {
        let resolved = own_or_application_window(Ok(Some("own")), || {
            panic!("an element that names its window must not cost an application read")
        });

        assert_eq!(resolved.ok().flatten(), Some("own"));
    }

    #[test]
    fn failed_own_window_read_is_not_replaced_by_a_guess() {
        let resolved =
            own_or_application_window::<&str>(Err(native_error("AXWindow", -25_204)), || {
                panic!("a failed read must keep its failure handling")
            });

        assert_eq!(resolved.map_err(|error| error.code()).err(), Some(-25_204));
    }
}
