use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration as StdDuration;

use rusqlite::{Connection, Transaction, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::schema::Event;

use super::{
    DAEMON_IDENTITY_STORE_SCHEMA_VERSION, DaemonMode, DaemonState, LEGACY_STORE_SCHEMA_VERSION,
    RETENTION_STORE_SCHEMA_VERSION, STORE_SCHEMA_VERSION, StoreError, retention_cutoff,
};

const BUSY_TIMEOUT_MILLISECONDS: u64 = 5_000;
const DATABASE_SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA busy_timeout=5000;
PRAGMA auto_vacuum=INCREMENTAL;

CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY,
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
    collector_failures_json TEXT NOT NULL DEFAULT '{}'
);
INSERT OR IGNORE INTO daemon_state(id) VALUES (1);

CREATE TABLE IF NOT EXISTS daemon_permissions (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    snapshot_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS meta (
    schema_version INTEGER NOT NULL
);
INSERT INTO meta(schema_version)
SELECT 4 WHERE NOT EXISTS (SELECT 1 FROM meta);
";

pub struct StoreWriter {
    connection: Connection,
}

impl StoreWriter {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(StdDuration::from_millis(BUSY_TIMEOUT_MILLISECONDS))?;
        connection.execute_batch(DATABASE_SCHEMA)?;
        migrate_schema(&connection)?;
        Ok(Self { connection })
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
            [permissions_json],
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
    let statements = match version {
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
        _ => return Err(StoreError::UnsupportedSchemaVersion(version)),
    };
    let transaction = connection.unchecked_transaction()?;
    transaction.execute_batch(statements)?;
    transaction.execute(
        "UPDATE meta SET schema_version = ?1",
        [STORE_SCHEMA_VERSION],
    )?;
    transaction.commit()?;
    Ok(())
}
