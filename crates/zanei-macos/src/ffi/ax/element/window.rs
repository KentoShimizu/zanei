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
mod tests;
