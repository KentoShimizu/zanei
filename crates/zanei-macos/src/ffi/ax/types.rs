//! Shared AX value, observation, and error shapes.

use std::{ffi::c_void, fmt};
use time::OffsetDateTime;

use super::cf::CfRef;
use crate::{
    capture_policy::CaptureDecision,
    ffi::{
        geometry::{AxPoint, AxSize},
        window_list::NativeWindow,
    },
};

pub(super) const AX_ERROR_ATTRIBUTE_UNSUPPORTED: i32 = -25_205;

const AX_VALUE_TYPE_POINT: u32 = 1;
const AX_VALUE_TYPE_SIZE: u32 = 2;
const AX_VALUE_TYPE_RANGE: u32 = 4;
const AX_VALUE_TYPE_ERROR: u32 = 5;

pub(super) enum DecodedAxError {
    NotError,
    Code(i32),
    Invalid,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AxTextRange {
    pub location: isize,
    pub length: isize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeElement {
    pub(crate) role: Option<String>,
    pub(crate) subrole: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) value_len: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeUiValueEvent {
    pub(crate) pid: i32,
    pub(crate) window: Option<NativeWindow>,
    pub(crate) element: NativeElement,
    pub(crate) text: Option<String>,
    pub(crate) capture_decision: Option<CaptureDecision>,
    pub(crate) observed_at: OffsetDateTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NativeAxEvent {
    WindowFocused {
        pid: i32,
        window: NativeWindow,
        observed_at: OffsetDateTime,
    },
    WindowTitleChanged {
        pid: i32,
        window: NativeWindow,
        observed_at: OffsetDateTime,
    },
    UiFocused {
        pid: i32,
        generation: u64,
        window: Option<NativeWindow>,
        element: Option<NativeElement>,
        observed_at: OffsetDateTime,
    },
    UiValueChanged(Box<NativeUiValueEvent>),
    PageLoaded {
        pid: i32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeHitTest {
    pub(crate) pid: i32,
    pub(crate) window: Option<NativeWindow>,
    pub(crate) element: NativeElement,
}

#[derive(Debug)]
pub(crate) struct NativeAxError {
    pub(super) operation: &'static str,
    pub(super) code: i32,
}

impl fmt::Display for NativeAxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failed with AXError {}",
            self.operation, self.code
        )
    }
}

impl NativeAxError {
    pub(super) const fn operation(&self) -> &'static str {
        self.operation
    }

    pub(super) const fn code(&self) -> i32 {
        self.code
    }

    pub(super) const fn is_attribute_unsupported(&self) -> bool {
        self.code == AX_ERROR_ATTRIBUTE_UNSUPPORTED
    }
}

pub(super) fn decode_point(value: CfRef) -> Option<AxPoint> {
    if ax_value_type(value) != Some(AX_VALUE_TYPE_POINT) {
        return None;
    }
    let mut point = AxPoint { x: 0.0, y: 0.0 };
    (unsafe { AXValueGetValue(value, AX_VALUE_TYPE_POINT, (&raw mut point).cast()) } != 0)
        .then_some(point)
}

pub(super) fn decode_size(value: CfRef) -> Option<AxSize> {
    if ax_value_type(value) != Some(AX_VALUE_TYPE_SIZE) {
        return None;
    }
    let mut size = AxSize {
        width: 0.0,
        height: 0.0,
    };
    (unsafe { AXValueGetValue(value, AX_VALUE_TYPE_SIZE, (&raw mut size).cast()) } != 0)
        .then_some(size)
}

pub(super) fn decode_range(value: CfRef) -> Option<AxTextRange> {
    if ax_value_type(value) != Some(AX_VALUE_TYPE_RANGE) {
        return None;
    }
    let mut range = AxTextRange {
        location: 0,
        length: 0,
    };
    (unsafe { AXValueGetValue(value, AX_VALUE_TYPE_RANGE, (&raw mut range).cast()) } != 0)
        .then_some(range)
}

pub(super) fn decode_error(value: CfRef) -> DecodedAxError {
    if ax_value_type(value) != Some(AX_VALUE_TYPE_ERROR) {
        return DecodedAxError::NotError;
    }
    let mut code = -1;
    if unsafe { AXValueGetValue(value, AX_VALUE_TYPE_ERROR, (&raw mut code).cast()) } == 0 {
        DecodedAxError::Invalid
    } else {
        DecodedAxError::Code(code)
    }
}

fn ax_value_type(value: CfRef) -> Option<u32> {
    (!value.is_null() && unsafe { CFGetTypeID(value) } == unsafe { AXValueGetTypeID() })
        .then(|| unsafe { AXValueGetType(value) })
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetType(value: CfRef) -> u32;
    fn AXValueGetValue(value: CfRef, value_type: u32, output: *mut c_void) -> u8;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFGetTypeID(value: CfRef) -> usize;
}
