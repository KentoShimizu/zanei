use std::error::Error;
use std::fmt::{self, Display, Formatter};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreFailureKind {
    Unavailable,
    Corrupt,
}

#[derive(Debug)]
pub enum StoreError {
    Database(rusqlite::Error),
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

    /// Classifies a failure encountered while opening or reading a store.
    #[must_use]
    pub fn failure_kind(&self) -> StoreFailureKind {
        match self {
            Self::Database(error) => database_failure_kind(error),
            Self::InvalidJson { .. }
            | Self::InvalidTimestamp { .. }
            | Self::InvalidDaemonMode(_)
            | Self::InvalidDaemonState(_)
            | Self::UnsupportedSchemaVersion(_)
            | Self::NumericOverflow(_) => StoreFailureKind::Corrupt,
            Self::InvalidTypePattern(_) => StoreFailureKind::Unavailable,
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
            Self::InvalidJson { source, .. } => Some(source),
            Self::InvalidTimestamp { .. }
            | Self::InvalidTypePattern(_)
            | Self::InvalidDaemonMode(_)
            | Self::InvalidDaemonState(_)
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
    use super::{StoreError, StoreFailureKind};

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
}
