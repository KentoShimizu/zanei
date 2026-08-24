//! Single ownership boundary for CoreGraphics on-screen window-list reads.

use std::{
    ffi::{CStr, c_char, c_void},
    ptr::NonNull,
};

use super::geometry::{AxFrame, AxPoint, AxSize};

type CfRef = *const c_void;
type CfMutableRef = *mut c_void;

const CF_STRING_UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT64: isize = 4;
const CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1 << 0;
const CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;
const WINDOW_LIST_OPTIONS: u32 =
    CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS;
const FRAME_MATCH_TOLERANCE_POINTS: f64 = 1.0;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OnScreenWindow {
    pub id: i64,
    pub layer: i64,
    pub title: Option<String>,
    pub bounds: AxFrame,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeWindow {
    pub title: Option<String>,
    pub id: Option<i64>,
}

pub(crate) fn on_screen_windows(pid: i64) -> Vec<OnScreenWindow> {
    // SAFETY: CoreGraphics returns a retained CFArray or null.
    let Some(windows) = NonNull::new(unsafe { CGWindowListCopyWindowInfo(WINDOW_LIST_OPTIONS, 0) })
    else {
        return Vec::new();
    };
    let count = unsafe { CFArrayGetCount(windows.as_ptr()) };
    let mut result = Vec::new();
    for index in 0..count {
        // SAFETY: index is inside this valid CFArray.
        let dictionary = unsafe { CFArrayGetValueAtIndex(windows.as_ptr(), index) };
        if unsafe { dictionary_i64(dictionary, kCGWindowOwnerPID) } != Some(pid) {
            continue;
        }
        let Some(id) = (unsafe { dictionary_i64(dictionary, kCGWindowNumber) }) else {
            continue;
        };
        let Some(layer) = (unsafe { dictionary_i64(dictionary, kCGWindowLayer) }) else {
            continue;
        };
        let Some(bounds) = (unsafe { dictionary_bounds(dictionary, kCGWindowBounds) }) else {
            continue;
        };
        result.push(OnScreenWindow {
            id,
            layer,
            title: unsafe { dictionary_string(dictionary, kCGWindowName) },
            bounds,
        });
    }
    // SAFETY: balances Copy ownership.
    unsafe { CFRelease(windows.as_ptr().cast_const()) };
    result
}

pub(crate) fn front_window(pid: i64) -> Option<NativeWindow> {
    on_screen_windows(pid)
        .into_iter()
        .find(|window| window.layer == 0)
        .map(|window| NativeWindow {
            title: window.title,
            id: Some(window.id),
        })
}

pub(crate) fn window_id_for_frame(pid: i64, frame: AxFrame) -> Option<i64> {
    window_id_for_frame_in_windows(&on_screen_windows(pid), frame)
}

pub(crate) fn window_id_for_frame_in_windows(
    windows: &[OnScreenWindow],
    frame: AxFrame,
) -> Option<i64> {
    windows
        .iter()
        .find(|window| window.layer == 0 && bounds_match_frame(window.bounds, frame))
        .map(|window| window.id)
}

fn bounds_match_frame(bounds: AxFrame, frame: AxFrame) -> bool {
    let bounds_right = bounds.origin.x + bounds.size.width;
    let bounds_bottom = bounds.origin.y + bounds.size.height;
    let frame_right = frame.origin.x + frame.size.width;
    let frame_bottom = frame.origin.y + frame.size.height;
    within_tolerance(bounds.origin.x, frame.origin.x)
        && within_tolerance(bounds.origin.y, frame.origin.y)
        && within_tolerance(bounds_right, frame_right)
        && within_tolerance(bounds_bottom, frame_bottom)
}

fn within_tolerance(left: f64, right: f64) -> bool {
    (left - right).abs() <= FRAME_MATCH_TOLERANCE_POINTS
}

unsafe fn dictionary_i64(dictionary: CfRef, key: CfRef) -> Option<i64> {
    let value = unsafe { CFDictionaryGetValue(dictionary, key) };
    if value.is_null() {
        return None;
    }
    let mut number = 0_i64;
    // SAFETY: documented numeric window-info keys contain CFNumber values.
    (unsafe { CFNumberGetValue(value, CF_NUMBER_SINT64, (&raw mut number).cast()) } != 0)
        .then_some(number)
}

unsafe fn dictionary_string(dictionary: CfRef, key: CfRef) -> Option<String> {
    let value = unsafe { CFDictionaryGetValue(dictionary, key) };
    if value.is_null() {
        return None;
    }
    let length = unsafe { CFStringGetLength(value) };
    let maximum = unsafe { CFStringGetMaximumSizeForEncoding(length, CF_STRING_UTF8) };
    let capacity = usize::try_from(maximum).ok()?.checked_add(1)?;
    let mut buffer = vec![0_i8; capacity];
    let copied = unsafe {
        CFStringGetCString(
            value,
            buffer.as_mut_ptr(),
            isize::try_from(capacity).ok()?,
            CF_STRING_UTF8,
        )
    };
    (copied != 0).then(|| {
        // SAFETY: success writes a NUL-terminated string.
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    })
}

unsafe fn dictionary_bounds(dictionary: CfRef, key: CfRef) -> Option<AxFrame> {
    let value = unsafe { CFDictionaryGetValue(dictionary, key) };
    if value.is_null() {
        return None;
    }
    let mut bounds = AxFrame {
        origin: AxPoint { x: 0.0, y: 0.0 },
        size: AxSize {
            width: 0.0,
            height: 0.0,
        },
    };
    (unsafe { CGRectMakeWithDictionaryRepresentation(value, &raw mut bounds) } != 0)
        .then_some(bounds)
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(options: u32, relative_window: u32) -> CfMutableRef;
    fn CGRectMakeWithDictionaryRepresentation(dictionary: CfRef, rect: *mut AxFrame) -> u8;
    static kCGWindowOwnerPID: CfRef;
    static kCGWindowLayer: CfRef;
    static kCGWindowName: CfRef;
    static kCGWindowNumber: CfRef;
    static kCGWindowBounds: CfRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: CfRef);
    fn CFArrayGetCount(array: CfRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
    fn CFDictionaryGetValue(dictionary: CfRef, key: CfRef) -> CfRef;
    fn CFNumberGetValue(number: CfRef, number_type: isize, value: *mut c_void) -> u8;
    fn CFStringGetLength(string: CfRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        string: CfRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::ax::{AxPoint, AxSize};

    fn frame() -> AxFrame {
        AxFrame {
            origin: AxPoint { x: 10.0, y: 20.0 },
            size: AxSize {
                width: 100.0,
                height: 200.0,
            },
        }
    }

    fn window(id: i64, layer: i64, bounds: AxFrame) -> OnScreenWindow {
        OnScreenWindow {
            id,
            layer,
            title: None,
            bounds,
        }
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> AxFrame {
        AxFrame {
            origin: AxPoint { x, y },
            size: AxSize { width, height },
        }
    }

    #[test]
    fn exact_frame_match_returns_window_id() {
        let windows = [window(42, 0, rect(10.0, 20.0, 100.0, 200.0))];

        assert_eq!(window_id_for_frame_in_windows(&windows, frame()), Some(42));
    }

    #[test]
    fn frame_edges_match_at_one_point_tolerance() {
        let windows = [window(42, 0, rect(9.0, 21.0, 102.0, 198.0))];

        assert_eq!(window_id_for_frame_in_windows(&windows, frame()), Some(42));
    }

    #[test]
    fn nonzero_layer_is_ignored() {
        let windows = [window(42, 1, rect(10.0, 20.0, 100.0, 200.0))];

        assert_eq!(window_id_for_frame_in_windows(&windows, frame()), None);
    }

    #[test]
    fn first_matching_window_is_frontmost() {
        let bounds = rect(10.0, 20.0, 100.0, 200.0);
        let windows = [window(42, 0, bounds), window(84, 0, bounds)];

        assert_eq!(window_id_for_frame_in_windows(&windows, frame()), Some(42));
    }

    #[test]
    fn frame_without_matching_edges_returns_none() {
        let windows = [window(42, 0, rect(8.9, 20.0, 100.0, 200.0))];

        assert_eq!(window_id_for_frame_in_windows(&windows, frame()), None);
    }
}
