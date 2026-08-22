use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use rusqlite::{Connection, Transaction, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::schema::Event;

use super::{
    COLLECTOR_FAILURES_STORE_SCHEMA_VERSION, DAEMON_IDENTITY_STORE_SCHEMA_VERSION, DaemonMode,
    DaemonState, LEGACY_STORE_SCHEMA_VERSION, LockedReason, RETENTION_STORE_SCHEMA_VERSION,
    SQLCIPHER_COMPATIBILITY, STORE_SCHEMA_VERSION, STORE_TABLES, StoreError, StoreFormat, StoreKey,
    apply_key, retention_cutoff, verify_key,
};

const BUSY_TIMEOUT_MILLISECONDS: u64 = 5_000;
const STORE_PRAGMAS: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA busy_timeout=5000;
PRAGMA auto_vacuum=INCREMENTAL;
";
const MIGRATION_STAGING_SUFFIX: &str = ".migrating";

pub struct StoreWriter {
    connection: Connection,
    format: StoreFormat,
}

impl StoreWriter {
    /// Opens or creates a plaintext store. Encrypted stores fail with
    /// [`LockedReason::KeyMissing`]; use [`Self::open_with_key`] for those.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with_key(path, None)
    }

    /// Opens a store for writing, creating it when it does not exist.
    ///
    /// A new store is encrypted when `key` is given. An existing store keeps its
    /// format: plaintext stores ignore the key (the recorder migrates them
    /// separately with [`migrate_to_encrypted`]), encrypted stores require it.
    /// New files are created owner-readable only.
    pub fn open_with_key(
        path: impl AsRef<Path>,
        key: Option<&StoreKey>,
    ) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let format = StoreFormat::probe(path)?;
        Self::open_known(path, format, key)
    }

    /// Like [`Self::open_with_key`] for a caller that already probed the format.
    /// Required when this process holds another connection to the store: a
    /// fresh probe would release that connection's file lock (see
    /// [`StoreFormat::probe`]).
    pub fn open_known(
        path: impl AsRef<Path>,
        format: StoreFormat,
        key: Option<&StoreKey>,
    ) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if format == StoreFormat::Missing {
            create_private_file(path)?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(StdDuration::from_millis(BUSY_TIMEOUT_MILLISECONDS))?;
        let format = match (format, key) {
            (StoreFormat::Encrypted, Some(key)) => {
                apply_key(&connection, key)?;
                verify_key(&connection)?;
                StoreFormat::Encrypted
            }
            (StoreFormat::Encrypted, None) => {
                return Err(StoreError::Locked(LockedReason::KeyMissing));
            }
            (StoreFormat::Missing, Some(key)) => {
                apply_key(&connection, key)?;
                StoreFormat::Encrypted
            }
            (StoreFormat::Missing, None) | (StoreFormat::Plaintext, _) => StoreFormat::Plaintext,
            // Opening a damaged file without a key lets SQLite report the corruption.
            (StoreFormat::Unrecognized, _) => StoreFormat::Unrecognized,
        };
        connection.execute_batch(STORE_PRAGMAS)?;
        connection.execute_batch(STORE_TABLES)?;
        migrate_schema(&connection)?;
        Ok(Self { connection, format })
    }

    /// The on-disk format the store was opened (or created) as.
    #[must_use]
    pub const fn format(&self) -> StoreFormat {
        self.format
    }

    pub fn append(&mut self, event: &Event) -> Result<(), StoreError> {
        self.append_batch(std::slice::from_ref(event))?;
        Ok(())
    }

    pub fn append_batch(&mut self, events: &[Event]) -> Result<usize, StoreError> {
        if events.is_empty() {
            return Ok(0);
        }
        let prepared = events
            .iter()
            .map(PreparedEvent::from_event)
            .collect::<Result<Vec<_>, _>>()?;
        let transaction = self.connection.transaction()?;
        insert_events(&transaction, &prepared)?;
        let captured = i64::try_from(prepared.len())
            .map_err(|_| StoreError::NumericOverflow("events_captured"))?;
        transaction.execute(
            "UPDATE daemon_state SET events_captured = events_captured + ?1, \
             last_event_ts = ?2 WHERE id = 1",
            params![captured, prepared.last().map(|event| event.ts)],
        )?;
        transaction.commit()?;
        Ok(prepared.len())
    }

    /// Persists retained events and the latest daemon snapshot in one transaction.
    ///
    /// Event progress remains owned by the database so a heartbeat snapshot prepared
    /// before this transaction cannot overwrite counters advanced by the same batch.
    pub fn persist(
        &mut self,
        events: &[Event],
        state: Option<&DaemonState>,
    ) -> Result<usize, StoreError> {
        let prepared = events
            .iter()
            .map(PreparedEvent::from_event)
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(state) = state {
            validate_daemon_state(state)?;
        }
        let transaction = self.connection.transaction()?;
        if !prepared.is_empty() {
            insert_events(&transaction, &prepared)?;
            increment_event_progress(&transaction, &prepared)?;
        }
        if let Some(state) = state {
            write_daemon_snapshot(&transaction, state, EventProgress::Preserve)?;
        }
        transaction.commit()?;
        Ok(prepared.len())
    }

    pub fn purge_before(&mut self, cutoff: &str) -> Result<usize, StoreError> {
        let cutoff = parse_timestamp("cutoff", cutoff)?;
        let cutoff = crate::normalize::format_timestamp(cutoff);
        let transaction = self.connection.transaction()?;
        let deleted = transaction.execute("DELETE FROM events WHERE ts < ?1", [&cutoff])?;
        transaction.commit()?;
        self.connection
            .execute_batch("PRAGMA incremental_vacuum;")?;
        Ok(deleted)
    }

    pub fn purge_all(&mut self) -> Result<usize, StoreError> {
        let transaction = self.connection.transaction()?;
        let deleted = transaction.execute("DELETE FROM events", [])?;
        transaction.commit()?;
        self.connection
            .execute_batch("PRAGMA incremental_vacuum;")?;
        Ok(deleted)
    }

    pub fn purge_retention(
        &mut self,
        now: OffsetDateTime,
        retention_hours: u64,
    ) -> Result<usize, StoreError> {
        let cutoff = retention_cutoff(now, retention_hours)?;
        self.purge_before(&cutoff)
    }

    pub fn write_daemon_state(&self, state: &DaemonState) -> Result<(), StoreError> {
        validate_daemon_state(state)?;
        let transaction = self.connection.unchecked_transaction()?;
        write_daemon_snapshot(&transaction, state, EventProgress::Replace)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn set_paused_until(&self, paused_until: Option<&str>) -> Result<(), StoreError> {
        validate_paused_until(paused_until)?;
        self.connection.execute(
            "UPDATE daemon_state SET paused_until = ?1 WHERE id = 1",
            [paused_until],
        )?;
        Ok(())
    }

    pub fn increment_events_dropped(&self, count: u64) -> Result<(), StoreError> {
        let count = signed("events_dropped", count)?;
        self.connection.execute(
            "UPDATE daemon_state SET events_dropped = events_dropped + ?1 WHERE id = 1",
            [count],
        )?;
        Ok(())
    }
}

/// Result of [`migrate_to_encrypted`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationOutcome {
    /// The store was already encrypted or does not exist yet.
    Unchanged,
    /// A plaintext store was rewritten as an encrypted one.
    Migrated { events: u64 },
}

/// Rewrites a plaintext store at `path` as a SQLCipher store keyed with `key`.
///
/// The encrypted copy is built in a sibling `.migrating` file and swapped in with
/// a single rename, so a crash leaves either the old plaintext store or the new
/// encrypted one, never a mix. Calling it again on the result is a no-op. The
/// caller must hold the recorder's store ownership; readers may keep reading the
/// old inode until they close it.
pub fn migrate_to_encrypted(path: &Path, key: &StoreKey) -> Result<MigrationOutcome, StoreError> {
    if StoreFormat::probe(path)? != StoreFormat::Plaintext {
        return Ok(MigrationOutcome::Unchanged);
    }
    let staging = migration_staging_path(path);
    remove_if_exists(&staging)?;
    create_private_file(&staging)?;
    let events = match export_to_encrypted(path, &staging, key) {
        Ok(events) => events,
        Err(error) => {
            remove_if_exists(&staging)?;
            return Err(error);
        }
    };
    File::open(&staging)
        .and_then(|file| file.sync_all())
        .map_err(|error| StoreError::io("sync the encrypted store copy", error))?;
    fs::rename(&staging, path)
        .map_err(|error| StoreError::io("replace the store with its encrypted copy", error))?;
    for suffix in ["-wal", "-shm"] {
        remove_if_exists(&sibling(path, suffix))?;
    }
    Ok(MigrationOutcome::Migrated { events })
}

fn export_to_encrypted(path: &Path, staging: &Path, key: &StoreKey) -> Result<u64, StoreError> {
    let staging_text = staging.to_str().ok_or_else(|| {
        StoreError::io(
            "resolve the store path",
            io::Error::new(io::ErrorKind::InvalidInput, "store path is not valid UTF-8"),
        )
    })?;
    let connection = Connection::open(path)?;
    connection.busy_timeout(StdDuration::from_millis(BUSY_TIMEOUT_MILLISECONDS))?;
    // Fold the WAL into the main file first; the copy reads through this
    // connection either way, but a truncated WAL leaves less to clean up.
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let events = connection.query_row("SELECT count(*) FROM events", [], |row| {
        row.get::<_, i64>(0)
    })?;
    connection.execute(
        "ATTACH DATABASE ?1 AS enc KEY ?2",
        params![staging_text, key.sqlcipher_literal().as_str()],
    )?;
    let export = (|| {
        connection.execute_batch(&format!(
            "PRAGMA enc.cipher_compatibility = {SQLCIPHER_COMPATIBILITY}; \
             PRAGMA enc.auto_vacuum = INCREMENTAL;"
        ))?;
        connection.query_row("SELECT sqlcipher_export('enc')", [], |_| Ok(()))?;
        Ok::<(), StoreError>(())
    })();
    connection.execute_batch("DETACH DATABASE enc;")?;
    export?;
    u64::try_from(events).map_err(|_| StoreError::NumericOverflow("events"))
}

fn migration_staging_path(path: &Path) -> PathBuf {
    sibling(path, MIGRATION_STAGING_SUFFIX)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut sibling = path.as_os_str().to_os_string();
    sibling.push(suffix);
    PathBuf::from(sibling)
}

fn remove_if_exists(path: &Path) -> Result<(), StoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StoreError::io("remove a stale store file", error)),
    }
}

/// Creates `path` as an empty file only the owner can read, leaving an existing
/// (empty) file alone. SQLite treats an empty file as a new database and copies
/// its mode to the journal files it creates next to it.
fn create_private_file(path: &Path) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(StoreError::io("create the store file", error)),
    }
}

struct PreparedEvent<'event> {
    event: &'event Event,
    mono_ns: i64,
    element_json: Option<String>,
    data_json: String,
    redaction_json: String,
    ts: &'event str,
}

impl<'event> PreparedEvent<'event> {
    fn from_event(event: &'event Event) -> Result<Self, StoreError> {
        serde_json::to_value(event).map_err(|error| StoreError::invalid_json("event", error))?;
        validate_timestamp("event.ts", &event.ts)?;
        let mono_ns =
            i64::try_from(event.mono_ns).map_err(|_| StoreError::NumericOverflow("mono_ns"))?;
        let element_json = event
            .element
            .as_ref()
            .map(|element| {
                serde_json::to_string(element)
                    .map_err(|error| StoreError::invalid_json("element_json", error))
            })
            .transpose()?;
        let data_json = serde_json::to_string(&event.data)
            .map_err(|error| StoreError::invalid_json("data_json", error))?;
        let redaction_json = serde_json::to_string(&event.redaction)
            .map_err(|error| StoreError::invalid_json("redaction_json", error))?;
        Ok(Self {
            event,
            mono_ns,
            element_json,
            data_json,
            redaction_json,
            ts: &event.ts,
        })
    }
}

fn insert_events(
    transaction: &Transaction<'_>,
    events: &[PreparedEvent<'_>],
) -> Result<(), StoreError> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO events( \
         id, ts, mono_ns, source, type, bundle_id, app_name, pid, \
         window_title, window_id, element_json, data_json, redaction_json \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )?;
    for prepared in events {
        let event = prepared.event;
        statement.execute(params![
            event.id,
            event.ts,
            prepared.mono_ns,
            event.source,
            event.event_type,
            event.app.bundle_id,
            event.app.name,
            event.app.pid,
            event
                .window
                .as_ref()
                .and_then(|window| window.title.as_ref()),
            event.window.as_ref().and_then(|window| window.id),
            prepared.element_json,
            prepared.data_json,
            prepared.redaction_json,
        ])?;
    }
    Ok(())
}

fn increment_event_progress(
    transaction: &Transaction<'_>,
    events: &[PreparedEvent<'_>],
) -> Result<(), StoreError> {
    let captured =
        i64::try_from(events.len()).map_err(|_| StoreError::NumericOverflow("events_captured"))?;
    transaction.execute(
        "UPDATE daemon_state SET events_captured = events_captured + ?1, \
         last_event_ts = ?2 WHERE id = 1",
        params![captured, events.last().map(|event| event.ts)],
    )?;
    Ok(())
}

#[derive(Clone, Copy)]
enum EventProgress {
    Preserve,
    Replace,
}

fn write_daemon_snapshot(
    transaction: &Transaction<'_>,
    state: &DaemonState,
    event_progress: EventProgress,
) -> Result<(), StoreError> {
    let events_dropped = signed("events_dropped", state.events_dropped)?;
    let retention_hours = state
        .retention_hours
        .map(|value| signed("retention_hours", value))
        .transpose()?;
    let degraded_json = serialize_degraded(&state.degraded)?;
    let collector_failures_json = serialize_collector_failures(&state.collector_failures)?;
    match event_progress {
        EventProgress::Preserve => {
            transaction.execute(
                "UPDATE daemon_state SET pid = ?1, started_at = ?2, instance_id = ?3, \
                 mode = ?4, heartbeat_at = ?5, retention_hours = ?6, paused_until = ?7, \
                 events_dropped = ?8, degraded_json = ?9, collector_failures_json = ?10 \
                 WHERE id = 1",
                params![
                    state.pid,
                    state.started_at,
                    state.instance_id,
                    state.mode.map(DaemonMode::as_str),
                    state.heartbeat_at,
                    retention_hours,
                    state.paused_until,
                    events_dropped,
                    degraded_json,
                    collector_failures_json,
                ],
            )?;
        }
        EventProgress::Replace => {
            let events_captured = signed("events_captured", state.events_captured)?;
            transaction.execute(
                "UPDATE daemon_state SET pid = ?1, started_at = ?2, instance_id = ?3, \
                 mode = ?4, heartbeat_at = ?5, retention_hours = ?6, paused_until = ?7, \
                 events_captured = ?8, events_dropped = ?9, last_event_ts = ?10, \
                 degraded_json = ?11, collector_failures_json = ?12 WHERE id = 1",
                params![
                    state.pid,
                    state.started_at,
                    state.instance_id,
                    state.mode.map(DaemonMode::as_str),
                    state.heartbeat_at,
                    retention_hours,
                    state.paused_until,
                    events_captured,
                    events_dropped,
                    state.last_event_ts,
                    degraded_json,
                    collector_failures_json,
                ],
            )?;
        }
    }
    write_permissions(transaction, state)
}

fn write_permissions(transaction: &Transaction<'_>, state: &DaemonState) -> Result<(), StoreError> {
    let permissions_json = state
        .permissions
        .as_ref()
        .map(serialize_permissions)
        .transpose()?;
    if let Some(permissions_json) = permissions_json {
        transaction.execute(
            "INSERT INTO daemon_permissions(id, snapshot_json) VALUES (1, ?1) \
             ON CONFLICT(id) DO UPDATE SET snapshot_json = excluded.snapshot_json",
            [&permissions_json],
        )?;
        transaction.execute(
            "UPDATE daemon_state SET last_known_permissions_json = ?1 WHERE id = 1",
            [&permissions_json],
        )?;
    } else {
        transaction.execute("DELETE FROM daemon_permissions WHERE id = 1", [])?;
    }
    Ok(())
}

fn validate_daemon_state(state: &DaemonState) -> Result<(), StoreError> {
    validate_optional_timestamp("started_at", state.started_at.as_deref())?;
    validate_optional_timestamp("heartbeat_at", state.heartbeat_at.as_deref())?;
    validate_paused_until(state.paused_until.as_deref())?;
    validate_optional_timestamp("last_event_ts", state.last_event_ts.as_deref())?;

    let identity = (
        state.pid,
        state.started_at.as_deref(),
        state.instance_id.as_deref(),
        state.mode,
    );
    if state.retention_hours == Some(0) {
        return Err(StoreError::InvalidDaemonState(
            "retention_hours must be greater than zero",
        ));
    }
    match (
        state.heartbeat_at.is_some(),
        identity,
        state.retention_hours,
    ) {
        (false, (None, None, None, None), None) => Ok(()),
        (true, (Some(pid), Some(started_at), Some(instance_id), Some(_)), Some(_)) => {
            if instance_id == format!("{pid}@{started_at}") {
                Ok(())
            } else {
                Err(StoreError::InvalidDaemonState(
                    "instance_id must match pid and started_at",
                ))
            }
        }
        (false, _, _) => Err(StoreError::InvalidDaemonState(
            "stopped state must not retain recorder identity or retention",
        )),
        (true, _, _) => Err(StoreError::InvalidDaemonState(
            "running state requires pid, started_at, instance_id, mode, and retention_hours",
        )),
    }
}

fn validate_optional_timestamp(field: &'static str, value: Option<&str>) -> Result<(), StoreError> {
    value
        .map(|value| validate_timestamp(field, value))
        .transpose()?;
    Ok(())
}

fn validate_paused_until(value: Option<&str>) -> Result<(), StoreError> {
    if value != Some("infinity") {
        validate_optional_timestamp("paused_until", value)?;
    }
    Ok(())
}

fn validate_timestamp(field: &'static str, value: &str) -> Result<(), StoreError> {
    parse_timestamp(field, value).map(|_| ())
}

fn parse_timestamp(field: &'static str, value: &str) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| StoreError::invalid_timestamp(field, value.to_owned()))
}

fn signed(field: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::NumericOverflow(field))
}

fn serialize_degraded(degraded: &BTreeMap<String, String>) -> Result<String, StoreError> {
    serde_json::to_string(degraded)
        .map_err(|error| StoreError::invalid_json("degraded_json", error))
}

fn serialize_collector_failures(
    collector_failures: &BTreeMap<String, u64>,
) -> Result<String, StoreError> {
    serde_json::to_string(collector_failures)
        .map_err(|error| StoreError::invalid_json("collector_failures_json", error))
}

fn serialize_permissions(permissions: &super::DaemonPermissions) -> Result<String, StoreError> {
    serde_json::to_string(permissions)
        .map_err(|error| StoreError::invalid_json("permissions_json", error))
}

fn migrate_schema(connection: &Connection) -> Result<(), StoreError> {
    let version = connection.query_row("SELECT schema_version FROM meta", [], |row| row.get(0))?;
    let prior_statements = match version {
        STORE_SCHEMA_VERSION => return Ok(()),
        LEGACY_STORE_SCHEMA_VERSION => {
            "ALTER TABLE daemon_state ADD COLUMN instance_id TEXT; \
             ALTER TABLE daemon_state ADD COLUMN mode TEXT; \
             ALTER TABLE daemon_state ADD COLUMN retention_hours INTEGER \
             CHECK (retention_hours > 0); \
             ALTER TABLE daemon_state ADD COLUMN collector_failures_json TEXT NOT NULL \
             DEFAULT '{}';"
        }
        DAEMON_IDENTITY_STORE_SCHEMA_VERSION => {
            "ALTER TABLE daemon_state ADD COLUMN retention_hours INTEGER \
             CHECK (retention_hours > 0); \
             ALTER TABLE daemon_state ADD COLUMN collector_failures_json TEXT NOT NULL \
             DEFAULT '{}';"
        }
        RETENTION_STORE_SCHEMA_VERSION => {
            "ALTER TABLE daemon_state ADD COLUMN collector_failures_json TEXT NOT NULL \
             DEFAULT '{}';"
        }
        COLLECTOR_FAILURES_STORE_SCHEMA_VERSION => "",
        _ => return Err(StoreError::UnsupportedSchemaVersion(version)),
    };
    let transaction = connection.unchecked_transaction()?;
    transaction.execute_batch(prior_statements)?;
    transaction.execute_batch(
        "ALTER TABLE daemon_state ADD COLUMN last_known_permissions_json TEXT; \
         UPDATE daemon_state SET last_known_permissions_json = \
             (SELECT snapshot_json FROM daemon_permissions WHERE id = 1) \
         WHERE id = 1;",
    )?;
    transaction.execute(
        "UPDATE meta SET schema_version = ?1",
        [STORE_SCHEMA_VERSION],
    )?;
    transaction.commit()?;
    Ok(())
}
