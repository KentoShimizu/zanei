//! Plaintext SQLite snapshots of an (encrypted) store.
//!
//! `zanei export --format sqlite` hands users a regular SQLite file with the
//! same tables as the live store, so any SQLite tool can read their data
//! without the key. The snapshot is an ordinary store file: the reader opens
//! it like any other. Events from set-aside plaintext stores (see
//! [`super::retired`]) are included, like every other read.

use std::path::Path;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, params, params_from_iter};
use time::OffsetDateTime;

use super::{
    EventSelection, QueryFilter, SQLCIPHER_COMPATIBILITY, STORE_TABLES, StoreError, StoreFormat,
    StoreKey, StoreReader, file_uri, query, retired_plaintext_stores,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotReport {
    pub events: u64,
}

/// Copies the events in `filter`'s time range (bounded by retention, like every
/// other read) from the store at `store` into a new plaintext SQLite file at `out`.
///
/// `out` is taken literally, even when it looks like a `file:` URI, and must not
/// exist yet or must be an empty file the caller created with the permissions it
/// wants. `since`, `until`, `types`, and the retention window apply. App filters
/// are intentionally ignored because the CLI does not expose them for export.
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
        filter.validate()?;
        let since = query::normalized_bound("since", filter.since.as_deref())?;
        let until = query::normalized_bound("until", filter.until.as_deref())?;
        (cutoff, since, until)
    };

    // The bundled SQLite interprets `file:` names as URIs on every open, so the
    // destination is turned into an escaped URI here; a literal path that happens
    // to start with `file:` then names exactly that file.
    let out = std::path::absolute(out)
        .map_err(|error| StoreError::io("resolve the snapshot path", error))?;
    let snapshot = Connection::open_with_flags(
        file_uri(&out, "")?,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    snapshot.execute_batch(STORE_TABLES)?;
    let conditions =
        CopyConditions::new(&cutoff, since.as_deref(), until.as_deref(), &filter.types)?;

    // Sources are attached one at a time: SQLite allows ten attached databases,
    // and the number of set-aside stores is not bounded.
    let source_uri = file_uri(store, "mode=ro")?;
    match key {
        Some(key) => attach_encrypted_source(&snapshot, store, key)?,
        None => {
            snapshot.execute("ATTACH DATABASE ?1 AS src", [source_uri])?;
        }
    }
    // The snapshot's tables were just created at the current schema version,
    // which is what `meta` already says; a source's older version is not copied.
    let main_copy = copy_events(&snapshot, "src", &conditions);
    snapshot.execute_batch("DETACH DATABASE src;")?;
    let mut events = main_copy?;

    for retired in retired_plaintext_stores(store)? {
        let actual = StoreFormat::probe(&retired.path)?;
        if actual != StoreFormat::Plaintext {
            return Err(StoreError::UnexpectedStoreFormat {
                path: retired.path,
                expected: StoreFormat::Plaintext,
                actual,
            });
        }
        // `KEY ''` keeps SQLCipher from applying any key to this plaintext file.
        snapshot.execute(
            "ATTACH DATABASE ?1 AS retired KEY ''",
            [file_uri(&retired.path, "mode=ro")?],
        )?;
        let copied = copy_events(&snapshot, "retired", &conditions);
        snapshot.execute_batch("DETACH DATABASE retired;")?;
        events += copied?;
    }
    Ok(SnapshotReport { events })
}

/// Attaches the encrypted store at `store` as `src`, read-only, with the same
/// file-format pin `apply_key` gives a direct connection. `ATTACH … KEY` reads
/// the first page at once, so the pin has to be in force before it; SQLCipher
/// only offers that as a process-wide default, which this sets to the pinned
/// generation (also the library's own default) right before attaching.
pub(super) fn attach_encrypted_source(
    snapshot: &Connection,
    store: &Path,
    key: &StoreKey,
) -> Result<(), StoreError> {
    snapshot.pragma_update(
        None,
        "cipher_default_compatibility",
        SQLCIPHER_COMPATIBILITY,
    )?;
    snapshot.execute(
        "ATTACH DATABASE ?1 AS src KEY ?2",
        params![
            file_uri(store, "mode=ro")?,
            key.sqlcipher_literal().as_str()
        ],
    )?;
    Ok(())
}

struct CopyConditions {
    sql: String,
    parameters: Vec<SqlValue>,
}

impl CopyConditions {
    fn new(
        cutoff: &str,
        since: Option<&str>,
        until: Option<&str>,
        types: &[String],
    ) -> Result<Self, StoreError> {
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
        EventSelection {
            types: types.to_vec(),
            ..EventSelection::default()
        }
        .append_predicate(&mut conditions, &mut parameters)?;
        Ok(Self {
            sql: conditions.join(" AND "),
            parameters,
        })
    }
}

/// Copies one source's events in range. Sources are copied live store first,
/// then set-aside stores oldest first, and an id that is already in the
/// snapshot keeps its first copy: sources can overlap, for example after a
/// backup of the previous store was restored and set aside a second time.
fn copy_events(
    snapshot: &Connection,
    alias: &str,
    conditions: &CopyConditions,
) -> Result<u64, StoreError> {
    let sql = format!(
        "INSERT INTO events(id, ts, mono_ns, source, type, bundle_id, app_name, pid, \
         window_title, window_id, element_json, data_json, redaction_json) \
         SELECT id, ts, mono_ns, source, type, bundle_id, app_name, pid, \
         window_title, window_id, element_json, data_json, redaction_json \
         FROM {alias}.events AS source WHERE {} \
         AND NOT EXISTS (SELECT 1 FROM main.events WHERE id = source.id)",
        conditions.sql
    );
    let count = snapshot.execute(&sql, params_from_iter(conditions.parameters.iter()))?;
    u64::try_from(count).map_err(|_| StoreError::NumericOverflow("events"))
}
