use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use time::OffsetDateTime;
use zanei_core::normalize::format_timestamp;
use zanei_core::schema::{App, EmptyData, Event, EventData, Redaction};
use zanei_core::store::{QueryFilter, StoreError, StoreReader, StoreWriter, export_plain_sqlite};

const CURRENT_SCHEMA_VERSION: i64 = 6;
const RETENTION_HOURS: u64 = 24 * 365 * 100;
static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn readers_accept_v1_through_v6_and_writers_migrate_v1_through_v5_sequentially() {
    let directory = TestDirectory::new("migration");
    for version in 1..=5 {
        let store = directory.path().join(format!("v{version}.sqlite"));
        create_schema(&store, version);
        StoreReader::open(&store).expect("prior schema remains readable");
        StoreWriter::open(&store).expect("prior schema migrates");
        assert_eq!(schema_version(&store), CURRENT_SCHEMA_VERSION);
        assert_eq!(daemon_columns(&store), daemon_columns_for(5));
    }

    let current = directory.path().join("v6.sqlite");
    StoreWriter::open(&current).expect("create current store");
    StoreReader::open(&current).expect("current schema remains readable");
    assert_eq!(schema_version(&current), CURRENT_SCHEMA_VERSION);
}

#[test]
fn schema_v6_writer_open_is_a_no_op_and_future_versions_fail_fast() {
    let directory = TestDirectory::new("current-future");
    let current = directory.path().join("current.sqlite");
    StoreWriter::open(&current).expect("create current store");
    let before = sqlite_schema_cookie(&current);
    StoreWriter::open(&current).expect("reopen current store");
    assert_eq!(sqlite_schema_cookie(&current), before);
    assert_eq!(schema_version(&current), CURRENT_SCHEMA_VERSION);

    set_schema_version(&current, 7);
    assert!(matches!(
        StoreReader::open(&current),
        Err(StoreError::UnsupportedSchemaVersion(7))
    ));
    assert!(matches!(
        StoreWriter::open(&current),
        Err(StoreError::UnsupportedSchemaVersion(7))
    ));
}

#[test]
fn active_v6_and_retired_v5_are_read_as_one_event_stream() {
    let directory = TestDirectory::new("retired-union");
    let store = directory.path().join("store.sqlite");
    let retired = directory
        .path()
        .join("store.sqlite.plaintext-20260823T020000Z");
    StoreWriter::open(&store)
        .and_then(|mut writer| writer.append(&event("evt_01K00000000000000000002001", "Active")))
        .expect("active v6 fixture");
    StoreWriter::open(&retired)
        .and_then(|mut writer| writer.append(&event("evt_01K00000000000000000002002", "Retired")))
        .expect("retired fixture");
    set_schema_version(&retired, 5);

    let result = StoreReader::open(&store)
        .and_then(|reader| {
            reader.query(
                &QueryFilter {
                    types: vec!["*".to_owned()],
                    ..QueryFilter::default()
                },
                RETENTION_HOURS,
            )
        })
        .expect("active and retired query");

    assert_eq!(result.events.len(), 2);
    assert_eq!(schema_version(&store), 6);
    assert_eq!(schema_version(&retired), 5);
}

#[test]
fn plaintext_snapshot_destination_uses_schema_v6() {
    let directory = TestDirectory::new("snapshot");
    let store = directory.path().join("store.sqlite");
    let snapshot = directory.path().join("snapshot.sqlite");
    StoreWriter::open(&store)
        .and_then(|mut writer| writer.append(&event("evt_01K00000000000000000002003", "Active")))
        .expect("snapshot source fixture");

    export_plain_sqlite(
        &store,
        None,
        &QueryFilter {
            types: vec!["*".to_owned()],
            ..QueryFilter::default()
        },
        RETENTION_HOURS,
        &snapshot,
    )
    .expect("plaintext snapshot");

    assert_eq!(schema_version(&snapshot), CURRENT_SCHEMA_VERSION);
    StoreReader::open(&snapshot).expect("snapshot is a readable v6 store");
}

fn create_schema(path: &Path, version: i64) {
    rusqlite::Connection::open(path)
        .expect("open prior schema")
        .execute_batch(&format!(
            "CREATE TABLE events (
                id TEXT PRIMARY KEY, ts TEXT NOT NULL, mono_ns INTEGER NOT NULL,
                source TEXT NOT NULL, type TEXT NOT NULL, bundle_id TEXT, app_name TEXT,
                pid INTEGER, window_title TEXT, window_id INTEGER, element_json TEXT,
                data_json TEXT, redaction_json TEXT
            );
            CREATE TABLE daemon_state (
                id INTEGER PRIMARY KEY CHECK (id = 1), pid INTEGER, started_at TEXT,
                {identity_columns} heartbeat_at TEXT, {retention_column} paused_until TEXT,
                events_captured INTEGER NOT NULL DEFAULT 0,
                events_dropped INTEGER NOT NULL DEFAULT 0, last_event_ts TEXT,
                degraded_json TEXT {collector_column} {permissions_column}
            );
            INSERT INTO daemon_state(id) VALUES (1);
            CREATE TABLE meta(schema_version INTEGER NOT NULL);
            INSERT INTO meta(schema_version) VALUES ({version});",
            identity_columns = if version >= 2 {
                "instance_id TEXT, mode TEXT,"
            } else {
                ""
            },
            retention_column = if version >= 3 {
                "retention_hours INTEGER CHECK (retention_hours > 0),"
            } else {
                ""
            },
            collector_column = if version >= 4 {
                ", collector_failures_json TEXT NOT NULL DEFAULT '{}'"
            } else {
                ""
            },
            permissions_column = if version >= 5 {
                ", last_known_permissions_json TEXT"
            } else {
                ""
            },
        ))
        .expect("create prior schema");
}

fn daemon_columns_for(version: i64) -> Vec<String> {
    let directory = TestDirectory::new("expected-columns");
    let store = directory.path().join("expected.sqlite");
    create_schema(&store, version);
    daemon_columns(&store)
}

fn daemon_columns(path: &Path) -> Vec<String> {
    let mut columns = rusqlite::Connection::open(path)
        .expect("open schema")
        .prepare("PRAGMA table_info(daemon_state)")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()
        })
        .expect("daemon_state columns");
    columns.sort_unstable();
    columns
}

fn schema_version(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .expect("open schema")
        .query_row("SELECT schema_version FROM meta", [], |row| row.get(0))
        .expect("schema version")
}

fn set_schema_version(path: &Path, version: i64) {
    rusqlite::Connection::open(path)
        .expect("open schema")
        .execute("UPDATE meta SET schema_version = ?1", [version])
        .expect("set schema version");
}

fn sqlite_schema_cookie(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .expect("open schema")
        .query_row("PRAGMA schema_version", [], |row| row.get(0))
        .expect("SQLite schema cookie")
}

fn event(id: &str, app_name: &str) -> Event {
    Event {
        version: 1,
        id: id.to_owned(),
        ts: format_timestamp(OffsetDateTime::now_utc()),
        mono_ns: 1,
        source: "test.store".to_owned(),
        event_type: "app.launch".to_owned(),
        app: App {
            name: app_name.to_owned(),
            bundle_id: Some(format!("dev.example.{app_name}")),
            pid: Some(1),
        },
        window: None,
        element: None,
        data: EventData::AppLaunch(EmptyData::default()),
        redaction: Redaction {
            applied: false,
            rules: Vec::new(),
        },
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "zanei-store-schema-v6-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove test directory");
    }
}
