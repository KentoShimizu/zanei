//! SQLite-backed event persistence.
//!
//! Stores are SQLCipher databases. A [`StoreKey`] unlocks them; the on-disk
//! [`StoreFormat`] decides whether a key is needed, so stores written before
//! encryption existed stay readable. The recorder never rewrites such a store:
//! it sets the file aside (see [`retired`]) and readers merge it back in.

mod append_sequence;
mod context_page;
mod error;
mod event_metadata;
mod event_row;
mod key;
mod key_store;
mod query;
mod reader;
mod retired;
mod selection;
mod snapshot;
mod types;

#[cfg(feature = "write")]
mod migration;
#[cfg(feature = "write")]
mod writer;

use rusqlite::Connection;

pub use append_sequence::AppendHead;
pub use context_page::{
    ContextCursor, ContextGap, ContextGapReason, ContextObservation, ContextPage, ContextPageError,
    ContextPageRequest, ContextPageResult, ContextRange, ContextText, MAX_CONTEXT_PAGE_ROWS,
    MAX_CONTEXT_TEXT_BYTES,
};
pub use error::{LockedReason, StoreError, StoreFailureKind};
pub use event_metadata::{EventMetadata, MetadataFilter};
pub use key::{STORE_KEY_BYTES, StoreFormat, StoreKey};
pub use key_store::{KeyStore, KeyStoreError, KeyStoreInteraction, load_or_create};
pub use query::{QueryFilter, QueryResult};
pub use reader::{SkippedRetired, StoreReader};
pub use retired::{RetiredPlaintext, remove_retired, resolve_store_path, retired_plaintext_stores};
pub use selection::EventSelection;
pub use snapshot::{SnapshotReport, export_plain_sqlite};
pub use types::{DaemonMode, DaemonState, HEARTBEAT_STALE_AFTER_SECONDS, StoreStatus};

#[cfg(feature = "write")]
pub use retired::{RetiredRetention, purge_retired_plaintext, set_aside_plaintext};
#[cfg(feature = "write")]
pub use writer::{PurgeFilter, StoreWriter};

const LEGACY_STORE_SCHEMA_VERSION: i64 = 1;
const DAEMON_IDENTITY_STORE_SCHEMA_VERSION: i64 = 2;
const RETENTION_STORE_SCHEMA_VERSION: i64 = 3;
const COLLECTOR_FAILURES_STORE_SCHEMA_VERSION: i64 = 4;
const PERMISSIONS_SNAPSHOT_STORE_SCHEMA_VERSION: i64 = 5;
const CONTENT_SNAPSHOT_STORE_SCHEMA_VERSION: i64 = 6;
const CAPABILITIES_STORE_SCHEMA_VERSION: i64 = 7;
const STORE_SCHEMA_VERSION: i64 = 8;

/// SQLCipher file-format generation pinned so a future library default cannot
/// silently make existing stores unreadable.
const SQLCIPHER_COMPATIBILITY: i64 = 4;

/// Tables, indexes, and seed rows of a store. Plain DDL without pragmas so the
/// same text serves the live store and plaintext snapshots.
pub(super) const STORE_TABLES: &str = "
CREATE TABLE IF NOT EXISTS events (
    append_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    ts TEXT NOT NULL,
    mono_ns INTEGER NOT NULL,
    source TEXT NOT NULL,
    type TEXT NOT NULL,
    bundle_id TEXT,
    app_name TEXT,
    pid INTEGER,
    window_title TEXT,
    window_id INTEGER,
    element_json TEXT,
    data_json TEXT,
    redaction_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);
CREATE INDEX IF NOT EXISTS idx_events_type_ts ON events(type, ts);
CREATE INDEX IF NOT EXISTS idx_events_bundle_ts ON events(bundle_id, ts);

CREATE TABLE IF NOT EXISTS store_identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    identity TEXT NOT NULL DEFAULT (lower(hex(randomblob(16))))
);
INSERT OR IGNORE INTO store_identity(id) VALUES (1);

CREATE TABLE IF NOT EXISTS daemon_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    pid INTEGER,
    started_at TEXT,
    instance_id TEXT,
    mode TEXT,
    heartbeat_at TEXT,
    retention_hours INTEGER CHECK (retention_hours > 0),
    paused_until TEXT,
    events_captured INTEGER NOT NULL DEFAULT 0,
    events_dropped INTEGER NOT NULL DEFAULT 0,
    last_event_ts TEXT,
    degraded_json TEXT,
    collector_failures_json TEXT NOT NULL DEFAULT '{}',
    last_known_capabilities_json TEXT
);
INSERT OR IGNORE INTO daemon_state(id) VALUES (1);

CREATE TABLE IF NOT EXISTS daemon_capabilities (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    snapshot_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS meta (
    schema_version INTEGER NOT NULL
);
INSERT INTO meta(schema_version)
SELECT 8 WHERE NOT EXISTS (SELECT 1 FROM meta);
";

/// The instant before which events are outside a `retention_hours` window.
fn retention_boundary(
    now: time::OffsetDateTime,
    retention_hours: u64,
) -> Result<time::OffsetDateTime, StoreError> {
    let seconds = retention_hours
        .checked_mul(60 * 60)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(StoreError::NumericOverflow("retention_hours"))?;
    now.checked_sub(time::Duration::seconds(seconds))
        .ok_or(StoreError::NumericOverflow("retention_hours"))
}

fn retention_cutoff(now: time::OffsetDateTime, retention_hours: u64) -> Result<String, StoreError> {
    Ok(crate::normalize::format_timestamp(retention_boundary(
        now,
        retention_hours,
    )?))
}

/// `path` with `suffix` appended to its file name (`store.sqlite` → `store.sqlite-wal`).
fn sibling(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut sibling = path.as_os_str().to_os_string();
    sibling.push(suffix);
    std::path::PathBuf::from(sibling)
}

fn remove_if_exists(path: &std::path::Path) -> Result<(), StoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StoreError::io("remove a stale store file", error)),
    }
}

/// Applies `key` to a freshly opened connection. Must run before any other
/// statement touches the database.
fn apply_key(connection: &Connection, key: &StoreKey) -> Result<(), StoreError> {
    connection.pragma_update(None, "key", key.sqlcipher_literal().as_str())?;
    connection.pragma_update(None, "cipher_compatibility", SQLCIPHER_COMPATIBILITY)?;
    Ok(())
}

/// Touches the schema so a wrong key surfaces as [`LockedReason::KeyMismatch`]
/// instead of a generic "file is not a database" error later on.
fn verify_key(connection: &Connection) -> Result<(), StoreError> {
    match connection.query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
        row.get::<_, i64>(0)
    }) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ffi::ErrorCode::NotADatabase =>
        {
            Err(StoreError::Locked(LockedReason::KeyMismatch))
        }
        Err(error) => Err(error.into()),
    }
}

/// Unlocks `connection` according to the file's format and the key on hand.
/// Plaintext stores ignore the key; encrypted stores require it.
fn unlock(
    connection: &Connection,
    format: StoreFormat,
    key: Option<&StoreKey>,
) -> Result<(), StoreError> {
    match (format, key) {
        (StoreFormat::Encrypted, Some(key)) => {
            apply_key(connection, key)?;
            verify_key(connection)
        }
        (StoreFormat::Encrypted, None) => Err(StoreError::Locked(LockedReason::KeyMissing)),
        // Damaged files are opened without a key so SQLite reports the corruption.
        (StoreFormat::Plaintext | StoreFormat::Missing | StoreFormat::Unrecognized, _) => Ok(()),
    }
}

/// The URI to hand SQLite for the store at `path`. The bundled SQLite treats
/// `file:` names as URIs on every open, so a literal path is always wrapped in
/// an escaped URI of its own and can never be reinterpreted.
fn store_uri(path: &std::path::Path) -> Result<String, StoreError> {
    let absolute = std::path::absolute(path)
        .map_err(|error| StoreError::io("resolve the store path", error))?;
    file_uri(&absolute, "")
}

/// Escapes a path for use inside a `file:` URI filename. SQLite reads the path
/// as UTF-8 and decodes `%XX`, so only the characters that would start the
/// query or fragment need escaping; everything else passes through unchanged.
fn file_uri(path: &std::path::Path, query: &str) -> Result<String, StoreError> {
    let text = path.to_str().ok_or_else(|| {
        StoreError::io(
            "resolve the store path",
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "store path is not valid UTF-8",
            ),
        )
    })?;
    let mut uri = String::with_capacity(text.len() + query.len() + 8);
    uri.push_str("file:");
    for character in text.chars() {
        match character {
            '%' | '?' | '#' => uri.push_str(&format!("%{:02X}", character as u32)),
            other => uri.push(other),
        }
    }
    if !query.is_empty() {
        uri.push('?');
        uri.push_str(query);
    }
    Ok(uri)
}

#[cfg(test)]
mod uri_tests {
    use super::file_uri;

    #[test]
    fn file_uri_keeps_non_ascii_and_escapes_only_delimiters() {
        let uri =
            file_uri(std::path::Path::new("/tmp/日本語 50%?#.sqlite"), "mode=ro").expect("uri");
        assert_eq!(uri, "file:/tmp/日本語 50%25%3F%23.sqlite?mode=ro");
    }
}

#[cfg(all(test, feature = "write"))]
mod tests;
