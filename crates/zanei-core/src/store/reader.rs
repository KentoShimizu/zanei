use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration as StdDuration;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params_from_iter};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::schema::{
    App, ClipboardOrigin, Element, Event, EventData, Redaction, Window, is_known_event_type,
};

use super::{
    DAEMON_IDENTITY_STORE_SCHEMA_VERSION, DaemonMode, DaemonPermissions,
    HEARTBEAT_STALE_AFTER_SECONDS, LEGACY_STORE_SCHEMA_VERSION, QueryFilter,
    RETENTION_STORE_SCHEMA_VERSION, STORE_SCHEMA_VERSION, StoreError, StoreStatus,
    retention_cutoff,
};

const BUSY_TIMEOUT_MILLISECONDS: u64 = 5_000;

pub struct StoreReader {
    connection: Connection,
    schema_version: i64,
}

impl StoreReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(StdDuration::from_millis(BUSY_TIMEOUT_MILLISECONDS))?;
        let schema_version = readable_schema_version(&connection)?;
        Ok(Self {
            connection,
            schema_version,
        })
    }

    pub fn query(
        &self,
        filter: &QueryFilter,
        configured_retention_hours: u64,
    ) -> Result<Vec<Event>, StoreError> {
        let now = OffsetDateTime::now_utc();
        let retention_hours = self.effective_retention_hours_at(now, configured_retention_hours)?;
        let cutoff = retention_cutoff(now, retention_hours)?;
        let (sql, parameters) = build_query(filter, &cutoff)?;
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(parameters.iter()))?;
        let mut events = Vec::new();

        while let Some(row) = rows.next()? {
            events.push(read_event(row)?);
        }
        Ok(events)
    }

    fn effective_retention_hours_at(
        &self,
        now: OffsetDateTime,
        configured_retention_hours: u64,
    ) -> Result<u64, StoreError> {
        if self.schema_version < RETENTION_STORE_SCHEMA_VERSION {
            return Ok(configured_retention_hours);
        }
        let state = self
            .connection
            .query_row(
                "SELECT heartbeat_at, retention_hours FROM daemon_state WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                    ))
                },
            )
            .optional()?;
        let Some((heartbeat_at, retention_hours)) = state else {
            return Ok(configured_retention_hours);
        };
        let running = heartbeat_is_fresh(heartbeat_at.as_deref(), now)?;
        let retention_hours = if running {
            retention_hours
                .map(|value| positive_unsigned("retention_hours", value))
                .transpose()?
        } else {
            None
        };
        Ok(StoreStatus {
            running,
            retention_hours,
            ..StoreStatus::default()
        }
        .effective_retention_hours(configured_retention_hours))
    }

    pub fn status(&self) -> Result<StoreStatus, StoreError> {
        self.status_at(OffsetDateTime::now_utc())
    }

    pub fn oldest_event_ts(&self) -> Result<Option<String>, StoreError> {
        let timestamp = self
            .connection
            .query_row("SELECT MIN(ts) FROM events", [], |row| {
                row.get::<_, Option<String>>(0)
            })?;
        if let Some(value) = timestamp.as_deref() {
            parse_timestamp("oldest_event_ts", value)?;
        }
        Ok(timestamp)
    }

    pub fn status_at(&self, now: OffsetDateTime) -> Result<StoreStatus, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let sql = match self.schema_version {
            LEGACY_STORE_SCHEMA_VERSION => {
                "SELECT pid, started_at, NULL, NULL, heartbeat_at, NULL, paused_until, \
                 events_captured, events_dropped, last_event_ts, degraded_json, NULL \
                 FROM daemon_state WHERE id = 1"
            }
            DAEMON_IDENTITY_STORE_SCHEMA_VERSION => {
                "SELECT pid, started_at, instance_id, mode, heartbeat_at, NULL, paused_until, \
                 events_captured, events_dropped, last_event_ts, degraded_json, NULL \
                 FROM daemon_state WHERE id = 1"
            }
            RETENTION_STORE_SCHEMA_VERSION => {
                "SELECT pid, started_at, instance_id, mode, heartbeat_at, retention_hours, \
                 paused_until, events_captured, events_dropped, last_event_ts, degraded_json, NULL \
                 FROM daemon_state WHERE id = 1"
            }
            STORE_SCHEMA_VERSION => {
                "SELECT pid, started_at, instance_id, mode, heartbeat_at, retention_hours, \
                 paused_until, events_captured, events_dropped, last_event_ts, degraded_json, \
                 collector_failures_json \
                 FROM daemon_state WHERE id = 1"
            }
            _ => unreachable!("schema version is validated when the reader opens"),
        };
        let state = transaction
            .query_row(sql, [], |row| {
                Ok(PersistedStatus {
                    pid: row.get(0)?,
                    started_at: row.get(1)?,
                    instance_id: row.get(2)?,
                    mode: row.get(3)?,
                    heartbeat_at: row.get(4)?,
                    retention_hours: row.get(5)?,
                    paused_until: row.get(6)?,
                    events_captured: row.get(7)?,
                    events_dropped: row.get(8)?,
                    last_event_ts: row.get(9)?,
                    degraded_json: row.get(10)?,
                    collector_failures_json: row.get(11)?,
                })
            })
            .optional()?;

        let permissions = read_daemon_permissions(&transaction)?;
        transaction.commit()?;
        state.map_or_else(
            || Ok(StoreStatus::default()),
            |state| state.derive(now, permissions),
        )
    }
}

struct PersistedStatus {
    pid: Option<i64>,
    started_at: Option<String>,
    instance_id: Option<String>,
    mode: Option<String>,
    heartbeat_at: Option<String>,
    retention_hours: Option<i64>,
    paused_until: Option<String>,
    events_captured: i64,
    events_dropped: i64,
    last_event_ts: Option<String>,
    degraded_json: Option<String>,
    collector_failures_json: Option<String>,
}

impl PersistedStatus {
    fn derive(
        self,
        now: OffsetDateTime,
        permissions: Option<DaemonPermissions>,
    ) -> Result<StoreStatus, StoreError> {
        let running = heartbeat_is_fresh(self.heartbeat_at.as_deref(), now)?;
        let pause_requested = match self.paused_until.as_deref() {
            None => false,
            Some("infinity") => true,
            Some(value) => parse_timestamp("paused_until", value)? > now,
        };
        if let Some(value) = self.last_event_ts.as_deref() {
            parse_timestamp("last_event_ts", value)?;
        }
        let persisted_degraded = self
            .degraded_json
            .as_deref()
            .map(|json| {
                serde_json::from_str::<BTreeMap<String, String>>(json)
                    .map_err(|error| StoreError::invalid_json("degraded_json", error))
            })
            .transpose()?
            .unwrap_or_default();
        let degraded = if running {
            persisted_degraded
        } else {
            BTreeMap::new()
        };
        let collector_failures = self
            .collector_failures_json
            .as_deref()
            .map(|json| {
                serde_json::from_str::<BTreeMap<String, u64>>(json)
                    .map_err(|error| StoreError::invalid_json("collector_failures_json", error))
            })
            .transpose()?
            .unwrap_or_default();
        let mode = self.mode.map(|mode| parse_daemon_mode(&mode)).transpose()?;
        let retention_hours = self
            .retention_hours
            .map(|value| positive_unsigned("retention_hours", value))
            .transpose()?;

        Ok(StoreStatus {
            running,
            paused: running && pause_requested,
            pid: self.pid,
            started_at: self.started_at,
            instance_id: self.instance_id,
            mode,
            heartbeat_at: self.heartbeat_at,
            retention_hours,
            paused_until: self.paused_until,
            events_captured: unsigned("events_captured", self.events_captured)?,
            events_dropped: unsigned("events_dropped", self.events_dropped)?,
            last_event_ts: self.last_event_ts,
            degraded,
            collector_failures,
            permissions,
        })
    }
}

fn read_daemon_permissions(
    connection: &Connection,
) -> Result<Option<DaemonPermissions>, StoreError> {
    let table_exists = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'daemon_permissions')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !table_exists {
        return Ok(None);
    }
    let json = connection
        .query_row(
            "SELECT snapshot_json FROM daemon_permissions WHERE id = 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    json.map(|json| {
        serde_json::from_str(&json)
            .map_err(|error| StoreError::invalid_json("permissions_json", error))
    })
    .transpose()
}

fn build_query(
    filter: &QueryFilter,
    retention_cutoff: &str,
) -> Result<(String, Vec<SqlValue>), StoreError> {
    filter.validate()?;
    let mut conditions = Vec::new();
    let mut parameters = Vec::new();

    conditions.push("ts >= ?".to_owned());
    parameters.push(SqlValue::Text(retention_cutoff.to_owned()));

    add_optional_bound(
        &mut conditions,
        &mut parameters,
        "ts >= ?",
        "since",
        filter.since.as_deref(),
    )?;
    add_optional_bound(
        &mut conditions,
        &mut parameters,
        "ts <= ?",
        "until",
        filter.until.as_deref(),
    )?;
    add_type_filter(&mut conditions, &mut parameters, &filter.types);
    add_optional_text(
        &mut conditions,
        &mut parameters,
        "app_name = ?",
        filter.app.as_deref(),
    );
    add_optional_text(
        &mut conditions,
        &mut parameters,
        "bundle_id = ?",
        filter.bundle_id.as_deref(),
    );

    let mut sql = String::from(
        "SELECT id, ts, mono_ns, source, type, bundle_id, app_name, pid, \
         window_title, window_id, element_json, data_json, redaction_json FROM events",
    );
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(" ORDER BY ts ASC, mono_ns ASC, id ASC");
    if let Some(limit) = filter.limit {
        let limit = i64::try_from(limit).map_err(|_| StoreError::NumericOverflow("limit"))?;
        sql.push_str(" LIMIT ?");
        parameters.push(SqlValue::Integer(limit));
    }

    Ok((sql, parameters))
}

fn add_optional_bound(
    conditions: &mut Vec<String>,
    parameters: &mut Vec<SqlValue>,
    condition: &str,
    field: &'static str,
    value: Option<&str>,
) -> Result<(), StoreError> {
    if let Some(value) = value {
        let timestamp = parse_timestamp(field, value)?;
        conditions.push(condition.to_owned());
        parameters.push(SqlValue::Text(crate::normalize::format_timestamp(
            timestamp,
        )));
    }
    Ok(())
}

fn add_optional_text(
    conditions: &mut Vec<String>,
    parameters: &mut Vec<SqlValue>,
    condition: &str,
    value: Option<&str>,
) {
    if let Some(value) = value {
        conditions.push(condition.to_owned());
        parameters.push(SqlValue::Text(value.to_owned()));
    }
}

fn add_type_filter(
    conditions: &mut Vec<String>,
    parameters: &mut Vec<SqlValue>,
    patterns: &[String],
) {
    if patterns.is_empty() {
        return;
    }

    let mut type_conditions = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        if let Some(prefix) = pattern.strip_suffix('*') {
            type_conditions.push("type LIKE ? ESCAPE '\\'".to_owned());
            parameters.push(SqlValue::Text(format!("{}%", escape_like(prefix))));
        } else {
            type_conditions.push("type = ?".to_owned());
            parameters.push(SqlValue::Text(pattern.clone()));
        }
    }
    conditions.push(format!("({})", type_conditions.join(" OR ")));
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn read_event(row: &rusqlite::Row<'_>) -> Result<Event, StoreError> {
    let event_type: String = row.get(4)?;
    if !is_known_event_type(&event_type) {
        return Err(StoreError::invalid_json(
            "event",
            <serde_json::Error as serde::de::Error>::custom(format!(
                "unknown event type in v1 store: {event_type}"
            )),
        ));
    }
    let data_json: String = row.get(11)?;
    let data_value = serde_json::from_str(&data_json)
        .map_err(|error| StoreError::invalid_json("data_json", error))?;
    let data = EventData::from_type_and_value(&event_type, data_value)
        .map_err(|error| StoreError::invalid_json("data_json", error))?;
    let element_json: Option<String> = row.get(10)?;
    let element = element_json
        .as_deref()
        .map(|json| {
            serde_json::from_str::<Element>(json)
                .map_err(|error| StoreError::invalid_json("element_json", error))
        })
        .transpose()?;
    let redaction_json: String = row.get(12)?;
    let redaction = serde_json::from_str::<Redaction>(&redaction_json)
        .map_err(|error| StoreError::invalid_json("redaction_json", error))?;
    let window_title: Option<String> = row.get(8)?;
    let window_id: Option<i64> = row.get(9)?;
    let window = (window_title.is_some() || window_id.is_some() || requires_window(&data))
        .then_some(Window {
            title: window_title,
            id: window_id,
        });
    let mono_ns = unsigned("mono_ns", row.get(2)?)?;

    let event = Event {
        version: crate::schema::EVENT_SCHEMA_VERSION,
        id: row.get(0)?,
        ts: row.get(1)?,
        mono_ns,
        source: row.get(3)?,
        event_type,
        app: App {
            bundle_id: row.get(5)?,
            name: row.get(6)?,
            pid: row.get(7)?,
        },
        window,
        element,
        data,
        redaction,
    };
    serde_json::to_value(&event).map_err(|error| StoreError::invalid_json("event", error))?;
    Ok(event)
}

fn requires_window(data: &EventData) -> bool {
    match data {
        EventData::WindowFocus(_)
        | EventData::WindowTitle(_)
        | EventData::UiFocus(_)
        | EventData::UiClick(_)
        | EventData::UiValue(_)
        | EventData::InputKey(_)
        | EventData::InputScroll(_)
        | EventData::BrowserNavigate(_)
        | EventData::ClipboardPaste(_) => true,
        EventData::ClipboardCopy(data) => data.origin == ClipboardOrigin::CopyShortcut,
        EventData::AppActivate(_) | EventData::AppLaunch(_) | EventData::AppTerminate(_) => false,
    }
}

fn parse_timestamp(field: &'static str, value: &str) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| StoreError::invalid_timestamp(field, value.to_owned()))
}

fn heartbeat_is_fresh(heartbeat_at: Option<&str>, now: OffsetDateTime) -> Result<bool, StoreError> {
    Ok(heartbeat_at
        .map(|value| parse_timestamp("heartbeat_at", value))
        .transpose()?
        .is_some_and(|heartbeat| {
            let age = now - heartbeat;
            age >= time::Duration::ZERO
                && age <= time::Duration::seconds(HEARTBEAT_STALE_AFTER_SECONDS)
        }))
}

fn readable_schema_version(connection: &Connection) -> Result<i64, StoreError> {
    let version = connection.query_row("SELECT schema_version FROM meta", [], |row| row.get(0))?;
    if matches!(
        version,
        LEGACY_STORE_SCHEMA_VERSION
            | DAEMON_IDENTITY_STORE_SCHEMA_VERSION
            | RETENTION_STORE_SCHEMA_VERSION
            | STORE_SCHEMA_VERSION
    ) {
        Ok(version)
    } else {
        Err(StoreError::UnsupportedSchemaVersion(version))
    }
}

fn parse_daemon_mode(mode: &str) -> Result<DaemonMode, StoreError> {
    match mode {
        "foreground" => Ok(DaemonMode::Foreground),
        "launchd" => Ok(DaemonMode::Launchd),
        _ => Err(StoreError::InvalidDaemonMode(mode.to_owned())),
    }
}

fn unsigned(field: &'static str, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::NumericOverflow(field))
}

fn positive_unsigned(field: &'static str, value: i64) -> Result<u64, StoreError> {
    let value = unsigned(field, value)?;
    if value == 0 {
        Err(StoreError::InvalidDaemonState(
            "retention_hours must be greater than zero",
        ))
    } else {
        Ok(value)
    }
}
