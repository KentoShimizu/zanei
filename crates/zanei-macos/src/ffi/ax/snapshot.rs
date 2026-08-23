//! Timeout-bounded AX primitives for content snapshot traversal.

use std::{ffi::c_void, fmt, ptr};

use super::cf::{CfRef, OwnedCf, cf_string, i64_value, string_value};
use crate::content_snapshot::budget::AX_CALL_TIMEOUT;

const AX_ERROR_SUCCESS: i32 = 0;
const AX_ERROR_CANNOT_COMPLETE: i32 = -25_204;
const AX_ERROR_ATTRIBUTE_UNSUPPORTED: i32 = -25_205;
const AX_ERROR_NO_VALUE: i32 = -25_212;
const WRAPPER_CONTRACT_ERROR: i32 = -1;

const AX_VALUE_TYPE_POINT: u32 = 1;
const AX_VALUE_TYPE_SIZE: u32 = 2;
const AX_VALUE_TYPE_RANGE: u32 = 4;
const AX_VALUE_TYPE_ERROR: u32 = 5;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxSize {
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxFrame {
    pub origin: AxPoint,
    pub size: AxSize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AxTextRange {
    pub location: isize,
    pub length: isize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotAttribute {
    Role,
    Subrole,
    Title,
    Description,
    Value,
    Position,
    Size,
}

impl SnapshotAttribute {
    const fn name(self) -> &'static str {
        match self {
            Self::Role => "AXRole",
            Self::Subrole => "AXSubrole",
            Self::Title => "AXTitle",
            Self::Description => "AXDescription",
            Self::Value => "AXValue",
            Self::Position => "AXPosition",
            Self::Size => "AXSize",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SnapshotAttributeValue {
    Text(String),
    Point(AxPoint),
    Size(AxSize),
}

pub type SnapshotAttributeResult = Result<Option<SnapshotAttributeValue>, SnapshotAxError>;

pub struct SnapshotAxApplication {
    element: SnapshotAxElement,
}

pub struct SnapshotAxElement {
    pid: i32,
    value: OwnedCf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotAxError {
    operation: &'static str,
    code: i32,
    pid: i32,
}

impl SnapshotAxError {
    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    pub const fn code(&self) -> i32 {
        self.code
    }

    pub const fn pid(&self) -> i32 {
        self.pid
    }

    pub const fn is_timeout(&self) -> bool {
        self.code == AX_ERROR_CANNOT_COMPLETE
    }
}

impl fmt::Display for SnapshotAxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failed with AXError {} for pid {}",
            self.operation, self.code, self.pid
        )
    }
}

impl std::error::Error for SnapshotAxError {}

impl SnapshotAxApplication {
    pub fn new(pid: i32) -> Result<Self, SnapshotAxError> {
        let value = unsafe { AXUIElementCreateApplication(pid) };
        let value = unsafe { OwnedCf::from_create(value) }
            .ok_or_else(|| ax_error("AXUIElementCreateApplication", WRAPPER_CONTRACT_ERROR, pid))?;
        Ok(Self {
            element: SnapshotAxElement::from_owned(pid, value)?,
        })
    }

    pub fn pid(&self) -> i32 {
        self.element.pid
    }

    pub fn focused_window(&self) -> Result<Option<SnapshotAxElement>, SnapshotAxError> {
        self.element.copy_element("AXFocusedWindow")
    }
}

impl SnapshotAxElement {
    fn from_owned(pid: i32, value: OwnedCf) -> Result<Self, SnapshotAxError> {
        set_timeout(value.as_ptr(), pid)?;
        Ok(Self { pid, value })
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn window_number(&self) -> Result<Option<i64>, SnapshotAxError> {
        let Some(value) = self.copy_attribute("AXWindowNumber")? else {
            return Ok(None);
        };
        i64_value(value.as_ptr())
            .map(Some)
            .ok_or_else(|| self.contract_error("AXWindowNumber type"))
    }

    pub fn frame(&self) -> Result<Option<AxFrame>, SnapshotAxError> {
        let values = self.copy_multiple(&[SnapshotAttribute::Position, SnapshotAttribute::Size])?;
        let mut values = values.into_iter();
        let position = values
            .next()
            .ok_or_else(|| self.contract_error("AX frame position"))??;
        let size = values
            .next()
            .ok_or_else(|| self.contract_error("AX frame size"))??;
        match (position, size) {
            (None, None) => Ok(None),
            (
                Some(SnapshotAttributeValue::Point(origin)),
                Some(SnapshotAttributeValue::Size(size)),
            ) => Ok(Some(AxFrame { origin, size })),
            _ => Err(self.contract_error("AX frame result")),
        }
    }

    pub fn children_range(
        &self,
        index: usize,
        maximum_count: usize,
    ) -> Result<Vec<Self>, SnapshotAxError> {
        set_timeout(self.value.as_ptr(), self.pid)?;
        let index =
            isize::try_from(index).map_err(|_| self.contract_error("AX children range index"))?;
        let maximum_count = isize::try_from(maximum_count)
            .map_err(|_| self.contract_error("AX children range count"))?;
        let attribute = cf_string("AXChildren")
            .ok_or_else(|| self.contract_error("CFStringCreateWithCString"))?;
        let mut values = ptr::null();
        let status = unsafe {
            AXUIElementCopyAttributeValues(
                self.value.as_ptr(),
                attribute.as_ptr(),
                index,
                maximum_count,
                &raw mut values,
            )
        };
        match status {
            AX_ERROR_ATTRIBUTE_UNSUPPORTED | AX_ERROR_NO_VALUE => return Ok(Vec::new()),
            AX_ERROR_SUCCESS => {}
            code => return Err(ax_error("AXUIElementCopyAttributeValues", code, self.pid)),
        }
        let values = unsafe { OwnedCf::from_create(values) }
            .ok_or_else(|| self.contract_error("AXUIElementCopyAttributeValues returned null"))?;
        let count = array_count(values.as_ptr(), self.pid)?;
        let mut children = Vec::with_capacity(count);
        for offset in 0..count {
            let child = array_value(values.as_ptr(), offset, self.pid)?;
            if unsafe { CFGetTypeID(child) } != unsafe { AXUIElementGetTypeID() } {
                return Err(self.contract_error("AX children element type"));
            }
            let child = unsafe { OwnedCf::retain(child) }
                .ok_or_else(|| self.contract_error("CFRetain AX child"))?;
            children.push(Self::from_owned(self.pid, child)?);
        }
        Ok(children)
    }

    pub fn copy_multiple(
        &self,
        attributes: &[SnapshotAttribute],
    ) -> Result<Vec<SnapshotAttributeResult>, SnapshotAxError> {
        set_timeout(self.value.as_ptr(), self.pid)?;
        let names = attributes
            .iter()
            .map(|attribute| {
                cf_string(attribute.name())
                    .ok_or_else(|| self.contract_error("CFStringCreateWithCString"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let pointers = names.iter().map(OwnedCf::as_ptr).collect::<Vec<_>>();
        let names = create_array(&pointers, self.pid)?;
        let mut values = ptr::null();
        let status = unsafe {
            AXUIElementCopyMultipleAttributeValues(
                self.value.as_ptr(),
                names.as_ptr(),
                0,
                &raw mut values,
            )
        };
        if status != AX_ERROR_SUCCESS {
            return Err(ax_error(
                "AXUIElementCopyMultipleAttributeValues",
                status,
                self.pid,
            ));
        }
        let values = unsafe { OwnedCf::from_create(values) }.ok_or_else(|| {
            self.contract_error("AXUIElementCopyMultipleAttributeValues returned null")
        })?;
        let count = array_count(values.as_ptr(), self.pid)?;
        if count != attributes.len() {
            return Err(self.contract_error("AX multiple attribute result count"));
        }
        attributes
            .iter()
            .copied()
            .enumerate()
            .map(|(index, attribute)| {
                let value = array_value(values.as_ptr(), index, self.pid)?;
                Ok(decode_attribute(attribute, value, self.pid))
            })
            .collect()
    }

    pub fn visible_character_range(&self) -> Result<Option<AxTextRange>, SnapshotAxError> {
        let Some(value) = self.copy_attribute("AXVisibleCharacterRange")? else {
            return Ok(None);
        };
        decode_range(value.as_ptr())
            .map(Some)
            .ok_or_else(|| self.contract_error("AXVisibleCharacterRange type"))
    }

    pub fn string_for_range(&self, range: AxTextRange) -> Result<Option<String>, SnapshotAxError> {
        set_timeout(self.value.as_ptr(), self.pid)?;
        let parameter = unsafe { AXValueCreate(AX_VALUE_TYPE_RANGE, (&raw const range).cast()) };
        let parameter = unsafe { OwnedCf::from_create(parameter) }
            .ok_or_else(|| self.contract_error("AXValueCreate range"))?;
        let attribute = cf_string("AXStringForRange")
            .ok_or_else(|| self.contract_error("CFStringCreateWithCString"))?;
        let mut value = ptr::null();
        let status = unsafe {
            AXUIElementCopyParameterizedAttributeValue(
                self.value.as_ptr(),
                attribute.as_ptr(),
                parameter.as_ptr(),
                &raw mut value,
            )
        };
        match status {
            AX_ERROR_ATTRIBUTE_UNSUPPORTED | AX_ERROR_NO_VALUE => Ok(None),
            AX_ERROR_SUCCESS => {
                let value = unsafe { OwnedCf::from_create(value) }
                    .ok_or_else(|| self.contract_error("AXStringForRange returned null"))?;
                string_value(value.as_ptr())
                    .map(Some)
                    .ok_or_else(|| self.contract_error("AXStringForRange type"))
            }
            code => Err(ax_error(
                "AXUIElementCopyParameterizedAttributeValue",
                code,
                self.pid,
            )),
        }
    }

    pub fn visible_text(&self) -> Result<Option<String>, SnapshotAxError> {
        let Some(range) = self.visible_character_range()? else {
            return Ok(None);
        };
        self.string_for_range(range)
    }

    fn copy_element(&self, attribute: &'static str) -> Result<Option<Self>, SnapshotAxError> {
        let Some(value) = self.copy_attribute(attribute)? else {
            return Ok(None);
        };
        if unsafe { CFGetTypeID(value.as_ptr()) } != unsafe { AXUIElementGetTypeID() } {
            return Err(self.contract_error("AX element attribute type"));
        }
        Self::from_owned(self.pid, value).map(Some)
    }

    fn copy_attribute(&self, attribute: &'static str) -> Result<Option<OwnedCf>, SnapshotAxError> {
        set_timeout(self.value.as_ptr(), self.pid)?;
        let attribute =
            cf_string(attribute).ok_or_else(|| self.contract_error("CFStringCreateWithCString"))?;
        let mut value = ptr::null();
        let status = unsafe {
            AXUIElementCopyAttributeValue(self.value.as_ptr(), attribute.as_ptr(), &raw mut value)
        };
        match status {
            AX_ERROR_ATTRIBUTE_UNSUPPORTED | AX_ERROR_NO_VALUE => Ok(None),
            AX_ERROR_SUCCESS => unsafe { OwnedCf::from_create(value) }
                .map(Some)
                .ok_or_else(|| self.contract_error("AXUIElementCopyAttributeValue returned null")),
            code => Err(ax_error("AXUIElementCopyAttributeValue", code, self.pid)),
        }
    }

    const fn contract_error(&self, operation: &'static str) -> SnapshotAxError {
        ax_error(operation, WRAPPER_CONTRACT_ERROR, self.pid)
    }
}

fn set_timeout(element: CfRef, pid: i32) -> Result<(), SnapshotAxError> {
    let status = unsafe { AXUIElementSetMessagingTimeout(element, AX_CALL_TIMEOUT.as_secs_f32()) };
    if status == AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(ax_error("AXUIElementSetMessagingTimeout", status, pid))
    }
}

fn create_array(values: &[CfRef], pid: i32) -> Result<OwnedCf, SnapshotAxError> {
    let count = isize::try_from(values.len())
        .map_err(|_| ax_error("CFArrayCreate count", WRAPPER_CONTRACT_ERROR, pid))?;
    let array = unsafe { CFArrayCreate(ptr::null(), values.as_ptr(), count, ptr::null()) };
    unsafe { OwnedCf::from_create(array) }
        .ok_or_else(|| ax_error("CFArrayCreate", WRAPPER_CONTRACT_ERROR, pid))
}

fn array_count(array: CfRef, pid: i32) -> Result<usize, SnapshotAxError> {
    if unsafe { CFGetTypeID(array) } != unsafe { CFArrayGetTypeID() } {
        return Err(ax_error("CFArray result type", WRAPPER_CONTRACT_ERROR, pid));
    }
    usize::try_from(unsafe { CFArrayGetCount(array) })
        .map_err(|_| ax_error("CFArrayGetCount", WRAPPER_CONTRACT_ERROR, pid))
}

fn array_value(array: CfRef, index: usize, pid: i32) -> Result<CfRef, SnapshotAxError> {
    let index = isize::try_from(index)
        .map_err(|_| ax_error("CFArrayGetValueAtIndex", WRAPPER_CONTRACT_ERROR, pid))?;
    let value = unsafe { CFArrayGetValueAtIndex(array, index) };
    if value.is_null() {
        Err(ax_error(
            "CFArrayGetValueAtIndex",
            WRAPPER_CONTRACT_ERROR,
            pid,
        ))
    } else {
        Ok(value)
    }
}

fn decode_attribute(
    attribute: SnapshotAttribute,
    value: CfRef,
    pid: i32,
) -> SnapshotAttributeResult {
    if unsafe { CFGetTypeID(value) } == unsafe { CFNullGetTypeID() } {
        return Ok(None);
    }
    if unsafe { CFGetTypeID(value) } == unsafe { AXValueGetTypeID() }
        && unsafe { AXValueGetType(value) } == AX_VALUE_TYPE_ERROR
    {
        let mut code = WRAPPER_CONTRACT_ERROR;
        if unsafe { AXValueGetValue(value, AX_VALUE_TYPE_ERROR, (&raw mut code).cast()) } == 0 {
            return Err(ax_error(
                "AXValueGetValue error",
                WRAPPER_CONTRACT_ERROR,
                pid,
            ));
        }
        return match code {
            AX_ERROR_ATTRIBUTE_UNSUPPORTED | AX_ERROR_NO_VALUE => Ok(None),
            code => Err(ax_error(attribute.name(), code, pid)),
        };
    }
    match attribute {
        SnapshotAttribute::Position => decode_point(value)
            .map(SnapshotAttributeValue::Point)
            .map(Some)
            .ok_or_else(|| ax_error("AXPosition type", WRAPPER_CONTRACT_ERROR, pid)),
        SnapshotAttribute::Size => decode_size(value)
            .map(SnapshotAttributeValue::Size)
            .map(Some)
            .ok_or_else(|| ax_error("AXSize type", WRAPPER_CONTRACT_ERROR, pid)),
        SnapshotAttribute::Role
        | SnapshotAttribute::Subrole
        | SnapshotAttribute::Title
        | SnapshotAttribute::Description
        | SnapshotAttribute::Value => string_value(value)
            .map(SnapshotAttributeValue::Text)
            .map(Some)
            .ok_or_else(|| ax_error(attribute.name(), WRAPPER_CONTRACT_ERROR, pid)),
    }
}

fn decode_point(value: CfRef) -> Option<AxPoint> {
    if unsafe { CFGetTypeID(value) } != unsafe { AXValueGetTypeID() }
        || unsafe { AXValueGetType(value) } != AX_VALUE_TYPE_POINT
    {
        return None;
    }
    let mut point = AxPoint { x: 0.0, y: 0.0 };
    (unsafe { AXValueGetValue(value, AX_VALUE_TYPE_POINT, (&raw mut point).cast()) } != 0)
        .then_some(point)
}

fn decode_size(value: CfRef) -> Option<AxSize> {
    if unsafe { CFGetTypeID(value) } != unsafe { AXValueGetTypeID() }
        || unsafe { AXValueGetType(value) } != AX_VALUE_TYPE_SIZE
    {
        return None;
    }
    let mut size = AxSize {
        width: 0.0,
        height: 0.0,
    };
    (unsafe { AXValueGetValue(value, AX_VALUE_TYPE_SIZE, (&raw mut size).cast()) } != 0)
        .then_some(size)
}

fn decode_range(value: CfRef) -> Option<AxTextRange> {
    if unsafe { CFGetTypeID(value) } != unsafe { AXValueGetTypeID() }
        || unsafe { AXValueGetType(value) } != AX_VALUE_TYPE_RANGE
    {
        return None;
    }
    let mut range = AxTextRange {
        location: 0,
        length: 0,
    };
    (unsafe { AXValueGetValue(value, AX_VALUE_TYPE_RANGE, (&raw mut range).cast()) } != 0)
        .then_some(range)
}

const fn ax_error(operation: &'static str, code: i32, pid: i32) -> SnapshotAxError {
    SnapshotAxError {
        operation,
        code,
        pid,
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> CfRef;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementSetMessagingTimeout(element: CfRef, timeout_seconds: f32) -> i32;
    fn AXUIElementCopyAttributeValue(element: CfRef, attribute: CfRef, value: *mut CfRef) -> i32;
    fn AXUIElementCopyAttributeValues(
        element: CfRef,
        attribute: CfRef,
        index: isize,
        maximum_values: isize,
        values: *mut CfRef,
    ) -> i32;
    fn AXUIElementCopyMultipleAttributeValues(
        element: CfRef,
        attributes: CfRef,
        options: u32,
        values: *mut CfRef,
    ) -> i32;
    fn AXUIElementCopyParameterizedAttributeValue(
        element: CfRef,
        attribute: CfRef,
        parameter: CfRef,
        value: *mut CfRef,
    ) -> i32;
    fn AXValueCreate(value_type: u32, value: *const c_void) -> CfRef;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetType(value: CfRef) -> u32;
    fn AXValueGetValue(value: CfRef, value_type: u32, output: *mut c_void) -> u8;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayCreate(
        allocator: CfRef,
        values: *const CfRef,
        count: isize,
        callbacks: CfRef,
    ) -> CfRef;
    fn CFArrayGetCount(array: CfRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
    fn CFArrayGetTypeID() -> usize;
    fn CFGetTypeID(value: CfRef) -> usize;
    fn CFNullGetTypeID() -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_error_preserves_operation_code_and_pid() {
        let error = ax_error("operation", -25_206, 42);

        assert_eq!(error.operation(), "operation");
        assert_eq!(error.code(), -25_206);
        assert_eq!(error.pid(), 42);
        assert_eq!(
            error.to_string(),
            "operation failed with AXError -25206 for pid 42"
        );
        assert!(!error.is_timeout());
        assert!(ax_error("operation", AX_ERROR_CANNOT_COMPLETE, 42).is_timeout());
    }

    #[test]
    fn snapshot_attribute_names_are_explicit() {
        assert_eq!(SnapshotAttribute::Role.name(), "AXRole");
        assert_eq!(SnapshotAttribute::Value.name(), "AXValue");
        assert_eq!(SnapshotAttribute::Position.name(), "AXPosition");
        assert_eq!(SnapshotAttribute::Size.name(), "AXSize");
    }
}
