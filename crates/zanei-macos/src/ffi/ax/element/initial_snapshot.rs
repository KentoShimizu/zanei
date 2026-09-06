//! Initial focused and hit-tested element acquisition.

use super::{
    AX_NUMBER_OF_CHARACTERS_ATTRIBUTE, AX_VALUE_ATTRIBUTE, copy_attribute, copy_element,
    copy_string, gated_value, value_length, window_snapshot,
};
use crate::ffi::ax::{
    NativeAxError, NativeWindow,
    cf::{CfRef, i64_value},
};
use crate::focused_field::{FieldClass, field_class, observed_field_class};
use crate::{capture_policy::CaptureDecision, ffi::ax::NativeElement};

pub(in crate::ffi::ax) struct FocusedElementSnapshot {
    pub(in crate::ffi::ax) window: Option<NativeWindow>,
    pub(in crate::ffi::ax) element: NativeElement,
    pub(in crate::ffi::ax) text_baseline: Option<String>,
    pub(in crate::ffi::ax) field_class: FieldClass,
}

pub(in crate::ffi::ax) fn element_snapshot(
    element: CfRef,
    capture_decision: impl FnOnce(Option<&NativeWindow>) -> Option<CaptureDecision>,
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
    let capture_decision = capture_decision(window.as_ref());
    let capture_text_content = capture_decision
        .as_ref()
        .is_some_and(CaptureDecision::is_allowed);
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
            capture_decision: value.as_ref().and(capture_decision).map(Box::new),
            value,
            value_len,
        },
    )))
}

pub(in crate::ffi::ax) fn focused_element_snapshot(
    element: CfRef,
    capture_decision: impl FnOnce(Option<&NativeWindow>) -> Option<CaptureDecision>,
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
    let capture_decision = capture_decision(window.as_ref());
    let capture_text_content = capture_decision
        .as_ref()
        .is_some_and(CaptureDecision::is_allowed);
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
            capture_decision: value.as_ref().and(capture_decision).map(Box::new),
            value,
            value_len,
        },
        text_baseline,
        field_class,
    }))
}

pub(in crate::ffi::ax) const fn focused_element_is_excluded(field_class: FieldClass) -> bool {
    matches!(field_class, FieldClass::SecureText)
}
