//! The window that owns an AX element.
//!
//! An element that exposes `AXWindow` names its window, and that window both
//! attributes the element's events and feeds capture decisions. Some toolkits expose
//! no `AXWindow` on any element; for those elements the window is resolved through
//! the owning application and used for attribution only. It sets the window of the
//! element's events and never authorizes a capture decision that depends on the
//! window:
//!
//! - Snapshot decisions read [`ElementWindow::decision_window`], which is unknown
//!   for a resolved window.
//! - Apps whose capture depends on the window are never given a resolved window:
//!   browsers decide per window id and IDEs by window title, here and further
//!   downstream (value reads, send-time checks, quarantine). Their elements stay at
//!   an unknown window, as before windows were resolved. For every other app the
//!   capture decision does not depend on the window, so the resolved window that a
//!   field's later value events carry for attribution cannot change it.

use std::slice;

use super::{
    AXUIElementGetTypeID, CFGetTypeID, cf_equal, copy_attribute, copy_element, copy_string,
    create_application, element_pid, is_ide, window_frame, window_snapshot,
};
use crate::{
    browser_context::BrowserTarget,
    ffi::{
        ax::{
            NativeAxError, NativeWindow,
            cf::{CfRef, i64_value},
            native_error,
        },
        geometry::{AxFrame, AxPoint},
        window_list::{OnScreenWindow, on_screen_windows, window_id_for_frame_in_windows},
    },
};

/// Where the application holds an element that does not name its own window.
#[derive(Clone, Copy)]
pub(in crate::ffi::ax) enum OwningWindow {
    /// The element is the application's focused element.
    Focused,
    /// The element was hit-tested at this screen point.
    UnderPoint(AxPoint),
}

#[derive(Debug, PartialEq)]
pub(in crate::ffi::ax) enum ElementWindow {
    /// Named by the element: attributes its events and decides capture.
    Own(NativeWindow),
    /// Resolved through the application: attributes events, never decides capture.
    Attributed(NativeWindow),
    Unknown,
}

impl ElementWindow {
    /// The window a capture decision may depend on.
    pub(in crate::ffi::ax) const fn decision_window(&self) -> Option<&NativeWindow> {
        match self {
            Self::Own(window) => Some(window),
            Self::Attributed(_) | Self::Unknown => None,
        }
    }

    /// The window the element's events are attributed to.
    pub(in crate::ffi::ax) fn into_attribution(self) -> Option<NativeWindow> {
        match self {
            Self::Own(window) | Self::Attributed(window) => Some(window),
            Self::Unknown => None,
        }
    }
}

/// Whether `app`'s elements may be attributed to an application-resolved window.
pub(in crate::ffi::ax) fn attributes_resolved_windows(app: &zanei_core::schema::App) -> bool {
    BrowserTarget::from_bundle_id(app.bundle_id.as_deref()).is_none() && !is_ide(app)
}

/// Resolves the window that owns `element`; `owning` is `None` when the element's
/// application may not attribute it to a window it does not name.
pub(in crate::ffi::ax) fn element_window(
    element: CfRef,
    owning: Option<OwningWindow>,
) -> Result<ElementWindow, NativeAxError> {
    if let Some(window) = copy_element(element, "AXWindow")? {
        return Ok(
            window_snapshot(window.as_ptr())?.map_or(ElementWindow::Unknown, ElementWindow::Own)
        );
    }
    let Some(owning) = owning else {
        return Ok(ElementWindow::Unknown);
    };
    let Some(pid) = element_pid(element)? else {
        return Ok(ElementWindow::Unknown);
    };
    let application = create_application(pid)?;
    let window = match owning {
        OwningWindow::Focused => {
            let focused = copy_element(application.as_ptr(), "AXFocusedUIElement")?;
            if !focused.is_some_and(|focused| cf_equal(focused.as_ptr(), element)) {
                return Ok(ElementWindow::Unknown);
            }
            copy_element(application.as_ptr(), "AXFocusedWindow")?
                .map(|window| window_snapshot(window.as_ptr()))
                .transpose()?
                .flatten()
        }
        OwningWindow::UnderPoint(point) => clicked_window(application.as_ptr(), pid, point)?,
    };
    Ok(window.map_or(ElementWindow::Unknown, ElementWindow::Attributed))
}

fn clicked_window(
    application: CfRef,
    pid: i32,
    point: AxPoint,
) -> Result<Option<NativeWindow>, NativeAxError> {
    let on_screen = on_screen_windows(i64::from(pid));
    let Some(clicked) = clicked_on_screen(&on_screen, point) else {
        return Ok(None);
    };
    let Some(windows) = copy_attribute(application, "AXWindows")? else {
        return Ok(None);
    };
    if unsafe { CFGetTypeID(windows.as_ptr()) != CFArrayGetTypeID() } {
        return Err(native_error("AXWindows attribute type", -1));
    }
    let mut elements = Vec::new();
    let mut identities = Vec::new();
    for index in 0..unsafe { CFArrayGetCount(windows.as_ptr()) } {
        // SAFETY: index is inside this live CFArray, which owns the returned value.
        let window = unsafe { CFArrayGetValueAtIndex(windows.as_ptr(), index) };
        if unsafe { CFGetTypeID(window) != AXUIElementGetTypeID() } {
            return Err(native_error("AXWindows element type", -1));
        }
        identities.push(
            match copy_attribute(window, "AXWindowNumber")?.and_then(|n| i64_value(n.as_ptr())) {
                Some(number) => AxWindowIdentity::Number(number),
                None => AxWindowIdentity::Frame(window_frame(window)?),
            },
        );
        elements.push(window);
    }
    let Some(index) = matching_window(&identities, &on_screen, clicked) else {
        return Ok(None);
    };
    let window = elements[index];
    if copy_string(window, "AXRole")?.as_deref() != Some("AXWindow") {
        return Ok(None);
    }
    // The id is the clicked window's, never re-derived from a later window list.
    Ok(Some(NativeWindow {
        title: copy_string(window, "AXTitle")?,
        id: Some(clicked.id),
    }))
}

/// The on-screen list is ordered front to back, so the first window under the point
/// was clicked. A click on a menu or panel above the normal layer is not attributed.
fn clicked_on_screen(on_screen: &[OnScreenWindow], point: AxPoint) -> Option<&OnScreenWindow> {
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

/// Picks the AX window that is the clicked on-screen window.
///
/// A window number identifies a window exactly. A frame does not: windows can share
/// one (tabs, stacked documents), so a frame match counts only when no other AX
/// window and no other on-screen window has that frame.
fn matching_window(
    identities: &[AxWindowIdentity],
    on_screen: &[OnScreenWindow],
    clicked: &OnScreenWindow,
) -> Option<usize> {
    if let Some(index) = identities.iter().position(
        |identity| matches!(identity, AxWindowIdentity::Number(number) if *number == clicked.id),
    ) {
        return Some(index);
    }
    let same_frame =
        |frame: AxFrame| window_id_for_frame_in_windows(slice::from_ref(clicked), frame).is_some();
    let on_screen_matches = on_screen
        .iter()
        .filter(|window| window.layer == 0 && same_frame(window.bounds))
        .count();
    let mut ax_matches = identities.iter().enumerate().filter(|(_, identity)| {
        matches!(identity, AxWindowIdentity::Frame(Some(frame)) if same_frame(*frame))
    });
    match (on_screen_matches, ax_matches.next(), ax_matches.next()) {
        (1, Some((index, _)), None) => Some(index),
        _ => None,
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(array: CfRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
}

#[cfg(test)]
mod tests;
