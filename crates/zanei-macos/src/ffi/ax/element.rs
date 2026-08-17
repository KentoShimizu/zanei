//! Timeout-bounded AX element reads and privacy-safe snapshots.

use std::ptr;

use crate::focused_field::{FieldClass, field_class, observed_field_class};

use super::{NativeAxError, NativeElement, NativeWindow, cf::*, native_error};

const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 0.5;
const AX_ERROR_SUCCESS: i32 = 0;
const AX_ERROR_ATTRIBUTE_UNSUPPORTED: i32 = -25_205;
const AX_ERROR_NO_VALUE: i32 = -25_212;
const AX_VALUE_ATTRIBUTE: &str = "AXValue";
const AX_NUMBER_OF_CHARACTERS_ATTRIBUTE: &str = "AXNumberOfCharacters";
const MAX_STATIC_TEXT_VALUE_CHARS: usize = 256;

#[cfg(test)]
pub(super) const VALUE_CHANGE_READ_SURFACE: [&str; 4] = [
    "AXRole",
    "AXSubrole",
    AX_VALUE_ATTRIBUTE,
    AX_NUMBER_OF_CHARACTERS_ATTRIBUTE,
];

pub(super) struct FocusedElementSnapshot {
    pub(super) window: Option<NativeWindow>,
    pub(super) element: NativeElement,
    pub(super) text_baseline: Option<String>,
    pub(super) field_class: FieldClass,
}

pub(super) struct ValueSnapshot {
    pub(super) value: Option<String>,
    pub(super) value_len: Option<u64>,
    pub(super) role: Option<String>,
    pub(super) subrole: Option<String>,
    pub(super) field_class: FieldClass,
    pub(super) degraded: bool,
}

pub(super) struct ValueFieldSnapshot {
    pub(super) role: Option<String>,
    pub(super) subrole: Option<String>,
    pub(super) field_class: FieldClass,
    pub(super) degraded: bool,
}

pub(super) fn element_snapshot(
    element: CfRef,
    capture_text_content: bool,
) -> Result<Option<(Option<NativeWindow>, NativeElement)>, NativeAxError> {
    let subrole = copy_string(element, "AXSubrole")?;
    let role = copy_string(element, "AXRole")?;
    let field_class = field_class(role.as_deref(), subrole.as_deref());
    if field_class == FieldClass::SecureText {
        return Ok(None);
    }
    let window = copy_element(element, "AXWindow")?
        .map(|window| window_snapshot(window.as_ptr()))
        .transpose()?
        .flatten();
    let value = gated_value(capture_text_content, field_class, role.as_deref(), || {
        copy_string(element, "AXValue")
    })?;
    let character_count = match field_class {
        FieldClass::KnownText(_) | FieldClass::KnownSafeNonText => {
            copy_attribute(element, "AXNumberOfCharacters")?
                .and_then(|value| i64_value(value.as_ptr()))
        }
        FieldClass::SecureText | FieldClass::Unknown => None,
    };
    let value_len = value_length(character_count, value.as_deref());
    Ok(Some((
        window,
        NativeElement {
            role,
            subrole,
            title: copy_string(element, "AXTitle")?,
            value,
            value_len,
        },
    )))
}

pub(super) fn focused_element_snapshot(
    element: CfRef,
    capture_text_content: bool,
    secure_input: bool,
) -> Result<Option<FocusedElementSnapshot>, NativeAxError> {
    let subrole = copy_string(element, "AXSubrole")?;
    let role = copy_string(element, "AXRole")?;
    let native_field_class = field_class(role.as_deref(), subrole.as_deref());
    if focused_element_is_excluded(native_field_class) {
        return Ok(None);
    }
    let field_class = observed_field_class(role.as_deref(), subrole.as_deref(), secure_input);
    let window = copy_element(element, "AXWindow")?
        .map(|window| window_snapshot(window.as_ptr()))
        .transpose()?
        .flatten();
    let (value, text_baseline) = match field_class {
        FieldClass::KnownText(_) => {
            let baseline = capture_text_content
                .then(|| copy_string(element, AX_VALUE_ATTRIBUTE))
                .transpose()?
                .flatten();
            (None, baseline)
        }
        FieldClass::KnownSafeNonText => (
            gated_value(capture_text_content, field_class, role.as_deref(), || {
                copy_string(element, AX_VALUE_ATTRIBUTE)
            })?,
            None,
        ),
        FieldClass::SecureText | FieldClass::Unknown => (None, None),
    };
    let character_count = match field_class {
        FieldClass::KnownText(_) | FieldClass::KnownSafeNonText => {
            copy_attribute(element, AX_NUMBER_OF_CHARACTERS_ATTRIBUTE)?
                .and_then(|value| i64_value(value.as_ptr()))
        }
        FieldClass::SecureText | FieldClass::Unknown => None,
    };
    let value_len = value_length(
        character_count,
        value.as_deref().or(text_baseline.as_deref()),
    );
    Ok(Some(FocusedElementSnapshot {
        window,
        element: NativeElement {
            role,
            subrole,
            title: copy_string(element, "AXTitle")?,
            value,
            value_len,
        },
        text_baseline,
        field_class,
    }))
}

pub(super) const fn focused_element_is_excluded(field_class: FieldClass) -> bool {
    matches!(field_class, FieldClass::SecureText)
}

/// Reads only the mutable value surface used by `AXValueChanged` handling.
pub(super) fn value_snapshot(
    element: CfRef,
    capture_text_content: bool,
    secure_input: bool,
) -> ValueSnapshot {
    let classification = value_field_snapshot(element, secure_input);
    if classification.degraded
        || matches!(
            classification.field_class,
            FieldClass::SecureText | FieldClass::Unknown
        )
    {
        return suppressed_value_snapshot(
            classification.field_class,
            classification.role,
            classification.subrole,
            classification.degraded,
        );
    }
    let ValueFieldSnapshot {
        role,
        subrole,
        field_class,
        ..
    } = classification;
    let value = match match field_class {
        FieldClass::KnownText(_) if capture_text_content => {
            copy_string(element, AX_VALUE_ATTRIBUTE)
        }
        FieldClass::KnownText(_) => Ok(None),
        FieldClass::KnownSafeNonText => {
            gated_value(capture_text_content, field_class, role.as_deref(), || {
                copy_string(element, AX_VALUE_ATTRIBUTE)
            })
        }
        FieldClass::SecureText | FieldClass::Unknown => Ok(None),
    } {
        Ok(value) => value,
        Err(error) => {
            trace_value_read_error(AX_VALUE_ATTRIBUTE, &error);
            return suppressed_value_snapshot(FieldClass::Unknown, role, subrole, true);
        }
    };
    let character_count = match copy_attribute(element, AX_NUMBER_OF_CHARACTERS_ATTRIBUTE) {
        Ok(character_count) => character_count,
        Err(error) => {
            trace_value_read_error(AX_NUMBER_OF_CHARACTERS_ATTRIBUTE, &error);
            return suppressed_value_snapshot(FieldClass::Unknown, role, subrole, true);
        }
    };
    let character_count = character_count.and_then(|value| i64_value(value.as_ptr()));
    ValueSnapshot {
        value_len: value_length(character_count, value.as_deref()),
        value,
        role,
        subrole,
        field_class,
        degraded: false,
    }
}

pub(super) fn value_field_snapshot(element: CfRef, secure_input: bool) -> ValueFieldSnapshot {
    if secure_input {
        return ValueFieldSnapshot {
            role: None,
            subrole: None,
            field_class: FieldClass::SecureText,
            degraded: false,
        };
    }
    let role = copy_string(element, "AXRole");
    let subrole = copy_string(element, "AXSubrole");
    if let Err(error) = &role {
        trace_value_read_error("AXRole", error);
    }
    if let Err(error) = &subrole {
        trace_value_read_error("AXSubrole", error);
    }
    match (role, subrole) {
        (Ok(role), Ok(subrole)) => ValueFieldSnapshot {
            field_class: observed_field_class(role.as_deref(), subrole.as_deref(), false),
            role,
            subrole,
            degraded: false,
        },
        _ => ValueFieldSnapshot {
            role: None,
            subrole: None,
            field_class: FieldClass::Unknown,
            degraded: true,
        },
    }
}

fn trace_value_read_error(attribute: &'static str, error: &NativeAxError) {
    crate::trace::trace!(
        "component=ax phase=value_read action=error attribute={} operation={} code={}",
        attribute,
        error.operation(),
        error.code()
    );
}

fn suppressed_value_snapshot(
    field_class: FieldClass,
    role: Option<String>,
    subrole: Option<String>,
    degraded: bool,
) -> ValueSnapshot {
    ValueSnapshot {
        value: None,
        value_len: None,
        role,
        subrole,
        field_class,
        degraded,
    }
}

pub(super) fn window_snapshot(window: CfRef) -> Result<Option<NativeWindow>, NativeAxError> {
    if copy_string(window, "AXRole")?.as_deref() != Some("AXWindow") {
        return Ok(None);
    }
    Ok(Some(NativeWindow {
        title: copy_string(window, "AXTitle")?,
        id: copy_attribute(window, "AXWindowNumber")?.and_then(|value| i64_value(value.as_ptr())),
    }))
}

pub(super) fn create_application(pid: i32) -> Result<OwnedCf, NativeAxError> {
    unsafe { OwnedCf::from_create(AXUIElementCreateApplication(pid)) }
        .ok_or_else(|| native_error("AXUIElementCreateApplication", -1))
}

pub(super) fn element_at_position(
    application: CfRef,
    x: f64,
    y: f64,
) -> Result<Option<OwnedCf>, NativeAxError> {
    let mut element = ptr::null();
    let status = unsafe {
        AXUIElementCopyElementAtPosition(application, x as f32, y as f32, &raw mut element)
    };
    if status != AX_ERROR_SUCCESS {
        return Err(native_error("AXUIElementCopyElementAtPosition", status));
    }
    Ok(unsafe { OwnedCf::from_create(element) })
}

pub(super) fn copy_element(
    element: CfRef,
    attribute: &str,
) -> Result<Option<OwnedCf>, NativeAxError> {
    let value = copy_attribute(element, attribute)?;
    match value {
        Some(value) if unsafe { CFGetTypeID(value.as_ptr()) == AXUIElementGetTypeID() } => {
            Ok(Some(value))
        }
        Some(_) => Err(native_error("AXUIElement attribute type", -1)),
        None => Ok(None),
    }
}

pub(super) fn set_timeout(element: CfRef) -> Result<(), NativeAxError> {
    let status = unsafe { AXUIElementSetMessagingTimeout(element, AX_MESSAGING_TIMEOUT_SECONDS) };
    if status == AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(native_error("AXUIElementSetMessagingTimeout", status))
    }
}

pub(super) fn set_boolean_attribute(element: CfRef, attribute: &str) -> Result<(), NativeAxError> {
    let attribute =
        cf_string(attribute).ok_or_else(|| native_error("CFStringCreateWithCString", -1))?;
    let status =
        unsafe { AXUIElementSetAttributeValue(element, attribute.as_ptr(), boolean_true()) };
    if status == AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(native_error("AXUIElementSetAttributeValue", status))
    }
}

pub(super) fn cf_equal(left: CfRef, right: CfRef) -> bool {
    unsafe { CFEqual(left, right) != 0 }
}

pub(super) fn element_role(element: CfRef) -> Option<String> {
    copy_string(element, "AXRole").ok().flatten()
}

pub(super) fn gated_value<E>(
    capture_text_content: bool,
    field_class: FieldClass,
    role: Option<&str>,
    read: impl FnOnce() -> Result<Option<String>, E>,
) -> Result<Option<String>, E> {
    if !capture_text_content || field_class != FieldClass::KnownSafeNonText {
        return Ok(None);
    }
    let value = read()?;
    Ok(match (role, value) {
        (Some("AXStaticText"), Some(value))
            if value.chars().count() > MAX_STATIC_TEXT_VALUE_CHARS =>
        {
            None
        }
        (_, value) => value,
    })
}

pub(super) fn value_length(character_count: Option<i64>, value: Option<&str>) -> Option<u64> {
    character_count
        .and_then(|length| u64::try_from(length).ok())
        .or_else(|| value.and_then(|value| u64::try_from(value.chars().count()).ok()))
}

fn copy_attribute(element: CfRef, attribute: &str) -> Result<Option<OwnedCf>, NativeAxError> {
    set_timeout(element)?;
    let attribute =
        cf_string(attribute).ok_or_else(|| native_error("CFStringCreateWithCString", -1))?;
    let mut value = ptr::null();
    let status =
        unsafe { AXUIElementCopyAttributeValue(element, attribute.as_ptr(), &raw mut value) };
    match status {
        AX_ERROR_SUCCESS => unsafe { OwnedCf::from_create(value) }
            .map(Some)
            .ok_or_else(|| native_error("AXUIElementCopyAttributeValue", -1)),
        AX_ERROR_ATTRIBUTE_UNSUPPORTED | AX_ERROR_NO_VALUE => Ok(None),
        code => Err(native_error("AXUIElementCopyAttributeValue", code)),
    }
}

fn copy_string(element: CfRef, attribute: &str) -> Result<Option<String>, NativeAxError> {
    Ok(copy_attribute(element, attribute)?.and_then(|value| string_value(value.as_ptr())))
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> CfRef;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementSetMessagingTimeout(element: CfRef, timeout_seconds: f32) -> i32;
    fn AXUIElementCopyAttributeValue(element: CfRef, attribute: CfRef, value: *mut CfRef) -> i32;
    fn AXUIElementSetAttributeValue(element: CfRef, attribute: CfRef, value: CfRef) -> i32;
    fn AXUIElementCopyElementAtPosition(
        application: CfRef,
        x: f32,
        y: f32,
        element: *mut CfRef,
    ) -> i32;
    fn CFGetTypeID(value: CfRef) -> usize;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFEqual(left: CfRef, right: CfRef) -> u8;
}
