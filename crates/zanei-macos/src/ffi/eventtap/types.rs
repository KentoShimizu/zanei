//! Decoded EventTap observations passed to the Rust worker.

use crate::{
    eventtap::logic::KeyObservation, ffi::eventtap::NativeContext, focused_field::FocusedField,
    text_capture::InputAuthorization,
};
use time::OffsetDateTime;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeDisableReason {
    Timeout,
    UserInput,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NativeEvent {
    Key {
        observation: KeyObservation,
        target: Option<NativeInputTarget>,
        authorization: Option<InputAuthorization>,
        secure_input: bool,
        ime_active: bool,
        observed_at: OffsetDateTime,
    },
    Scroll {
        vertical: f64,
        horizontal: f64,
        observed_at: OffsetDateTime,
    },
    MouseDown {
        x: f64,
        y: f64,
        button: u32,
        click_count: i64,
        observed_at: OffsetDateTime,
    },
    Disabled(NativeDisableReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeInputTarget {
    pub(crate) context: NativeContext,
    pub(crate) focused_field: Option<FocusedField>,
    pub(crate) focus_generation: u64,
}
