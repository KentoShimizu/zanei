use rusqlite::{Connection, params};

use super::{
    DaemonState, PurgeFilter, QueryFilter, StoreError, StoreFormat, StoreKey, StoreReader,
    StoreWriter, TEST_RETENTION_HOURS, TestDatabase, app_launch, browser_navigate,
    export_plain_sqlite,
};
use crate::schema::Event;

const NOW: &str = "2026-09-06T10:00:00Z";

fn event(id: &str, ts: &str) -> Event {
    app_launch(id, ts, "Notes", "com.apple.Notes")
}

fn open_reader(database: &TestDatabase, key: Option<&StoreKey>) -> StoreReader {
    let format = if key.is_some() {
        StoreFormat::Encrypted
    } else {
        StoreFormat::Plaintext
    };
    StoreReader::open_known(database.path(), format, key).expect("open without re-probing locks")
}

fn rows(reader: &StoreReader) -> Vec<(u64, String)> {
    reader
        .connection
        .prepare("SELECT append_sequence, id FROM events ORDER BY append_sequence")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect()
        })
        .expect("read explicit sequence")
}

#[test]
fn committed_order_survives_clock_reversal_duplicates_and_purge() {
    let database = TestDatabase::new("append-order");
    let key = StoreKey::generate().expect("fixture key");
    let mut writer = StoreWriter::open_with_key(database.path(), Some(&key)).expect("writer");
    let reader = open_reader(&database, Some(&key));
    let empty = reader.append_head().expect("empty head");
    assert_eq!(empty.sequence, 0);
    let first = event("evt_01K00000000000000000000003", NOW);
    let same_time = event("evt_01K00000000000000000000002", NOW);
    let older = event("evt_01K00000000000000000000001", "2026-09-05T10:00:00Z");
    writer
        .append_batch(&[first.clone(), same_time.clone()])
        .expect("same-time batch");
    writer
        .persist(std::slice::from_ref(&older), Some(&DaemonState::default()))
        .expect("late append");
    assert_eq!(
        rows(&reader),
        vec![(1, first.id.clone()), (2, same_time.id), (3, older.id)]
    );
    let committed = reader.append_head().expect("committed head");
    assert_eq!(committed.store_identity, empty.store_identity);
    assert_eq!(committed.sequence, 3);
    // A new row precedes the duplicate: both its insertion and its allocated
    // sequence must roll back, along with the daemon snapshot and counters.
    let attempted = event("evt_01K00000000000000000000004", NOW);
    for persist in [false, true] {
        let batch = [attempted.clone(), first.clone()];
        let result = if persist {
            writer.persist(
                &batch,
                Some(&DaemonState {
                    events_dropped: 99,
                    ..Default::default()
                }),
            )
        } else {
            writer.append_batch(&batch)
        };
        assert!(matches!(result, Err(StoreError::Database(_))));
        assert_eq!(
            reader.append_head().expect("head after rollback"),
            committed
        );
        assert_eq!(rows(&reader).len(), 3);
        let status = reader.status().expect("status after rollback");
        assert_eq!((status.events_captured, status.events_dropped), (3, 0));
    }
    writer.purge(&PurgeFilter::all()).expect("purge all");
    assert!(rows(&reader).is_empty());
    assert_eq!(reader.append_head().expect("head after purge"), committed);
    drop(writer);
    let mut writer = StoreWriter::open_known(database.path(), StoreFormat::Encrypted, Some(&key))
        .expect("reopen writer");
    writer.append(&first).expect("append after deletion");
    assert_eq!(rows(&reader), vec![(4, first.id)]);
    assert_eq!(
        reader.append_head().expect("reopened head").store_identity,
        empty.store_identity
    );
}

#[test]
fn failure_after_event_insert_rolls_back_sequence_and_progress() {
    let database = TestDatabase::new("append-fault");
    let mut writer = StoreWriter::open(database.path()).expect("writer");
    let reader = open_reader(&database, None);
    let connection = Connection::open(database.path()).expect("fault fixture");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_progress BEFORE UPDATE OF events_captured ON daemon_state \
         BEGIN SELECT RAISE(ABORT, 'injected progress write failure'); END;",
        )
        .expect("inject failure after insertion");
    assert!(
        writer
            .append(&event("evt_01K00000000000000000000005", NOW))
            .is_err()
    );
    assert!(rows(&reader).is_empty());
    assert_eq!(reader.append_head().expect("rolled-back head").sequence, 0);
    assert_eq!(
        reader
            .status()
            .expect("rolled-back progress")
            .events_captured,
        0
    );
    connection
        .execute_batch("DROP TRIGGER fail_progress;")
        .expect("remove injected fault");
    writer
        .append(&event("evt_01K00000000000000000000006", NOW))
        .expect("commit");
    drop(writer);
    drop(reader);
    drop(connection);
    let reopened = open_reader(&database, None);
    assert_eq!(reopened.append_head().expect("durable commit").sequence, 1);
    assert_eq!(rows(&reopened).len(), 1);
    assert_eq!(
        reopened.status().expect("durable progress").events_captured,
        1
    );
}

#[test]
fn concurrent_writer_batches_have_disjoint_committed_sequences() {
    let database = TestDatabase::new("append-concurrent");
    StoreWriter::open(database.path()).expect("create store");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|batch| {
            let barrier = barrier.clone();
            let path = database.path().to_owned();
            std::thread::spawn(move || {
                let mut writer =
                    StoreWriter::open_known(path, StoreFormat::Plaintext, None).expect("writer");
                barrier.wait();
                writer
                    .append_batch(&[
                        event(&format!("evt_01K000000000000000000000{}1", batch), NOW),
                        event(&format!("evt_01K000000000000000000000{}2", batch), NOW),
                    ])
                    .expect("commit batch");
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("writer thread");
    }
    let reader = open_reader(&database, None);
    let rows = rows(&reader);
    assert_eq!(
        rows.iter().map(|row| row.0).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(
        &rows[0].1[..rows[0].1.len() - 1],
        &rows[1].1[..rows[1].1.len() - 1]
    );
    assert_eq!(
        &rows[2].1[..rows[2].1.len() - 1],
        &rows[3].1[..rows[3].1.len() - 1]
    );
    assert_eq!(reader.append_head().expect("committed head").sequence, 4);
}

#[test]
fn sequence_exhaustion_fails_instead_of_reusing_committed_positions() {
    let database = TestDatabase::new("append-overflow");
    let mut writer = StoreWriter::open(database.path()).expect("writer");
    writer
        .append(&event("evt_01K00000000000000000000007", NOW))
        .expect("first");
    Connection::open(database.path())
        .expect("overflow fixture")
        .execute(
            "UPDATE sqlite_sequence SET seq = ?1 WHERE name = 'events'",
            [i64::MAX],
        )
        .expect("set last representable sequence");
    assert!(
        writer
            .append(&event("evt_01K00000000000000000000008", NOW))
            .is_err()
    );
    let reader = open_reader(&database, None);
    assert_eq!(
        reader.append_head().expect("maximum head").sequence,
        i64::MAX as u64
    );
    assert_eq!(rows(&reader).len(), 1);
    assert_eq!(reader.status().expect("progress").events_captured, 1);
}

#[test]
fn encrypted_v7_migration_preserves_events_state_and_indexes_atomically() {
    let database = TestDatabase::new("append-v7");
    let key = StoreKey::generate().expect("key");
    let connection = legacy_store(&database, &key);
    let original = browser_navigate("evt_01K00000000000000000000009", NOW);
    insert_legacy(&connection, &original);
    connection.execute_batch("UPDATE daemon_state SET events_captured = 7, events_dropped = 2, paused_until = 'infinity';").expect("state");
    drop(connection);
    let before = open_reader(&database, Some(&key));
    assert!(matches!(
        before.append_head(),
        Err(StoreError::UnsupportedSchemaVersion(7))
    ));
    let before_status = before.status().expect("legacy status");
    let before_events = before
        .query(&QueryFilter::default(), TEST_RETENTION_HOURS)
        .expect("legacy query")
        .events;
    assert_eq!(before_events, [original]);
    drop(before);
    let mut writer = StoreWriter::open_with_key(database.path(), Some(&key)).expect("migrate");
    let reader = open_reader(&database, Some(&key));
    assert_eq!(reader.status().expect("migrated status"), before_status);
    assert_eq!(
        reader
            .query(&QueryFilter::default(), TEST_RETENTION_HOURS)
            .expect("migrated query")
            .events,
        before_events
    );
    let baseline = reader.append_head().expect("baseline");
    assert_eq!(baseline.sequence, 1);
    assert_eq!(reader.connection.query_row("SELECT count(*) FROM sqlite_schema WHERE type = 'index' AND tbl_name = 'events' AND name LIKE 'idx_events_%'", [], |row| row.get::<_, i64>(0)).expect("indexes"), 3);
    assert_eq!(
        reader
            .connection
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .expect("integrity"),
        "ok"
    );
    assert_eq!(
        reader
            .connection
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("foreign keys"),
        0
    );
    writer
        .append(&event(
            "evt_01K00000000000000000000010",
            "2026-09-01T00:00:00Z",
        ))
        .expect("late new event");
    assert_eq!(reader.append_head().expect("new head").sequence, 2);
    drop(writer);
    let writer = StoreWriter::open_known(database.path(), StoreFormat::Encrypted, Some(&key))
        .expect("reopen");
    assert_eq!(
        reader
            .append_head()
            .expect("stable identity")
            .store_identity,
        baseline.store_identity
    );
    drop(writer);
}

#[test]
fn failed_migration_leaves_original_encrypted_store_without_partial_identity() {
    let database = TestDatabase::new("append-migration-fault");
    let key = StoreKey::generate().expect("key");
    let connection = legacy_store(&database, &key);
    insert_legacy(&connection, &event("evt_01K00000000000000000000011", NOW));
    connection.execute_batch("CREATE TRIGGER fail_migration BEFORE UPDATE ON meta BEGIN SELECT RAISE(ABORT, 'injected schema commit failure'); END;").expect("fault at migration end");
    drop(connection);
    assert!(StoreWriter::open_with_key(database.path(), Some(&key)).is_err());
    let reader = open_reader(&database, Some(&key));
    assert_eq!(reader.schema_version, 7);
    assert_eq!(
        reader
            .query(&QueryFilter::default(), TEST_RETENTION_HOURS)
            .expect("original event")
            .events
            .len(),
        1
    );
    assert_eq!(reader.connection.query_row("SELECT count(*) FROM sqlite_schema WHERE name IN ('store_identity', 'events_before_append_sequence', 'sqlite_sequence')", [], |row| row.get::<_, i64>(0)).expect("no partial schema"), 0);
    drop(reader);
    let connection = Connection::open(database.path()).expect("remove injected fault");
    super::super::apply_key(&connection, &key).expect("key");
    connection
        .execute_batch("DROP TRIGGER fail_migration;")
        .expect("drop fault");
    drop(connection);
    StoreWriter::open_with_key(database.path(), Some(&key)).expect("retry migration");
    assert_eq!(
        open_reader(&database, Some(&key))
            .append_head()
            .expect("new baseline")
            .sequence,
        1
    );
}

#[test]
fn snapshots_and_recreated_stores_have_independent_append_identities() {
    let database = TestDatabase::new("append-export-source");
    let snapshot = TestDatabase::new("append-export");
    let retired = TestDatabase {
        path: super::super::sibling(database.path(), ".plaintext-20260906T100000Z"),
    };
    let key = StoreKey::generate().expect("key");
    let mut writer = StoreWriter::open_with_key(database.path(), Some(&key)).expect("writer");
    writer
        .append_batch(&[
            event("evt_01K00000000000000000000012", NOW),
            event("evt_01K00000000000000000000013", NOW),
        ])
        .expect("batch");
    let original = open_reader(&database, Some(&key))
        .append_head()
        .expect("source head");
    drop(writer);
    StoreWriter::open(retired.path())
        .and_then(|mut writer| {
            writer.append_batch(&[
                event("evt_01K00000000000000000000012", NOW),
                event("evt_01K00000000000000000000014", NOW),
            ])
        })
        .expect("overlapping retired source");
    export_plain_sqlite(
        database.path(),
        Some(&key),
        &QueryFilter::default(),
        TEST_RETENTION_HOURS,
        snapshot.path(),
    )
    .expect("export");
    let exported = open_reader(&snapshot, None)
        .append_head()
        .expect("snapshot head");
    assert_ne!(original.store_identity, exported.store_identity);
    assert_eq!(exported.sequence, 3);
    assert_eq!(
        rows(&open_reader(&snapshot, None))
            .into_iter()
            .map(|row| row.0)
            .collect::<Vec<_>>(),
        [1, 2, 3],
    );
    std::fs::remove_file(database.path()).expect("recreate isolated fixture");
    StoreWriter::open_with_key(database.path(), Some(&key)).expect("new store");
    let recreated = open_reader(&database, Some(&key))
        .append_head()
        .expect("new identity");
    assert_ne!(original.store_identity, recreated.store_identity);
    assert_eq!(recreated.sequence, 0);
}

#[test]
fn export_rejects_a_null_legacy_id_instead_of_silently_skipping_it() {
    let database = TestDatabase::new("append-export-invalid");
    let snapshot = TestDatabase::new("append-export-invalid-output");
    let key = StoreKey::generate().expect("key");
    let connection = legacy_store(&database, &key);
    connection.execute(
        "INSERT INTO events(id, ts, mono_ns, source, type) VALUES(NULL, ?1, 1, 'test', 'app.launch')",
        [NOW],
    ).expect("legacy TEXT PRIMARY KEY permits NULL");
    drop(connection);
    let result = export_plain_sqlite(
        database.path(),
        Some(&key),
        &QueryFilter::default(),
        TEST_RETENTION_HOURS,
        snapshot.path(),
    );
    assert!(
        matches!(result, Err(StoreError::Database(rusqlite::Error::SqliteFailure(error, _)))
        if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL)
    );
}

fn legacy_store(database: &TestDatabase, key: &StoreKey) -> Connection {
    let connection = Connection::open(database.path()).expect("legacy fixture");
    super::super::apply_key(&connection, key).expect("legacy encryption");
    connection.execute_batch(V7_SCHEMA).expect("legacy v7 DDL");
    connection
}

fn insert_legacy(connection: &Connection, event: &Event) {
    connection.execute(
        "INSERT INTO events(id, ts, mono_ns, source, type, bundle_id, app_name, pid, window_title, window_id, element_json, data_json, redaction_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![event.id, event.ts, event.mono_ns, event.source, event.event_type, event.app.bundle_id, event.app.name, event.app.pid, event.window.as_ref().and_then(|w| w.title.as_ref()), event.window.as_ref().and_then(|w| w.id), event.element.as_ref().map(serde_json::to_string).transpose().expect("element"), serde_json::to_string(&event.data).expect("data"), serde_json::to_string(&event.redaction).expect("redaction")],
    ).expect("legacy event");
}

// Frozen pre-sequence schema: the migration fixture must not inherit new DDL.
const V7_SCHEMA: &str = "
CREATE TABLE events (
    id TEXT PRIMARY KEY, ts TEXT NOT NULL, mono_ns INTEGER NOT NULL,
    source TEXT NOT NULL, type TEXT NOT NULL, bundle_id TEXT, app_name TEXT,
    pid INTEGER, window_title TEXT, window_id INTEGER, element_json TEXT,
    data_json TEXT, redaction_json TEXT
);
CREATE INDEX idx_events_ts ON events(ts);
CREATE INDEX idx_events_type_ts ON events(type, ts);
CREATE INDEX idx_events_bundle_ts ON events(bundle_id, ts);
CREATE TABLE daemon_state (
    id INTEGER PRIMARY KEY CHECK (id = 1), pid INTEGER, started_at TEXT,
    instance_id TEXT, mode TEXT, heartbeat_at TEXT,
    retention_hours INTEGER CHECK (retention_hours > 0), paused_until TEXT,
    events_captured INTEGER NOT NULL DEFAULT 0,
    events_dropped INTEGER NOT NULL DEFAULT 0, last_event_ts TEXT, degraded_json TEXT,
    collector_failures_json TEXT NOT NULL DEFAULT '{}', last_known_capabilities_json TEXT
);
INSERT INTO daemon_state(id) VALUES (1);
CREATE TABLE daemon_capabilities (id INTEGER PRIMARY KEY CHECK (id = 1), snapshot_json TEXT NOT NULL);
CREATE TABLE meta (schema_version INTEGER NOT NULL);
INSERT INTO meta(schema_version) VALUES (7);
";
