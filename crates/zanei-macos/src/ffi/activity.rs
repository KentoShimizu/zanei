//! Thin CoreGraphics wrapper for session-wide input recency.

use std::fmt;

const COMBINED_SESSION_STATE: i32 = 0;
const ANY_INPUT_EVENT_TYPE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ActivityError {
    NonFinite { seconds: f64 },
    Negative { seconds: f64 },
}

impl fmt::Display for ActivityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinite { seconds } => {
                write!(formatter, "input recency is not finite: {seconds}")
            }
            Self::Negative { seconds } => {
                write!(formatter, "input recency is negative: {seconds}")
            }
        }
    }
}

impl std::error::Error for ActivityError {}

pub fn seconds_since_last_input() -> Result<f64, ActivityError> {
    // SAFETY: this public CoreGraphics query does not create an EventTap or request TCC access.
    let seconds = unsafe {
        CGEventSourceSecondsSinceLastEventType(COMBINED_SESSION_STATE, ANY_INPUT_EVENT_TYPE)
    };
    validate_seconds(seconds)
}

fn validate_seconds(seconds: f64) -> Result<f64, ActivityError> {
    if !seconds.is_finite() {
        Err(ActivityError::NonFinite { seconds })
    } else if seconds < 0.0 {
        Err(ActivityError::Negative { seconds })
    } else {
        Ok(seconds)
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_non_negative_values_satisfy_the_contract() {
        assert_eq!(validate_seconds(0.0), Ok(0.0));
        assert_eq!(validate_seconds(42.5), Ok(42.5));
    }

    #[test]
    fn non_finite_and_negative_values_are_contract_errors() {
        assert!(matches!(
            validate_seconds(f64::NAN),
            Err(ActivityError::NonFinite { seconds }) if seconds.is_nan()
        ));
        assert!(matches!(
            validate_seconds(f64::INFINITY),
            Err(ActivityError::NonFinite { seconds }) if seconds.is_infinite()
        ));
        assert_eq!(
            validate_seconds(-0.5),
            Err(ActivityError::Negative { seconds: -0.5 })
        );
    }
}
