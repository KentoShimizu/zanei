//! Validation for observations returned by Chrome's scripting boundary.

use crate::chrome::{ChromeQuery, ChromeSnapshot};

#[derive(Debug, thiserror::Error)]
pub(crate) enum SnapshotError {
    #[error("Chrome window identity is empty")]
    EmptyWindowIdentity,
    #[error("Chrome tab identity is empty")]
    EmptyTabIdentity,
    #[error("Chrome returned a different window identity")]
    WindowIdentityMismatch,
    #[error("Chrome returned a non-absolute URL")]
    InvalidUrl,
}

pub(super) fn validate_query_snapshot(
    query: &ChromeQuery,
    snapshot: &ChromeSnapshot,
) -> Result<(), SnapshotError> {
    validate_snapshot(snapshot)?;
    if query
        .applescript_window_id()
        .is_some_and(|expected| expected != &snapshot.applescript_window_id)
    {
        return Err(SnapshotError::WindowIdentityMismatch);
    }
    Ok(())
}

pub(super) fn validate_snapshot(snapshot: &ChromeSnapshot) -> Result<(), SnapshotError> {
    if snapshot.applescript_window_id.as_str().is_empty() {
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
