//! SQLite-backed event persistence.

mod error;
mod reader;
mod types;

#[cfg(feature = "write")]
mod writer;

pub use error::{StoreError, StoreFailureKind};
pub use reader::StoreReader;
pub use types::{
    DaemonMode, DaemonPermissions, DaemonState, HEARTBEAT_STALE_AFTER_SECONDS, PermissionState,
    QueryFilter, StoreStatus,
};

#[cfg(feature = "write")]
pub use writer::StoreWriter;

const LEGACY_STORE_SCHEMA_VERSION: i64 = 1;
const DAEMON_IDENTITY_STORE_SCHEMA_VERSION: i64 = 2;
const RETENTION_STORE_SCHEMA_VERSION: i64 = 3;
const COLLECTOR_FAILURES_STORE_SCHEMA_VERSION: i64 = 4;
const STORE_SCHEMA_VERSION: i64 = 5;

fn retention_cutoff(now: time::OffsetDateTime, retention_hours: u64) -> Result<String, StoreError> {
    let seconds = retention_hours
        .checked_mul(60 * 60)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(StoreError::NumericOverflow("retention_hours"))?;
    let cutoff = now
        .checked_sub(time::Duration::seconds(seconds))
        .ok_or(StoreError::NumericOverflow("retention_hours"))?;
    Ok(crate::normalize::format_timestamp(cutoff))
}

#[cfg(all(test, feature = "write"))]
mod tests;
