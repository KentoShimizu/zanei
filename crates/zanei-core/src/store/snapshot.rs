//! Plaintext SQLite snapshots of an (encrypted) store.
//!
//! `zanei export --format sqlite` hands users a regular SQLite file with the
//! same tables as the live store, so any SQLite tool can read their data
//! without the Keychain key. The snapshot is an ordinary store file: the
//! reader opens it like any other.

use std::path::Path;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, params, params_from_iter};
use time::OffsetDateTime;

use super::{QueryFilter, STORE_TABLES, StoreError, StoreKey, StoreReader, file_uri, reader};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotReport {
    pub events: u64,
}

/// Copies the events in `filter`'s time range (bounded by retention, like every
/// other read) from the store at `store` into a new plaintext SQLite file at `out`.
///
/// `out` must not exist yet, or must be an empty file the caller created with the
/// permissions it wants. Only `since`, `until`, and the retention window apply;
/// type and app filters are ignored so the snapshot stays a faithful copy.
pub fn export_plain_sqlite(
    store: &Path,
    key: Option<&StoreKey>,
    filter: &QueryFilter,
    configured_retention_hours: u64,
    out: &Path,
) -> Result<SnapshotReport, StoreError> {
    let (cutoff, since, until) = {
        let reader = StoreReader::open_with_key(store, key)?;
        let cutoff =
            reader.retention_cutoff_at(OffsetDateTime::now_utc(), configured_retention_hours)?;
        let since = reader::normalized_bound("since", filter.since.as_deref())?;
        let until = reader::normalized_bound("until", filter.until.as_deref())?;
        (cutoff, since, until)
    };

    let snapshot = Connection::open_with_flags(
        out,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let source_uri = file_uri(store, "mode=ro")?;
    match key {
        Some(key) => snapshot.execute(
            "ATTACH DATABASE ?1 AS src KEY ?2",
            params![source_uri, key.sqlcipher_literal().as_str()],
        )?,
        None => snapshot.execute("ATTACH DATABASE ?1 AS src", [source_uri])?,
    };
    let result = copy_range(&snapshot, &cutoff, since.as_deref(), until.as_deref());
    snapshot.execute_batch("DETACH DATABASE src;")?;
    let events = result?;
    Ok(SnapshotReport { events })
}

fn copy_range(
    snapshot: &Connection,
    cutoff: &str,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<u64, StoreError> {
    snapshot.execute_batch(STORE_TABLES)?;
    snapshot.execute(
        "UPDATE meta SET schema_version = (SELECT schema_version FROM src.meta)",
        [],
    )?;
    let mut conditions = vec!["ts >= ?".to_owned()];
    let mut parameters = vec![SqlValue::Text(cutoff.to_owned())];
    if let Some(since) = since {
        conditions.push("ts >= ?".to_owned());
        parameters.push(SqlValue::Text(since.to_owned()));
    }
    if let Some(until) = until {
        conditions.push("ts <= ?".to_owned());
        parameters.push(SqlValue::Text(until.to_owned()));
    }
    let sql = format!(
        "INSERT INTO events(id, ts, mono_ns, source, type, bundle_id, app_name, pid, \
         window_title, window_id, element_json, data_json, redaction_json) \
         SELECT id, ts, mono_ns, source, type, bundle_id, app_name, pid, \
         window_title, window_id, element_json, data_json, redaction_json \
         FROM src.events WHERE {} ORDER BY ts ASC, mono_ns ASC, id ASC",
        conditions.join(" AND ")
    );
    let inserted = snapshot.execute(&sql, params_from_iter(parameters.iter()))?;
    u64::try_from(inserted).map_err(|_| StoreError::NumericOverflow("events"))
}
