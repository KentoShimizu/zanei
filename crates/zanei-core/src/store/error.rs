use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::PathBuf;

use super::StoreFormat;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreFailureKind {
    Unavailable,
    Corrupt,
    /// The store is encrypted and cannot be opened with the available key.
    Locked,
}

/// Why an encrypted store could not be opened.
///
/// The platform-specific variants carry the platform's own wording (see
/// [`super::KeyStoreError`]); core itself does not know what a keychain is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LockedReason {
    /// The store is encrypted but no key is available.
    KeyMissing,
    /// A key was supplied but it does not decrypt the store.
    KeyMismatch,
    /// The key store is locked; the text says how to unlock it.
    KeyStoreLocked(String),
    /// The platform refused this process access to the key; the text says what to do.
    KeyStoreDenied(String),
    /// The key could not be read for another reason.
    KeyUnavailable(String),
}

impl Display for LockedReason {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyMissing => formatter.write_str(
                "the store is encrypted but no key for it is available; if the key is gone \
                 the recorded data cannot be recovered: run `zanei stop`, move the store \
                 aside, then `zanei start`",
            ),
            Self::KeyMismatch => formatter.write_str(
                "the available key does not decrypt this store (it was encrypted with a \
                 different key, or the file is not a Zanei store); move the store aside, \
                 then `zanei start`",
            ),
            Self::KeyStoreLocked(advice) | Self::KeyStoreDenied(advice) => {
                formatter.write_str(advice)
            }
            Self::KeyUnavailable(reason) => {
                write!(formatter, "the store key is unavailable: {reason}")
            }
        }
    }
}

#[derive(Debug)]
pub enum StoreError {
    Database(rusqlite::Error),
    Io {
        operation: &'static str,
        source: std::io::Error,
    },
    Locked(LockedReason),
    InvalidKey(&'static str),
    KeyGeneration(String),
    InvalidJson {
        field: &'static str,
        source: serde_json::Error,
    },
    InvalidTimestamp {
        field: &'static str,
        value: String,
    },
    InvalidTypePattern(String),
    InvalidDaemonMode(String),
    InvalidDaemonState(&'static str),
    UnexpectedStoreFormat {
        path: PathBuf,
        expected: StoreFormat,
        actual: StoreFormat,
    },
    UnsupportedSchemaVersion(i64),
    NumericOverflow(&'static str),
}

impl StoreError {
    pub(crate) const fn invalid_timestamp(field: &'static str, value: String) -> Self {
        Self::InvalidTimestamp { field, value }
    }

    pub(crate) const fn invalid_json(field: &'static str, source: serde_json::Error) -> Self {
        Self::InvalidJson { field, source }
    }

    /// An I/O failure while handling the store or its key; `operation` reads as
    /// "failed to {operation}: …".
    #[must_use]
    pub const fn io(operation: &'static str, source: std::io::Error) -> Self {
        Self::Io { operation, source }
    }

    /// Classifies a failure encountered while opening or reading a store.
    #[must_use]
    pub fn failure_kind(&self) -> StoreFailureKind {
        match self {
            Self::Database(error) => database_failure_kind(error),
            Self::Locked(_) => StoreFailureKind::Locked,
            Self::InvalidJson { .. }
            | Self::InvalidTimestamp { .. }
            | Self::InvalidDaemonMode(_)
            | Self::InvalidDaemonState(_)
            | Self::UnexpectedStoreFormat { .. }
            | Self::UnsupportedSchemaVersion(_)
            | Self::NumericOverflow(_) => StoreFailureKind::Corrupt,
            Self::Io { .. }
            | Self::InvalidKey(_)
            | Self::KeyGeneration(_)
            | Self::InvalidTypePattern(_) => StoreFailureKind::Unavailable,
        }
    }
}

fn database_failure_kind(error: &rusqlite::Error) -> StoreFailureKind {
    use rusqlite::Error as SqliteError;
    use rusqlite::ffi::ErrorCode;

    match error {
        SqliteError::SqliteFailure(error, _) => match error.code {
            ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase | ErrorCode::Unknown => {
                StoreFailureKind::Corrupt
            }
            _ => StoreFailureKind::Unavailable,
        },
        SqliteError::FromSqlConversionFailure(..)
        | SqliteError::IntegralValueOutOfRange(..)
        | SqliteError::Utf8Error(_)
        | SqliteError::QueryReturnedNoRows
        | SqliteError::InvalidColumnIndex(_)
        | SqliteError::InvalidColumnName(_)
        | SqliteError::InvalidColumnType(..) => StoreFailureKind::Corrupt,
        _ => StoreFailureKind::Unavailable,
    }
}

impl Display for StoreError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "SQLite store error: {error}"),
            Self::Io { operation, source } => write!(formatter, "failed to {operation}: {source}"),
            Self::Locked(reason) => write!(formatter, "store is locked: {reason}"),
            Self::InvalidKey(reason) => write!(formatter, "invalid store key: {reason}"),
            Self::KeyGeneration(reason) => {
                write!(formatter, "failed to generate a store key: {reason}")
            }
            Self::InvalidJson { field, source } => {
                write!(formatter, "invalid JSON in {field}: {source}")
            }
            Self::InvalidTimestamp { field, value } => {
                write!(formatter, "invalid RFC3339 timestamp in {field}: {value}")
            }
            Self::InvalidTypePattern(pattern) => write!(
                formatter,
                "invalid event type pattern {pattern:?}: only one trailing '*' is allowed"
            ),
            Self::InvalidDaemonMode(mode) => {
                write!(formatter, "invalid daemon mode in store: {mode}")
            }
            Self::InvalidDaemonState(message) => {
                write!(formatter, "invalid daemon state: {message}")
            }
            Self::UnexpectedStoreFormat {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "unexpected store format at {}: expected {}, found {}",
                path.display(),
                expected.as_str(),
                actual.as_str()
            ),
            Self::UnsupportedSchemaVersion(version) => {
                write!(formatter, "unsupported store schema version: {version}")
            }
            Self::NumericOverflow(field) => {
                write!(formatter, "{field} exceeds SQLite's signed integer range")
            }
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::InvalidJson { source, .. } => Some(source),
            Self::Locked(_)
            | Self::InvalidKey(_)
            | Self::KeyGeneration(_)
            | Self::InvalidTimestamp { .. }
            | Self::InvalidTypePattern(_)
            | Self::InvalidDaemonMode(_)
            | Self::InvalidDaemonState(_)
            | Self::UnexpectedStoreFormat { .. }
            | Self::UnsupportedSchemaVersion(_)
            | Self::NumericOverflow(_) => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{LockedReason, StoreError, StoreFailureKind};

    #[test]
    fn invalid_persisted_values_are_store_corruption() {
        assert_eq!(
            StoreError::UnsupportedSchemaVersion(99).failure_kind(),
            StoreFailureKind::Corrupt
        );
        assert_eq!(
            StoreError::InvalidDaemonMode("unknown".to_owned()).failure_kind(),
            StoreFailureKind::Corrupt
        );
    }

    #[test]
    fn locked_stores_are_neither_corrupt_nor_unavailable() {
        assert_eq!(
            StoreError::Locked(LockedReason::KeyMissing).failure_kind(),
            StoreFailureKind::Locked
        );
        assert!(
            StoreError::Locked(LockedReason::KeyStoreLocked("unlock it first".to_owned()))
                .to_string()
                .ends_with("unlock it first")
        );
    }
}
