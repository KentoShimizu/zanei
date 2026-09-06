//! Timeout-bounded AX element reads and privacy-safe snapshots.

use std::ptr;

use crate::{
    ffi::{geometry::AxFrame, window_list::window_id_for_frame},
    focused_field::{FieldClass, field_class, observed_field_class},
};

use super::{
    AX_ERROR_ATTRIBUTE_UNSUPPORTED, NativeAxError, NativeWindow,
    cf::*,
    native_error,
    types::{decode_point, decode_size},
};

pub(super) mod initial_snapshot;

const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 0.5;
const AX_ERROR_SUCCESS: i32 = 0;
const AX_ERROR_NO_VALUE: i32 = -25_212;
const AX_VALUE_ATTRIBUTE: &str = "AXValue";
const AX_NUMBER_OF_CHARACTERS_ATTRIBUTE: &str = "AXNumberOfCharacters";
const MAX_STATIC_TEXT_VALUE_CHARS: usize = 256;

pub(super) struct ValueSnapshot {
    pub(super) value: Option<String>,
    pub(super) value_len: Option<u64>,
    pub(super) role: Option<String>,
    pub(super) subrole: Option<String>,
    pub(super) field_class: FieldClass,
    pub(super) failure: Option<NativeAxError>,
}

pub(super) struct ValueFieldSnapshot {
    pub(super) role: Option<String>,
    pub(super) subrole: Option<String>,
    pub(super) field_class: FieldClass,
    pub(super) registration_class: Option<FieldClass>,
    pub(super) failure: Option<NativeAxError>,
}

pub(super) fn capture_value_snapshot(
    element: CfRef,
    window: &mut Option<NativeWindow>,
    policy: &crate::CapturePolicy,
    app: &zanei_core::schema::App,
    capture_enabled: bool,
    secure_input: bool,
    surface_changed: impl FnMut(),
) -> (ValueSnapshot, Option<crate::CaptureDecision>) {
    capture_value_snapshot_with(
        window,
        policy,
        app,
        capture_enabled,
        surface_changed,
        || {
            copy_element(element, "AXWindow")?
                .map(|window| window_snapshot(window.as_ptr()))
                .transpose()
                .map(Option::flatten)
        },
        |allowed| value_snapshot(element, allowed, secure_input),
    )
}

pub(super) fn capture_value_snapshot_with(
    window: &mut Option<NativeWindow>,
    policy: &crate::CapturePolicy,
    app: &zanei_core::schema::App,
    capture_enabled: bool,
    mut surface_changed: impl FnMut(),
    mut read_window: impl FnMut() -> Result<Option<NativeWindow>, NativeAxError>,
    read_value: impl FnOnce(bool) -> ValueSnapshot,
) -> (ValueSnapshot, Option<crate::CaptureDecision>) {
    if !capture_enabled {
        return (read_value(false), None);
    }
    let is_ide = matches!(
        app.name.trim().to_lowercase().as_str(),
        "cursor" | "visual studio code" | "code"
    );
    if is_ide {
        match read_window() {
            Ok(current) => {
                if !same_value_surface(window.as_ref(), current.as_ref()) {
                    surface_changed();
                }
                *window = current;
            }
            Err(error) => {
                surface_changed();
                *window = None;
                return (
                    suppressed_value_snapshot(FieldClass::Unknown, None, None, Some(error)),
                    None,
                );
            }
        }
    }
    let decision = policy.decision(
        zanei_core::privacy::PrivacyScope::TextContent,
        app,
        window.as_ref().and_then(|window| window.id),
        window.as_ref().and_then(|window| window.title.as_deref()),
    );
    let snapshot = read_value(decision.is_allowed());
    if is_ide && decision.is_allowed() {
        let failure = match read_window() {
            Ok(current) if same_value_surface(window.as_ref(), current.as_ref()) => {
                *window = current;
                return (snapshot, Some(decision));
            }
            Ok(current) => {
                *window = current;
                None
            }
            Err(error) => {
                *window = None;
                Some(error)
            }
        };
        surface_changed();
        return (
            suppressed_value_snapshot(FieldClass::Unknown, None, None, failure),
            None,
        );
    }
    (snapshot, Some(decision))
}

fn same_value_surface(previous: Option<&NativeWindow>, current: Option<&NativeWindow>) -> bool {
    if previous == current {
        return true;
    }
    let (Some(previous), Some(current)) = (previous, current) else {
        return false;
    };
    let (Some(previous_id), Some(current_id), Some(previous_title), Some(current_title)) = (
        previous.id,
        current.id,
        previous.title.as_deref(),
        current.title.as_deref(),
    ) else {
        return false;
    };
    // A leading dirty marker changes on edits; preserve the full remaining title and window ID.
    fn without_dirty(title: &str) -> &str {
        title
            .strip_prefix("● ")
            .or_else(|| title.strip_prefix("• "))
            .unwrap_or(title)
    }
    previous_id == current_id && without_dirty(previous_title) == without_dirty(current_title)
}

/// Reads only the mutable value surface used by `AXValueChanged` handling.
pub(super) fn value_snapshot(
    element: CfRef,
    capture_text_content: bool,
    secure_input: bool,
) -> ValueSnapshot {
    value_snapshot_with(
        value_field_snapshot(element, secure_input),
        capture_text_content,
        || copy_string(element, AX_VALUE_ATTRIBUTE),
        || {
            copy_attribute(element, AX_NUMBER_OF_CHARACTERS_ATTRIBUTE)
                .map(|value| value.and_then(|value| i64_value(value.as_ptr())))
        },
    )
}

pub(super) fn value_snapshot_with(
    classification: ValueFieldSnapshot,
    capture_text_content: bool,
    read_value: impl FnOnce() -> Result<Option<String>, NativeAxError>,
    read_count: impl FnOnce() -> Result<Option<i64>, NativeAxError>,
) -> ValueSnapshot {
    if classification.failure.is_some()
        || matches!(
            classification.field_class,
            FieldClass::SecureText | FieldClass::Unknown
        )
    {
        return suppressed_value_snapshot(
            classification.field_class,
            classification.role,
            classification.subrole,
            classification.failure,
        );
    }
    let ValueFieldSnapshot {
        role,
        subrole,
        field_class,
        ..
    } = classification;
    let value = match match field_class {
        FieldClass::KnownText(_) if capture_text_content => read_value(),
        FieldClass::KnownText(_) => Ok(None),
        FieldClass::KnownSafeNonText => gated_value(
            capture_text_content,
            field_class,
            role.as_deref(),
            read_value,
        ),
        FieldClass::SecureText | FieldClass::Unknown => Ok(None),
    } {
        Ok(value) => value,
        Err(error) => {
            trace_value_read_error(AX_VALUE_ATTRIBUTE, &error);
            return suppressed_value_snapshot(FieldClass::Unknown, role, subrole, Some(error));
        }
    };
    let character_count = match read_count() {
        Ok(character_count) => character_count,
        Err(error) => {
            trace_value_read_error(AX_NUMBER_OF_CHARACTERS_ATTRIBUTE, &error);
            return suppressed_value_snapshot(FieldClass::Unknown, role, subrole, Some(error));
        }
    };
    ValueSnapshot {
        value_len: value_length(character_count, value.as_deref()),
        value,
        role,
        subrole,
        field_class,
        failure: None,
    }
}

pub(super) fn value_field_snapshot(element: CfRef, secure_input: bool) -> ValueFieldSnapshot {
    if secure_input {
        return ValueFieldSnapshot {
            role: None,
            subrole: None,
            field_class: FieldClass::SecureText,
            registration_class: None,
            failure: None,
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
            registration_class: Some(field_class(role.as_deref(), subrole.as_deref())),
            role,
            subrole,
            failure: None,
        },
        (Err(error), _) | (_, Err(error)) => ValueFieldSnapshot {
            role: None,
            subrole: None,
            field_class: FieldClass::Unknown,
            registration_class: None,
            failure: Some(error),
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
    failure: Option<NativeAxError>,
) -> ValueSnapshot {
    ValueSnapshot {
        value: None,
        value_len: None,
        role,
        subrole,
        field_class,
        failure,
    }
}

pub(super) fn window_snapshot(window: CfRef) -> Result<Option<NativeWindow>, NativeAxError> {
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

pub(super) fn set_boolean_attribute(
    element: CfRef,
    attribute: &str,
    value: bool,
) -> Result<(), NativeAxError> {
    let attribute =
        cf_string(attribute).ok_or_else(|| native_error("CFStringCreateWithCString", -1))?;
    let status =
        unsafe { AXUIElementSetAttributeValue(element, attribute.as_ptr(), boolean_value(value)) };
    if status == AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(native_error("AXUIElementSetAttributeValue", status))
    }
}

pub(super) fn cf_equal(left: CfRef, right: CfRef) -> bool {
    unsafe { CFEqual(left, right) != 0 }
}

pub(super) fn element_role(element: CfRef) -> Result<String, NativeAxError> {
    let value = copy_required_attribute(element, "AXRole")?;
    string_value(value.as_ptr()).ok_or_else(|| native_error("AXRole attribute type", -1))
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
    match copy_required_attribute(element, attribute) {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if matches!(
                error.code(),
                AX_ERROR_ATTRIBUTE_UNSUPPORTED | AX_ERROR_NO_VALUE
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn copy_required_attribute(element: CfRef, attribute: &str) -> Result<OwnedCf, NativeAxError> {
    set_timeout(element)?;
    let attribute =
        cf_string(attribute).ok_or_else(|| native_error("CFStringCreateWithCString", -1))?;
    let mut value = ptr::null();
    let status =
        unsafe { AXUIElementCopyAttributeValue(element, attribute.as_ptr(), &raw mut value) };
    if status != AX_ERROR_SUCCESS {
        return Err(native_error("AXUIElementCopyAttributeValue", status));
    }
    unsafe { OwnedCf::from_create(value) }
        .ok_or_else(|| native_error("AXUIElementCopyAttributeValue", -1))
}

fn copy_string(element: CfRef, attribute: &str) -> Result<Option<String>, NativeAxError> {
    Ok(copy_attribute(element, attribute)?.and_then(|value| string_value(value.as_ptr())))
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> CfRef;
    fn AXUIElementGetPid(element: CfRef, pid: *mut i32) -> i32;
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
