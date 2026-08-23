//! Typed failures from Accessibility snapshot scans.

use std::fmt;

use crate::content_snapshot::{
    SnapshotAxError,
    walker::{SnapshotReadError, SnapshotWalkError},
};

#[derive(Debug)]
pub(crate) enum ScanError {
    Ax(SnapshotAxError),
    Walk(SnapshotWalkError),
}

impl From<SnapshotAxError> for ScanError {
    fn from(error: SnapshotAxError) -> Self {
        Self::Ax(error)
    }
}

pub(super) fn scan_timed_out(error: &ScanError) -> bool {
    match error {
        ScanError::Ax(error) => error.is_timeout(),
        ScanError::Walk(error) => match &error.source {
            SnapshotReadError::Ax(error) => error.is_timeout(),
            SnapshotReadError::Contract(_) => false,
        },
    }
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ax(error) => error.fmt(formatter),
            Self::Walk(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ax(error) => Some(error),
            Self::Walk(error) => Some(error),
        }
    }
}
