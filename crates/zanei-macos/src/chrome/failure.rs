//! Privacy-safe Chrome dependency failures and lifecycle state.

use std::{
    fmt,
    sync::{Arc, PoisonError, RwLock},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChromeFailure {
    Query(ChromeQueryFailure),
    Parse(ChromeParseFailure),
    Validation(ChromeValidationFailure),
}

impl ChromeFailure {
    #[must_use]
    pub const fn phase(self) -> ChromeFailurePhase {
        match self {
            Self::Query(_) => ChromeFailurePhase::Query,
            Self::Parse(_) => ChromeFailurePhase::Parse,
            Self::Validation(_) => ChromeFailurePhase::Validation,
        }
    }
}

impl fmt::Display for ChromeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query(ChromeQueryFailure::AppleEvent(code)) => {
                write!(formatter, "phase=query kind=apple_event code={code}")
            }
            Self::Query(kind) => write!(formatter, "phase=query kind={}", kind.name()),
            Self::Parse(kind) => write!(formatter, "phase=parse kind={}", kind.name()),
            Self::Validation(kind) => {
                write!(formatter, "phase=validation kind={}", kind.name())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChromeFailurePhase {
    Query,
    Parse,
    Validation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChromeQueryFailure {
    AppleEvent(i64),
    AppleEventCodeUnavailable,
    RuntimeUnavailable,
}

impl ChromeQueryFailure {
    const fn name(self) -> &'static str {
        match self {
            Self::AppleEvent(_) => "apple_event",
            Self::AppleEventCodeUnavailable => "apple_event_code_unavailable",
            Self::RuntimeUnavailable => "runtime_unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChromeParseFailure {
    EmptyResponse,
    InvalidResponseShape,
    MissingText,
    UnknownStatus,
    UnsupportedWindowMode,
    InvalidString,
}

impl ChromeParseFailure {
    const fn name(self) -> &'static str {
        match self {
            Self::EmptyResponse => "empty_response",
            Self::InvalidResponseShape => "invalid_response_shape",
            Self::MissingText => "missing_text",
            Self::UnknownStatus => "unknown_status",
            Self::UnsupportedWindowMode => "unsupported_window_mode",
            Self::InvalidString => "invalid_string",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChromeValidationFailure {
    EmptyWindowIdentity,
    EmptyTabIdentity,
    WindowIdentityMismatch,
    InvalidUrl,
    MissingApplication,
}

impl ChromeValidationFailure {
    const fn name(self) -> &'static str {
        match self {
            Self::EmptyWindowIdentity => "empty_window_identity",
            Self::EmptyTabIdentity => "empty_tab_identity",
            Self::WindowIdentityMismatch => "window_identity_mismatch",
            Self::InvalidUrl => "invalid_url",
            Self::MissingApplication => "missing_application",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChromeFailureState {
    #[default]
    Available,
    Unavailable(ChromeFailure),
}

impl ChromeFailureState {
    #[must_use]
    pub const fn current(self) -> Option<ChromeFailure> {
        match self {
            Self::Available => None,
            Self::Unavailable(failure) => Some(failure),
        }
    }

    fn observe_failure(&mut self, failure: ChromeFailure) -> Option<ChromeFailureTransition> {
        match *self {
            Self::Available => {
                *self = Self::Unavailable(failure);
                Some(ChromeFailureTransition::Occurred(failure))
            }
            Self::Unavailable(previous) if previous != failure => {
                *self = Self::Unavailable(failure);
                Some(ChromeFailureTransition::Changed {
                    previous,
                    current: failure,
                })
            }
            Self::Unavailable(_) => None,
        }
    }

    fn observe_success(&mut self) -> Option<ChromeFailureTransition> {
        let Self::Unavailable(previous) = *self else {
            return None;
        };
        *self = Self::Available;
        Some(ChromeFailureTransition::Recovered(previous))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChromeFailureTransition {
    Occurred(ChromeFailure),
    Changed {
        previous: ChromeFailure,
        current: ChromeFailure,
    },
    Recovered(ChromeFailure),
}

impl fmt::Display for ChromeFailureTransition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Occurred(failure) => write!(formatter, "state=unavailable {failure}"),
            Self::Changed { previous, current } => {
                write!(
                    formatter,
                    "state=unavailable previous=({previous}) current=({current})"
                )
            }
            Self::Recovered(previous) => {
                write!(formatter, "state=available recovered_from=({previous})")
            }
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct ChromeFailurePublisher {
    state: Arc<RwLock<ChromeFailureState>>,
}

impl ChromeFailurePublisher {
    pub(super) fn state(&self) -> ChromeFailureState {
        *self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn observe_failure(&self, failure: ChromeFailure) {
        let transition = self
            .state
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .observe_failure(failure);
        trace_transition(transition);
    }

    pub(super) fn observe_success(&self) {
        let transition = self
            .state
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .observe_success();
        trace_transition(transition);
    }
}

fn trace_transition(transition: Option<ChromeFailureTransition>) {
    if let Some(transition) = transition {
        crate::trace::trace!("component=chrome action=failure_transition {transition}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: ChromeFailure = ChromeFailure::Query(ChromeQueryFailure::AppleEvent(-1712));

    #[test]
    fn state_records_occurrence_change_and_recovery() {
        let mut state = ChromeFailureState::default();

        assert_eq!(
            state.observe_failure(TIMEOUT),
            Some(ChromeFailureTransition::Occurred(TIMEOUT))
        );
        assert_eq!(state.observe_failure(TIMEOUT), None);

        let invalid_url = ChromeFailure::Validation(ChromeValidationFailure::InvalidUrl);
        assert_eq!(
            state.observe_failure(invalid_url),
            Some(ChromeFailureTransition::Changed {
                previous: TIMEOUT,
                current: invalid_url,
            })
        );
        assert_eq!(
            state.observe_success(),
            Some(ChromeFailureTransition::Recovered(invalid_url))
        );
        assert_eq!(state, ChromeFailureState::Available);
        assert_eq!(state.observe_success(), None);
        assert_eq!(
            ChromeFailureTransition::Occurred(TIMEOUT).to_string(),
            "state=unavailable phase=query kind=apple_event code=-1712"
        );
    }
}
