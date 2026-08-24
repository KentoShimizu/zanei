//! Validation for observations returned by Chrome's scripting boundary.

use crate::chrome::ChromeSnapshot;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SnapshotError {
    #[error("Chrome window identity is empty")]
    EmptyWindowIdentity,
    #[error("Chrome tab identity is empty")]
    EmptyTabIdentity,
    #[error("Chrome returned a non-absolute URL")]
    InvalidUrl,
}

pub(super) fn validate_snapshot(snapshot: &ChromeSnapshot) -> Result<(), SnapshotError> {
    if snapshot.window_key.is_empty() {
        return Err(SnapshotError::EmptyWindowIdentity);
    }
    if snapshot.tab_key.is_empty() {
        return Err(SnapshotError::EmptyTabIdentity);
    }
    if !is_absolute_uri(&snapshot.url) {
        return Err(SnapshotError::InvalidUrl);
    }
    Ok(())
}

fn is_absolute_uri(value: &str) -> bool {
    let Some((scheme, remainder)) = value.split_once(':') else {
        return false;
    };
    !scheme.is_empty()
        && scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic()
                || (index > 0 && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')))
        })
        && !remainder.is_empty()
        && !value.chars().any(char::is_whitespace)
}
