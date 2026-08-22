use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::schema::{
    App, BrowserMode, BrowserNavigateData, ClipboardCopyData, ClipboardOrigin, ContentKind,
    EmptyData, Event, EventData, Redaction, Window,
};

use super::{
    DaemonMode, DaemonPermissions, DaemonState, LockedReason, MigrationOutcome, PermissionState,
    QueryFilter, StoreError, StoreFailureKind, StoreFormat, StoreKey, StoreReader, StoreStatus,
    StoreWriter, export_plain_sqlite, migrate_to_encrypted,
};

static NEXT_DATABASE_ID: AtomicU64 = AtomicU64::new(0);
const TEST_RETENTION_HOURS: u64 = 24 * 365 * 100;

#[test]
fn writes_reads_filters_and_rejects_unknown_types() {
    let database = TestDatabase::new("query");
    let mut writer = StoreWriter::open(database.path()).expect("open writer");
    let first = app_launch(
        "evt_01K00000000000000000000001",
        "2026-08-16T09:00:00.000Z",
        "Safari",
        "com.apple.Safari",
    );
    let browser = browser_navigate("evt_01K00000000000000000000003", "2026-08-16T09:01:00.000Z");
    writer
        .append_batch(&[first.clone(), browser.clone()])
        .expect("append events");

    let reader = StoreReader::open(database.path()).expect("open reader");
    let all = reader
        .query(&QueryFilter::default(), TEST_RETENTION_HOURS)
        .expect("query all");
    assert_eq!(all, vec![first.clone(), browser.clone()]);

    let filtered = reader
        .query(
            &QueryFilter {
                since: Some("2026-08-16T18:01:00+09:00".to_owned()),
                until: Some("2026-08-16T09:01:00Z".to_owned()),
                types: vec!["browser.*".to_owned()],
                app: Some("Google Chrome".to_owned()),
                bundle_id: Some("com.google.Chrome".to_owned()),
                limit: Some(1),
            },
            TEST_RETENTION_HOURS,
        )
        .expect("query filtered");
    assert_eq!(filtered, vec![browser]);

    let invalid_pattern = reader.query(
        &QueryFilter {
            types: vec!["browser.*.navigate".to_owned()],
            ..QueryFilter::default()
        },
        TEST_RETENTION_HOURS,
    );
    assert!(invalid_pattern.is_err());

    rusqlite::Connection::open(database.path())
        .expect("open store for corruption fixture")
        .execute(
            "UPDATE events SET type = 'future.event', data_json = '{}' WHERE id = ?1",
            [&first.id],
        )
        .expect("write unknown type fixture");
    let error = reader
        .query(&QueryFilter::default(), TEST_RETENTION_HOURS)
        .expect_err("unknown v1 type must fail closed");
    assert!(error.to_string().contains("unknown event type"));
}

#[test]
fn truncated_marker_round_trips_through_the_store() {
    let database = TestDatabase::new("truncated-round-trip");
    let mut event = app_launch(
        "evt_01K00000000000000000000004",
        "2026-08-16T09:02:00.000Z",
        "Finder",
        "com.apple.finder",
    );
    event.mark_truncated();
    StoreWriter::open(database.path())
        .and_then(|mut writer| writer.append_batch(&[event.clone()]))
        .expect("store truncated event");

    let stored = StoreReader::open(database.path())
        .and_then(|reader| reader.query(&QueryFilter::default(), TEST_RETENTION_HOURS))
        .expect("query truncated event");

    assert_eq!(stored, [event]);
    assert!(stored[0].is_truncated());
    assert_eq!(
        serde_json::to_value(&stored[0]).expect("serialize stored event")["truncated"],
        true
    );
}

#[test]
fn query_excludes_events_older_than_retention_cutoff() {
    let database = TestDatabase::new("query-retention");
    let now = OffsetDateTime::now_utc();
    let expired = app_launch(
        "evt_01K00000000000000000000001",
        &crate::normalize::format_timestamp(now - time::Duration::hours(2)),
        "Expired",
        "dev.example.Expired",
    );
    let retained = app_launch(
        "evt_01K00000000000000000000002",
        &crate::normalize::format_timestamp(now - time::Duration::minutes(30)),
        "Retained",
        "dev.example.Retained",
    );
    StoreWriter::open(database.path())
        .and_then(|mut writer| writer.append_batch(&[expired, retained.clone()]))
        .expect("store retention fixtures");

    let events = StoreReader::open(database.path())
        .and_then(|reader| reader.query(&QueryFilter::default(), 1))
        .expect("query retained events");

    assert_eq!(events, [retained]);
}

#[test]
fn query_prefers_fresh_daemon_retention_and_falls_back_for_untrusted_heartbeats() {
    let database = TestDatabase::new("active-retention");
    let now = OffsetDateTime::now_utc();
    let older = app_launch(
        "evt_01K00000000000000000000005",
        &crate::normalize::format_timestamp(now - time::Duration::hours(2)),
        "Older",
        "dev.example.Older",
    );
    let recent = app_launch(
        "evt_01K00000000000000000000006",
        &crate::normalize::format_timestamp(now - time::Duration::minutes(30)),
        "Recent",
        "dev.example.Recent",
    );
    let mut writer = StoreWriter::open(database.path()).expect("open writer");
    writer
        .append_batch(&[older.clone(), recent.clone()])
        .expect("store retention fixtures");
    writer
        .write_daemon_state(&running_state(now, 3))
        .expect("write fresh retention");
    let reader = StoreReader::open(database.path()).expect("open reader");

    assert_eq!(
        reader
            .query(&QueryFilter::default(), 1)
            .expect("query fresh daemon retention"),
        [older, recent.clone()]
    );

    writer
        .write_daemon_state(&running_state(
            now - time::Duration::seconds(super::HEARTBEAT_STALE_AFTER_SECONDS + 1),
            3,
        ))
        .expect("write stale retention");
    assert_eq!(
        reader
            .query(&QueryFilter::default(), 1)
            .expect("query configured retention after stale heartbeat"),
        std::slice::from_ref(&recent)
    );

    writer
        .write_daemon_state(&running_state(now + time::Duration::seconds(30), 3))
        .expect("write future retention");
    assert_eq!(
        reader
            .query(&QueryFilter::default(), 1)
            .expect("query configured retention after future heartbeat"),
        std::slice::from_ref(&recent)
    );

    writer
        .write_daemon_state(&DaemonState::default())
        .expect("clear heartbeat");
    assert_eq!(
        reader
            .query(&QueryFilter::default(), 1)
            .expect("query configured retention without heartbeat"),
        [recent]
    );
}

#[test]
fn query_retention_resolution_ignores_unrelated_corrupt_status_fields() {
    let database = TestDatabase::new("retention-status-isolation");
    let now = OffsetDateTime::now_utc();
    let event = app_launch(
        "evt_01K00000000000000000000007",
        &crate::normalize::format_timestamp(now - time::Duration::minutes(30)),
        "Retained",
        "dev.example.Retained",
    );
    let mut writer = StoreWriter::open(database.path()).expect("open writer");
    writer.append(&event).expect("store event");
    writer
        .write_daemon_state(&running_state(now, 1))
        .expect("write daemon state");
    drop(writer);
    let connection = rusqlite::Connection::open(database.path()).expect("open corruption fixture");
    connection
        .execute(
            "UPDATE daemon_state SET degraded_json = '{' WHERE id = 1",
            [],
        )
        .expect("corrupt degraded JSON");
    connection
        .execute(
            "UPDATE daemon_permissions SET snapshot_json = '{' WHERE id = 1",
            [],
        )
        .expect("corrupt permissions JSON");
    drop(connection);

    let events = StoreReader::open(database.path())
        .and_then(|reader| reader.query(&QueryFilter::default(), 48))
        .expect("query must depend only on heartbeat and retention status fields");
    assert_eq!(events, [event]);
}

#[test]
fn required_empty_window_presence_round_trips_through_nullable_store_columns() {
    let database = TestDatabase::new("required-empty-window");
    let mut browser =
        browser_navigate("evt_01K00000000000000000000008", "2026-08-16T09:01:00.000Z");
    browser.window = Some(Window {
        title: None,
        id: None,
    });
    let copy =
        clipboard_copy_shortcut("evt_01K00000000000000000000009", "2026-08-16T09:02:00.000Z");
    StoreWriter::open(database.path())
        .and_then(|mut writer| writer.append_batch(&[browser.clone(), copy.clone()]))
        .expect("store required empty windows");

    let stored = StoreReader::open(database.path())
        .and_then(|reader| reader.query(&QueryFilter::default(), TEST_RETENTION_HOURS))
        .expect("read required empty windows");
    assert_eq!(stored, [browser, copy]);
    assert!(stored.iter().all(|event| event.window.is_some()));
}

#[test]
fn purge_uses_an_exclusive_cutoff_and_retention_window() {
    let database = TestDatabase::new("purge");
    let mut writer = StoreWriter::open(database.path()).expect("open writer");
    writer
        .append_batch(&[
            app_launch(
                "evt_01K00000000000000000000001",
                "2026-08-16T08:00:00.000Z",
                "Safari",
                "com.apple.Safari",
            ),
            app_launch(
                "evt_01K00000000000000000000002",
                "2026-08-16T09:00:00.000Z",
                "Safari",
                "com.apple.Safari",
            ),
            app_launch(
                "evt_01K00000000000000000000003",
                "2026-08-16T10:00:00.000Z",
                "Safari",
                "com.apple.Safari",
            ),
        ])
        .expect("append events");

    assert_eq!(
        writer
            .purge_before("2026-08-16T09:00:00Z")
            .expect("purge before"),
        1
    );
    let now = timestamp("2026-08-16T11:00:00Z");
    assert!(writer.purge_retention(now, u64::MAX).is_err());
    assert_eq!(writer.purge_retention(now, 1).expect("purge retention"), 1);

    let reader = StoreReader::open(database.path()).expect("open reader");
    let remaining = reader
        .query(&QueryFilter::default(), TEST_RETENTION_HOURS)
        .expect("query events");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].ts, "2026-08-16T10:00:00.000Z");
}

#[test]
fn purge_all_removes_every_event_and_oldest_timestamp_becomes_empty() {
    let database = TestDatabase::new("purge-all");
    let mut writer = StoreWriter::open(database.path()).expect("open writer");
    writer
        .append_batch(&[
            app_launch(
                "evt_01K00000000000000000000001",
                "2026-08-16T08:00:00.000Z",
                "Safari",
                "com.apple.Safari",
            ),
            app_launch(
                "evt_01K00000000000000000000002",
                "2026-08-16T09:00:00.000Z",
                "Finder",
                "com.apple.finder",
            ),
        ])
        .expect("append events");
    let reader = StoreReader::open(database.path()).expect("open reader");
    assert_eq!(
        reader.oldest_event_ts().expect("read oldest timestamp"),
        Some("2026-08-16T08:00:00.000Z".to_owned())
    );

    assert_eq!(writer.purge_all().expect("purge every event"), 2);
    assert_eq!(reader.oldest_event_ts().expect("read empty store"), None);
    assert_eq!(writer.purge_all().expect("purge empty store"), 0);
}

#[test]
fn status_derives_running_paused_and_counters_from_persisted_state() {
    let database = TestDatabase::new("status");
    let writer = StoreWriter::open(database.path()).expect("open writer");
    let state = DaemonState {
        pid: Some(42),
        started_at: Some("2026-08-16T08:00:00Z".to_owned()),
        instance_id: Some("42@2026-08-16T08:00:00Z".to_owned()),
        mode: Some(DaemonMode::Foreground),
        heartbeat_at: Some("2026-08-16T09:59:45Z".to_owned()),
        retention_hours: Some(72),
        paused_until: Some("infinity".to_owned()),
        events_captured: 12,
        events_dropped: 3,
        last_event_ts: Some("2026-08-16T09:59:44Z".to_owned()),
        degraded: BTreeMap::from([("chrome".to_owned(), "permission denied".to_owned())]),
        collector_failures: BTreeMap::from([("ax".to_owned(), 2), ("eventtap".to_owned(), 5)]),
        permissions: Some(DaemonPermissions {
            permissions_ok: false,
            accessibility: PermissionState::Granted,
            input_monitoring: PermissionState::Denied,
            automation: BTreeMap::new(),
        }),
    };
    writer.write_daemon_state(&state).expect("write state");

    let reader = StoreReader::open(database.path()).expect("open reader");
    let fresh = reader
        .status_at(timestamp("2026-08-16T10:00:00Z"))
        .expect("fresh status");
    assert!(fresh.running);
    assert!(fresh.paused);
    assert_eq!(fresh.events_captured, 12);
    assert_eq!(fresh.events_dropped, 3);
    assert_eq!(fresh.degraded, state.degraded);
    assert_eq!(fresh.collector_failures, state.collector_failures);
    assert_eq!(fresh.reported_permissions(), state.permissions.as_ref());
    assert_eq!(
        fresh.last_reported_permissions(),
        state.permissions.as_ref()
    );
    assert_eq!(fresh.effective_retention_hours(48), 72);

    let stale = reader
        .status_at(timestamp("2026-08-16T10:00:00.001Z"))
        .expect("stale status");
    assert!(!stale.running);
    assert!(!stale.paused);
    assert!(stale.degraded.is_empty());
    assert_eq!(stale.collector_failures, state.collector_failures);
    assert_eq!(stale.reported_permissions(), None);
    assert_eq!(
        stale.last_reported_permissions(),
        state.permissions.as_ref()
    );
    assert_eq!(stale.effective_retention_hours(48), 48);

    let future = reader
        .status_at(timestamp("2026-08-16T09:59:44Z"))
        .expect("future status");
    assert!(!future.running);
    assert!(future.degraded.is_empty());
    assert_eq!(future.collector_failures, state.collector_failures);
    assert_eq!(future.effective_retention_hours(48), 48);
    assert_eq!(StoreStatus::default().effective_retention_hours(48), 48);

    let mut resumed = state;
    resumed.heartbeat_at = Some("2026-08-16T10:00:00Z".to_owned());
    resumed.paused_until = None;
    writer
        .write_daemon_state(&resumed)
        .expect("write resumed heartbeat");
    writer.set_paused_until(None).expect("clear pause");
    writer
        .increment_events_dropped(2)
        .expect("increment dropped");
    let updated = reader
        .status_at(timestamp("2026-08-16T10:00:15Z"))
        .expect("updated status");
    assert!(updated.running);
    assert!(!updated.paused);
    assert_eq!(updated.events_dropped, 5);
}

#[test]
fn stopped_state_retains_last_known_permissions_without_reporting_them_as_current() {
    let database = TestDatabase::new("stopped-last-known-permissions");
    let writer = StoreWriter::open(database.path()).expect("open writer");
    let permissions = permission_snapshot(true);
    let mut running = running_state(OffsetDateTime::now_utc(), 48);
    running.permissions = Some(permissions.clone());
    writer
        .write_daemon_state(&running)
        .expect("write recorder permission report");

    writer
        .write_daemon_state(&DaemonState::default())
        .expect("write stopped state");

    let status = StoreReader::open(database.path())
        .and_then(|reader| reader.status())
        .expect("read stopped status");
    assert!(!status.running);
    assert_eq!(status.heartbeat_at, None);
    assert_eq!(status.instance_id, None);
    assert_eq!(status.permissions, None);
    assert_eq!(status.reported_permissions(), None);
    assert_eq!(status.last_reported_permissions(), Some(&permissions));
}

#[test]
fn new_heartbeat_does_not_expose_last_known_permissions_as_current() {
    let database = TestDatabase::new("new-heartbeat-last-known-permissions");
    let writer = StoreWriter::open(database.path()).expect("open writer");
    let now = OffsetDateTime::now_utc();
    let permissions = permission_snapshot(true);
    let mut previous = running_state(now, 48);
    previous.permissions = Some(permissions.clone());
    writer
        .write_daemon_state(&previous)
        .expect("write previous recorder report");

    let mut current = running_state(now, 48);
    current.pid = Some(43);
    current.started_at = Some("2026-08-17T11:00:00Z".to_owned());
    current.instance_id = Some("43@2026-08-17T11:00:00Z".to_owned());
    writer
        .write_daemon_state(&current)
        .expect("write new recorder heartbeat");

    let status = StoreReader::open(database.path())
        .and_then(|reader| reader.status_at(now + time::Duration::seconds(1)))
        .expect("read new recorder status");
    assert!(status.running);
    assert_eq!(status.reported_permissions(), None);
    assert_eq!(status.last_reported_permissions(), Some(&permissions));
}

#[test]
fn status_rejects_a_corrupt_last_event_timestamp() {
    let database = TestDatabase::new("corrupt-last-event-timestamp");
    let now = OffsetDateTime::now_utc();
    StoreWriter::open(database.path())
        .and_then(|writer| writer.write_daemon_state(&running_state(now, 48)))
        .expect("write valid daemon state");
    rusqlite::Connection::open(database.path())
        .expect("open store for corruption fixture")
        .execute(
            "UPDATE daemon_state SET last_event_ts = 'invalid' WHERE id = 1",
            [],
        )
        .expect("corrupt last event timestamp");

    let error = StoreReader::open(database.path())
        .and_then(|reader| reader.status())
        .expect_err("corrupt last event timestamp must fail closed");

    assert!(matches!(
        error,
        StoreError::InvalidTimestamp {
            field: "last_event_ts",
            ref value,
        } if value == "invalid"
    ));
    assert_eq!(error.failure_kind(), StoreFailureKind::Corrupt);
}

#[test]
fn recovery_persists_retained_events_and_daemon_snapshot_atomically() {
    let database = TestDatabase::new("recovery");
    let mut writer = StoreWriter::open(database.path()).expect("open writer");
    let event = app_launch(
        "evt_01K00000000000000000000009",
        "2026-08-16T09:59:59.000Z",
        "Finder",
        "com.apple.finder",
    );
    let state = DaemonState {
        pid: Some(42),
        started_at: Some("2026-08-16T08:00:00Z".to_owned()),
        instance_id: Some("42@2026-08-16T08:00:00Z".to_owned()),
        mode: Some(DaemonMode::Launchd),
        heartbeat_at: Some("2026-08-16T10:00:00Z".to_owned()),
        retention_hours: Some(48),
        events_captured: 999,
        events_dropped: 7,
        last_event_ts: Some("2026-08-16T08:00:00Z".to_owned()),
        degraded: BTreeMap::from([("collector".to_owned(), "restarting".to_owned())]),
        ..DaemonState::default()
    };

    assert_eq!(writer.persist(&[event], Some(&state)).expect("recover"), 1);

    let status = StoreReader::open(database.path())
        .and_then(|reader| reader.status_at(timestamp("2026-08-16T10:00:01Z")))
        .expect("recovered status");
    assert_eq!(status.events_captured, 1);
    assert_eq!(status.events_dropped, 7);
    assert_eq!(
        status.last_event_ts.as_deref(),
        Some("2026-08-16T09:59:59.000Z")
    );
    assert_eq!(status.instance_id, state.instance_id);
    assert_eq!(status.mode, state.mode);
    assert_eq!(status.degraded, state.degraded);
}

#[test]
fn recovery_rolls_back_events_when_the_transaction_fails() {
    let database = TestDatabase::new("recovery-rollback");
    let mut writer = StoreWriter::open(database.path()).expect("open writer");
    let event = app_launch(
        "evt_01K00000000000000000000010",
        "2026-08-16T09:59:59.000Z",
        "Finder",
        "com.apple.finder",
    );
    let state = DaemonState {
        pid: Some(42),
        started_at: Some("2026-08-16T08:00:00Z".to_owned()),
        instance_id: Some("42@2026-08-16T08:00:00Z".to_owned()),
        mode: Some(DaemonMode::Foreground),
        heartbeat_at: Some("2026-08-16T10:00:00Z".to_owned()),
        retention_hours: Some(48),
        ..DaemonState::default()
    };

    assert!(
        writer
            .persist(&[event.clone(), event], Some(&state))
            .is_err()
    );

    let status = StoreReader::open(database.path())
        .and_then(|reader| reader.status())
        .expect("rolled back status");
    assert_eq!(status.events_captured, 0);
    assert_eq!(status.heartbeat_at, None);
}

#[test]
fn status_reads_legacy_store_without_daemon_permission_table() {
    let database = TestDatabase::new("legacy-status");
    let writer = StoreWriter::open(database.path()).expect("open writer");
    writer
        .write_daemon_state(&DaemonState {
            pid: Some(42),
            started_at: Some("2026-08-16T08:00:00Z".to_owned()),
            instance_id: Some("42@2026-08-16T08:00:00Z".to_owned()),
            mode: Some(DaemonMode::Foreground),
            heartbeat_at: Some("2026-08-16T10:00:00Z".to_owned()),
            retention_hours: Some(48),
            ..DaemonState::default()
        })
        .expect("write heartbeat");
    drop(writer);
    rusqlite::Connection::open(database.path())
        .expect("open legacy store")
        .execute("DROP TABLE daemon_permissions", [])
        .expect("remove post-v1 permission table");

    let status = StoreReader::open(database.path())
        .and_then(|reader| reader.status_at(timestamp("2026-08-16T10:00:01Z")))
        .expect("read legacy status");

    assert!(status.running);
    assert_eq!(status.reported_permissions(), None);
}

#[test]
fn readers_handle_retention_and_failure_metrics_from_prior_schemas() {
    for version in [
        super::LEGACY_STORE_SCHEMA_VERSION,
        super::DAEMON_IDENTITY_STORE_SCHEMA_VERSION,
        super::RETENTION_STORE_SCHEMA_VERSION,
    ] {
        let database = TestDatabase::new(&format!("v{version}-reader"));
        let connection = rusqlite::Connection::open(database.path()).expect("open legacy store");
        connection
            .execute_batch(&legacy_daemon_schema(version))
            .expect("create legacy schema");
        connection
            .execute(legacy_running_state_sql(version), [])
            .expect("write legacy heartbeat");
        drop(connection);

        let status = StoreReader::open(database.path())
            .and_then(|reader| reader.status_at(timestamp("2026-08-16T10:00:01Z")))
            .expect("read legacy status");
        assert!(status.running);
        if version >= super::RETENTION_STORE_SCHEMA_VERSION {
            assert_eq!(status.retention_hours, Some(48));
            assert_eq!(status.effective_retention_hours(24), 48);
        } else {
            assert_eq!(status.retention_hours, None);
            assert_eq!(status.effective_retention_hours(24), 24);
        }
        assert!(status.collector_failures.is_empty());
    }
}

#[test]
fn writer_migrates_prior_daemon_state_schemas_to_v5() {
    for version in [
        super::LEGACY_STORE_SCHEMA_VERSION,
        super::DAEMON_IDENTITY_STORE_SCHEMA_VERSION,
        super::RETENTION_STORE_SCHEMA_VERSION,
        super::COLLECTOR_FAILURES_STORE_SCHEMA_VERSION,
    ] {
        let database = TestDatabase::new(&format!("v{version}-migration"));
        let connection = rusqlite::Connection::open(database.path()).expect("open legacy store");
        connection
            .execute_batch(&legacy_daemon_schema(version))
            .expect("create legacy schema");
        drop(connection);

        StoreWriter::open(database.path()).expect("migrate writer");

        let connection =
            rusqlite::Connection::open(database.path()).expect("inspect migrated store");
        let migrated_version: i64 = connection
            .query_row("SELECT schema_version FROM meta", [], |row| row.get(0))
            .expect("schema version");
        let columns = connection
            .prepare("PRAGMA table_info(daemon_state)")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .expect("daemon columns");
        assert_eq!(migrated_version, super::STORE_SCHEMA_VERSION);
        assert!(columns.iter().any(|column| column == "instance_id"));
        assert!(columns.iter().any(|column| column == "mode"));
        assert!(columns.iter().any(|column| column == "retention_hours"));
        assert!(
            columns
                .iter()
                .any(|column| column == "collector_failures_json")
        );
        assert!(
            columns
                .iter()
                .any(|column| column == "last_known_permissions_json")
        );
    }
}

#[test]
fn v4_migration_copies_the_existing_permission_snapshot_to_last_known() {
    let database = TestDatabase::new("v4-permissions-migration");
    let permissions = permission_snapshot(false);
    let permissions_json = serde_json::to_string(&permissions).expect("serialize permissions");
    let connection = rusqlite::Connection::open(database.path()).expect("open v4 store");
    connection
        .execute_batch(&legacy_daemon_schema(
            super::COLLECTOR_FAILURES_STORE_SCHEMA_VERSION,
        ))
        .expect("create v4 schema");
    connection
        .execute_batch(
            "CREATE TABLE daemon_permissions (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                snapshot_json TEXT NOT NULL
            );",
        )
        .expect("create v4 permission table");
    connection
        .execute(
            "INSERT INTO daemon_permissions(id, snapshot_json) VALUES (1, ?1)",
            [&permissions_json],
        )
        .expect("write v4 permission snapshot");
    drop(connection);

    StoreWriter::open(database.path()).expect("migrate v4 writer");

    let status = StoreReader::open(database.path())
        .and_then(|reader| reader.status())
        .expect("read migrated status");
    assert_eq!(status.reported_permissions(), None);
    assert_eq!(status.last_reported_permissions(), Some(&permissions));
}

fn app_launch(id: &str, ts: &str, app_name: &str, bundle_id: &str) -> Event {
    Event {
        version: crate::schema::EVENT_SCHEMA_VERSION,
        id: id.to_owned(),
        ts: ts.to_owned(),
        mono_ns: 1,
        source: "macos.workspace".to_owned(),
        event_type: "app.launch".to_owned(),
        app: App {
            name: app_name.to_owned(),
            bundle_id: Some(bundle_id.to_owned()),
            pid: Some(10),
        },
        window: None,
        element: None,
        data: EventData::AppLaunch(EmptyData::default()),
        redaction: no_redaction(),
    }
}

fn browser_navigate(id: &str, ts: &str) -> Event {
    Event {
        version: crate::schema::EVENT_SCHEMA_VERSION,
        id: id.to_owned(),
        ts: ts.to_owned(),
        mono_ns: 3,
        source: "macos.applescript".to_owned(),
        event_type: "browser.navigate".to_owned(),
        app: App {
            name: "Google Chrome".to_owned(),
            bundle_id: Some("com.google.Chrome".to_owned()),
            pid: Some(20),
        },
        window: Some(Window {
            title: Some("Zanei".to_owned()),
            id: Some(30),
        }),
        element: None,
        data: EventData::BrowserNavigate(BrowserNavigateData {
            url: "https://example.com".to_owned().into(),
            tab_title: Some("Zanei".to_owned()),
            mode: BrowserMode::Normal,
            transition: None,
        }),
        redaction: no_redaction(),
    }
}

fn clipboard_copy_shortcut(id: &str, ts: &str) -> Event {
    Event {
        version: crate::schema::EVENT_SCHEMA_VERSION,
        id: id.to_owned(),
        ts: ts.to_owned(),
        mono_ns: 4,
        source: "macos.eventtap".to_owned(),
        event_type: "clipboard.copy".to_owned(),
        app: App {
            name: "Notes".to_owned(),
            bundle_id: Some("com.apple.Notes".to_owned()),
            pid: Some(21),
        },
        window: Some(Window {
            title: None,
            id: None,
        }),
        element: None,
        data: EventData::ClipboardCopy(ClipboardCopyData {
            origin: ClipboardOrigin::CopyShortcut,
            content_kind: ContentKind::Text,
            size_bytes: None,
            text: None,
        }),
        redaction: no_redaction(),
    }
}

fn running_state(heartbeat_at: OffsetDateTime, retention_hours: u64) -> DaemonState {
    let started_at = "2026-08-16T08:00:00Z";
    DaemonState {
        pid: Some(42),
        started_at: Some(started_at.to_owned()),
        instance_id: Some(format!("42@{started_at}")),
        mode: Some(DaemonMode::Foreground),
        heartbeat_at: Some(crate::normalize::format_timestamp(heartbeat_at)),
        retention_hours: Some(retention_hours),
        paused_until: None,
        events_captured: 0,
        events_dropped: 0,
        last_event_ts: None,
        degraded: BTreeMap::new(),
        collector_failures: BTreeMap::new(),
        permissions: None,
    }
}

fn permission_snapshot(permissions_ok: bool) -> DaemonPermissions {
    DaemonPermissions {
        permissions_ok,
        accessibility: PermissionState::Granted,
        input_monitoring: if permissions_ok {
            PermissionState::Granted
        } else {
            PermissionState::Denied
        },
        automation: BTreeMap::new(),
    }
}

fn legacy_daemon_schema(version: i64) -> String {
    let identity_columns = if version >= super::DAEMON_IDENTITY_STORE_SCHEMA_VERSION {
        "instance_id TEXT, mode TEXT,"
    } else {
        ""
    };
    let retention_column = if version >= super::RETENTION_STORE_SCHEMA_VERSION {
        "retention_hours INTEGER CHECK (retention_hours > 0),"
    } else {
        ""
    };
    let collector_failures_column = if version >= super::COLLECTOR_FAILURES_STORE_SCHEMA_VERSION {
        ", collector_failures_json TEXT NOT NULL DEFAULT '{}'"
    } else {
        ""
    };
    format!(
        "CREATE TABLE daemon_state (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            pid INTEGER,
            started_at TEXT,
            {identity_columns}
            heartbeat_at TEXT,
            {retention_column}
            paused_until TEXT,
            events_captured INTEGER NOT NULL DEFAULT 0,
            events_dropped INTEGER NOT NULL DEFAULT 0,
            last_event_ts TEXT,
            degraded_json TEXT
            {collector_failures_column}
        );
        INSERT INTO daemon_state(id) VALUES (1);
        CREATE TABLE meta(schema_version INTEGER NOT NULL);
        INSERT INTO meta(schema_version) VALUES ({version});"
    )
}

fn legacy_running_state_sql(version: i64) -> &'static str {
    if version >= super::RETENTION_STORE_SCHEMA_VERSION {
        "UPDATE daemon_state SET pid = 42, started_at = '2026-08-16T08:00:00Z',
         instance_id = '42@2026-08-16T08:00:00Z', mode = 'foreground',
         heartbeat_at = '2026-08-16T10:00:00Z', retention_hours = 48 WHERE id = 1"
    } else if version >= super::DAEMON_IDENTITY_STORE_SCHEMA_VERSION {
        "UPDATE daemon_state SET pid = 42, started_at = '2026-08-16T08:00:00Z',
         instance_id = '42@2026-08-16T08:00:00Z', mode = 'foreground',
         heartbeat_at = '2026-08-16T10:00:00Z' WHERE id = 1"
    } else {
        "UPDATE daemon_state SET pid = 42, started_at = '2026-08-16T08:00:00Z',
         heartbeat_at = '2026-08-16T10:00:00Z' WHERE id = 1"
    }
}

fn no_redaction() -> Redaction {
    Redaction {
        applied: false,
        rules: Vec::new(),
    }
}

fn timestamp(value: &str) -> OffsetDateTime {
    OffsetDateTime::parse(value, &Rfc3339).expect("valid test timestamp")
}

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(label: &str) -> Self {
        let id = NEXT_DATABASE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "zanei-store-{label}-{}-{id}.sqlite",
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            PathBuf::from(format!("{}-wal", self.path.display())),
            PathBuf::from(format!("{}-shm", self.path.display())),
        ] {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[test]
fn encrypted_store_round_trips_through_keyed_reader_and_writer() {
    let database = TestDatabase::new("encrypted");
    let key = StoreKey::generate().expect("generate key");
    let first = app_launch(
        "evt_01K00000000000000000000101",
        "2026-08-16T09:00:00.000Z",
        "Safari",
        "com.apple.Safari",
    );
    {
        let mut writer = StoreWriter::open_with_key(database.path(), Some(&key))
            .expect("create encrypted store");
        assert_eq!(writer.format(), StoreFormat::Encrypted);
        writer.append(&first).expect("append event");
    }
    assert_eq!(
        StoreFormat::probe(database.path()).expect("probe"),
        StoreFormat::Encrypted
    );
    let header = std::fs::read(database.path()).expect("read store")[..16].to_vec();
    assert_ne!(header.as_slice(), b"SQLite format 3\0");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(database.path())
            .expect("store metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    let reader =
        StoreReader::open_with_key(database.path(), Some(&key)).expect("open encrypted reader");
    assert_eq!(reader.format(), StoreFormat::Encrypted);
    assert_eq!(
        reader
            .query(&QueryFilter::default(), TEST_RETENTION_HOURS)
            .expect("query encrypted store"),
        vec![first.clone()]
    );

    let Err(locked) = StoreReader::open(database.path()) else {
        panic!("reader without key must fail");
    };
    assert!(matches!(
        locked,
        StoreError::Locked(LockedReason::KeyMissing)
    ));
    assert_eq!(locked.failure_kind(), StoreFailureKind::Locked);
    let other = StoreKey::generate().expect("generate other key");
    let Err(mismatch) = StoreReader::open_with_key(database.path(), Some(&other)) else {
        panic!("wrong key must fail");
    };
    assert!(matches!(
        mismatch,
        StoreError::Locked(LockedReason::KeyMismatch)
    ));
    assert_eq!(mismatch.failure_kind(), StoreFailureKind::Locked);
    let Err(writer_locked) = StoreWriter::open(database.path()) else {
        panic!("writer without key must fail");
    };
    assert!(matches!(
        writer_locked,
        StoreError::Locked(LockedReason::KeyMissing)
    ));

    let mut writer =
        StoreWriter::open_with_key(database.path(), Some(&key)).expect("reopen encrypted writer");
    assert_eq!(writer.format(), StoreFormat::Encrypted);
    writer
        .append(&browser_navigate(
            "evt_01K00000000000000000000102",
            "2026-08-16T09:01:00.000Z",
        ))
        .expect("append second event");
    assert_eq!(
        reader
            .query(&QueryFilter::default(), TEST_RETENTION_HOURS)
            .expect("query after second write")
            .len(),
        2
    );
}

#[test]
fn plaintext_store_ignores_a_supplied_key() {
    let database = TestDatabase::new("plaintext-key");
    StoreWriter::open(database.path()).expect("plaintext store");
    let key = StoreKey::generate().expect("generate key");

    let reader = StoreReader::open_with_key(database.path(), Some(&key))
        .expect("plaintext store opens with a key on hand");
    assert_eq!(reader.format(), StoreFormat::Plaintext);
    let writer = StoreWriter::open_with_key(database.path(), Some(&key))
        .expect("plaintext writer with a key on hand");
    assert_eq!(writer.format(), StoreFormat::Plaintext);
    assert_eq!(
        StoreFormat::probe(database.path()).expect("probe"),
        StoreFormat::Plaintext
    );
}

#[test]
fn format_probe_distinguishes_missing_plaintext_and_encrypted() {
    let database = TestDatabase::new("probe");
    assert_eq!(
        StoreFormat::probe(database.path()).expect("probe missing"),
        StoreFormat::Missing
    );
    std::fs::write(database.path(), b"").expect("write empty file");
    assert_eq!(
        StoreFormat::probe(database.path()).expect("probe empty"),
        StoreFormat::Missing
    );
    std::fs::write(database.path(), b"definitely not a database").expect("write garbage");
    // A foreign header that is not a whole number of pages is damage, not ciphertext.
    assert_eq!(
        StoreFormat::probe(database.path()).expect("probe garbage"),
        StoreFormat::Unrecognized
    );
    assert_eq!(
        StoreReader::open(database.path())
            .err()
            .expect("garbage store must not open")
            .failure_kind(),
        StoreFailureKind::Corrupt
    );
    std::fs::write(database.path(), vec![0xA5_u8; 8192]).expect("write page-aligned noise");
    assert_eq!(
        StoreFormat::probe(database.path()).expect("probe page-aligned noise"),
        StoreFormat::Encrypted
    );
    std::fs::remove_file(database.path()).expect("remove garbage");
    StoreWriter::open(database.path()).expect("plaintext store");
    assert_eq!(
        StoreFormat::probe(database.path()).expect("probe plaintext"),
        StoreFormat::Plaintext
    );
}

#[test]
fn migration_encrypts_a_plaintext_store_in_place_and_is_idempotent() {
    let database = TestDatabase::new("migrate");
    let first = app_launch(
        "evt_01K00000000000000000000201",
        "2026-08-16T09:00:00.000Z",
        "Safari",
        "com.apple.Safari",
    );
    let second = browser_navigate("evt_01K00000000000000000000202", "2026-08-16T09:01:00.000Z");
    let heartbeat = OffsetDateTime::parse("2026-08-16T09:02:00Z", &Rfc3339).expect("heartbeat");
    {
        let mut writer = StoreWriter::open(database.path()).expect("plaintext store");
        writer
            .append_batch(&[first.clone(), second.clone()])
            .expect("append events");
        writer
            .write_daemon_state(&running_state(heartbeat, 48))
            .expect("write daemon state");
    }
    let key = StoreKey::generate().expect("generate key");

    let outcome = migrate_to_encrypted(database.path(), &key).expect("migrate");
    assert_eq!(outcome, MigrationOutcome::Migrated { events: 2 });
    assert_eq!(
        StoreFormat::probe(database.path()).expect("probe migrated"),
        StoreFormat::Encrypted
    );
    assert!(!PathBuf::from(format!("{}.migrating", database.path().display())).exists());

    let reader =
        StoreReader::open_with_key(database.path(), Some(&key)).expect("open migrated store");
    assert_eq!(
        reader
            .query(&QueryFilter::default(), TEST_RETENTION_HOURS)
            .expect("query migrated store"),
        vec![first, second]
    );
    let status = reader
        .status_at(heartbeat)
        .expect("status of migrated store");
    assert!(status.running);
    assert_eq!(status.pid, Some(42));
    assert_eq!(status.retention_hours, Some(48));

    assert_eq!(
        migrate_to_encrypted(database.path(), &key).expect("second migration"),
        MigrationOutcome::Unchanged
    );
    let mut writer =
        StoreWriter::open_with_key(database.path(), Some(&key)).expect("writer after migration");
    writer
        .append(&app_launch(
            "evt_01K00000000000000000000203",
            "2026-08-16T09:03:00.000Z",
            "Finder",
            "com.apple.finder",
        ))
        .expect("append after migration");
    assert_eq!(
        reader
            .query(&QueryFilter::default(), TEST_RETENTION_HOURS)
            .expect("query after post-migration write")
            .len(),
        3
    );

    let missing = TestDatabase::new("migrate-missing");
    assert_eq!(
        migrate_to_encrypted(missing.path(), &key).expect("migrate missing store"),
        MigrationOutcome::Unchanged
    );
    assert!(!missing.path().exists());
}

#[test]
fn plaintext_snapshot_copies_the_requested_range_into_a_regular_sqlite_file() {
    let database = TestDatabase::new("snapshot-source");
    let snapshot = TestDatabase::new("snapshot-output");
    let key = StoreKey::generate().expect("generate key");
    let events = [
        app_launch(
            "evt_01K00000000000000000000301",
            "2026-08-16T09:00:00.000Z",
            "Safari",
            "com.apple.Safari",
        ),
        browser_navigate("evt_01K00000000000000000000302", "2026-08-16T09:01:00.000Z"),
        app_launch(
            "evt_01K00000000000000000000303",
            "2026-08-16T09:02:00.000Z",
            "Finder",
            "com.apple.finder",
        ),
    ];
    {
        let mut writer =
            StoreWriter::open_with_key(database.path(), Some(&key)).expect("encrypted store");
        writer.append_batch(&events).expect("append events");
    }

    let report = export_plain_sqlite(
        database.path(),
        Some(&key),
        &QueryFilter {
            since: Some("2026-08-16T09:00:30Z".to_owned()),
            until: Some("2026-08-16T09:01:30Z".to_owned()),
            ..QueryFilter::default()
        },
        TEST_RETENTION_HOURS,
        snapshot.path(),
    )
    .expect("export snapshot");
    assert_eq!(report.events, 1);
    assert_eq!(
        StoreFormat::probe(snapshot.path()).expect("probe snapshot"),
        StoreFormat::Plaintext
    );
    assert_eq!(
        StoreFormat::probe(database.path()).expect("probe source"),
        StoreFormat::Encrypted
    );

    let plain = rusqlite::Connection::open(snapshot.path()).expect("open snapshot without a key");
    let count: i64 = plain
        .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
        .expect("count snapshot events");
    assert_eq!(count, 1);
    let version: i64 = plain
        .query_row("SELECT schema_version FROM meta", [], |row| row.get(0))
        .expect("snapshot schema version");
    assert_eq!(version, super::STORE_SCHEMA_VERSION);
    drop(plain);

    let reader = StoreReader::open(snapshot.path()).expect("reader opens the snapshot");
    assert_eq!(reader.format(), StoreFormat::Plaintext);
    assert_eq!(
        reader
            .query(&QueryFilter::default(), TEST_RETENTION_HOURS)
            .expect("query snapshot"),
        vec![events[1].clone()]
    );

    let plain_source = TestDatabase::new("snapshot-plain-source");
    let plain_snapshot = TestDatabase::new("snapshot-plain-output");
    {
        let mut writer = StoreWriter::open(plain_source.path()).expect("plaintext store");
        writer.append_batch(&events).expect("append events");
    }
    let report = export_plain_sqlite(
        plain_source.path(),
        None,
        &QueryFilter::default(),
        TEST_RETENTION_HOURS,
        plain_snapshot.path(),
    )
    .expect("export plaintext snapshot");
    assert_eq!(report.events, 3);
}
