//! Main-thread input-source monitoring with an atomic EventTap read path.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::ffi::input_source::NativeInputSourceObserver;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputSourceType {
    KeyboardInputMode,
    KeyboardLayout,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImeClassification {
    KeyboardInputMode,
    KeyboardLayout,
    Other,
    InferredInputMethod,
    Unknown,
}

pub(crate) fn input_source_uses_ime(
    // `None` means `kTISPropertyInputSourceType` could not be read.
    source_type: Option<InputSourceType>,
    input_source_id: Option<&str>,
    input_mode_id: Option<&str>,
) -> bool {
    !matches!(
        classify_input_source(source_type, input_source_id, input_mode_id),
        ImeClassification::KeyboardLayout
    )
}

fn classify_input_source(
    source_type: Option<InputSourceType>,
    input_source_id: Option<&str>,
    input_mode_id: Option<&str>,
) -> ImeClassification {
    match source_type {
        Some(InputSourceType::KeyboardInputMode) => ImeClassification::KeyboardInputMode,
        Some(InputSourceType::KeyboardLayout) => ImeClassification::KeyboardLayout,
        Some(InputSourceType::Other) => ImeClassification::Other,
        None if [input_source_id, input_mode_id]
            .into_iter()
            .flatten()
            .any(|value| value.to_ascii_lowercase().contains("inputmethod")) =>
        {
            ImeClassification::InferredInputMethod
        }
        None => ImeClassification::Unknown,
    }
}

#[derive(Clone)]
pub(crate) struct ImeState {
    active: Arc<AtomicBool>,
}

impl ImeState {
    pub(crate) fn new() -> Self {
        Self {
            // Unknown state suppresses text until the main-thread observer refreshes it.
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(crate) fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

pub struct InputSourceObserver {
    _native: NativeInputSourceObserver,
}

impl InputSourceObserver {
    pub(crate) fn new(state: &ImeState) -> Option<Self> {
        NativeInputSourceObserver::new(Arc::clone(&state.active))
            .map(|native| Self { _native: native })
    }
}

#[cfg(test)]
mod tests {
    use super::{ImeClassification, InputSourceType, classify_input_source, input_source_uses_ime};

    #[test]
    fn keyboard_input_mode_is_ime_without_inputmethod_identifier() {
        assert!(input_source_uses_ime(
            Some(InputSourceType::KeyboardInputMode),
            Some("com.third-party.custom-ime"),
            None,
        ));
    }

    #[test]
    fn keyboard_layout_is_not_ime() {
        assert!(!input_source_uses_ime(
            Some(InputSourceType::KeyboardLayout),
            Some("com.apple.keylayout.ABC"),
            None,
        ));
    }

    #[test]
    fn source_type_takes_precedence_over_inputmethod_identifier() {
        assert!(!input_source_uses_ime(
            Some(InputSourceType::KeyboardLayout),
            Some("com.example.inputmethod.named-layout"),
            None,
        ));
    }

    #[test]
    fn other_input_source_type_fails_closed() {
        assert!(input_source_uses_ime(
            Some(InputSourceType::Other),
            Some("com.example.keyboard"),
            None,
        ));
    }

    #[test]
    fn input_source_type_property_failure_fails_closed() {
        assert_eq!(
            classify_input_source(None, Some("com.apple.keylayout.ABC"), None),
            ImeClassification::Unknown,
        );
        assert!(input_source_uses_ime(
            None,
            Some("com.apple.keylayout.ABC"),
            None,
        ));
    }

    #[test]
    fn missing_type_uses_inputmethod_identifier_as_ime_hint() {
        assert_eq!(
            classify_input_source(None, Some("com.apple.InputMethod.Custom"), None),
            ImeClassification::InferredInputMethod,
        );
        assert!(input_source_uses_ime(
            None,
            Some("com.apple.InputMethod.Custom"),
            None,
        ));
    }
}
