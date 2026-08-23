//! Pure authorization and value-capture state for macOS text observations.

mod authorization;
mod focused_target;
mod policy;
mod quarantine;
mod value;

#[cfg(test)]
pub(crate) use authorization::AUTHORIZATION_QUEUE_CAPACITY;
pub(crate) use authorization::InputAuthorization;
pub use authorization::{
    InputAuthorizationPublisher, InputAuthorizations, input_authorization_channel,
};
pub(crate) use focused_target::FocusedTarget;
pub use policy::TextContentPolicy;
pub(crate) use quarantine::{ChromeWindowKey, TextQuarantine};
pub(crate) use value::{FocusChangeCapture, ValueCapture, ValueEmission, ValueObservation};
#[cfg(test)]
pub(crate) use value::{VALUE_DEBOUNCE, VALUE_MAX_HOLD};

#[cfg(test)]
pub(crate) use authorization::INPUT_WINDOW;

#[cfg(test)]
mod tests;
