use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use time::OffsetDateTime;
use zanei_core::normalize::format_timestamp;
use zanei_core::schema::{
    App, ContentSnapshotData, ContentSnapshotTrigger, EmptyData, Event, EventData, Redaction,
    Window,
};
use zanei_core::store::{
    MetadataFilter, QueryFilter, StoreError, StoreReader, StoreWriter, export_plain_sqlite,
};

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);
const RETENTION_HOURS: u64 = 24 * 365 * 100;

#[test]
fn unknown_types_are_counted_without_consuming_the_known_event_limit() {
    let directory = TestDirectory::new("unknown-limit");
    let store = directory.path().join("store.sqlite");
    let now = OffsetDateTime::now_utc();
    let first = event("evt_01K00000000000000000001001", now, 1);
    let unknown = event(
        "evt_01K00000000000000000001002",
        now + time::Duration::milliseconds(1),
        2,
    );
    let last = event(
        "evt_01K00000000000000000001003",
        now + time::Duration::milliseconds(2),
        3,
    );
    StoreWriter::open(&store)
        .and_then(|mut writer| writer.append_batch(&[first.clone(), unknown.clone(), last.clone()]))
        .expect("store query fixtures");
    update_row(&store, &unknown.id, "type = 'future.event'");

    let result = StoreReader::open(&store)
        .and_then(|reader| {
            reader.query(
                &QueryFilter {
                    types: vec!["*".to_owned()],
                    limit: Some(2),
                    ..QueryFilter::default()
                },
                RETENTION_HOURS,
            )
        })
        .expect("query skips only unknown types");

    assert_eq!(result.events, [first, last]);
    assert_eq!(result.skipped_unknown_types, 1);
}

#[test]
fn malformed_known_rows_remain_typed_errors() {
    let directory = TestDirectory::new("malformed-known");
    let store = directory.path().join("store.sqlite");
    let fixture = event(
        "evt_01K00000000000000000001004",
        OffsetDateTime::now_utc(),
        1,
    );
    StoreWriter::open(&store)
        .and_then(|mut writer| writer.append(&fixture))
        .expect("store malformed fixture");
    update_row(&store, &fixture.id, "data_json = '{'");

    let error = StoreReader::open(&store)
        .and_then(|reader| {
            reader.query(
                &QueryFilter {
                    types: vec!["*".to_owned()],
                    ..QueryFilter::default()
                },
                RETENTION_HOURS,
            )
        })
        .expect_err("known malformed data must fail");

    assert!(matches!(
        error,
        StoreError::InvalidJson {
            field: "data_json",
            ..
        }
    ));
}

#[test]
fn active_and_retired_sources_share_selection_and_unknown_reporting() {
    let directory = TestDirectory::new("active-retired");
    let store = directory.path().join("store.sqlite");
    let retired = directory
        .path()
        .join("store.sqlite.plaintext-20260823T000000Z");
    let now = OffsetDateTime::now_utc();
    let active = event("evt_01K00000000000000000001005", now, 1);
    let retired_known = event(
        "evt_01K00000000000000000001006",
        now + time::Duration::milliseconds(1),
        2,
    );
    let retired_unknown = event(
        "evt_01K00000000000000000001007",
        now + time::Duration::milliseconds(2),
        3,
    );
    StoreWriter::open(&store)
        .and_then(|mut writer| writer.append(&active))
        .expect("active fixture");
    StoreWriter::open(&retired)
        .and_then(|mut writer| {
            writer.append_batch(&[retired_known.clone(), retired_unknown.clone()])
        })
        .expect("retired fixtures");
    update_row(&retired, &retired_unknown.id, "type = 'future.retired'");

    let result = StoreReader::open(&store)
        .and_then(|reader| {
            reader.query(
                &QueryFilter {
                    types: vec!["app.*".to_owned(), "future.*".to_owned()],
                    ..QueryFilter::default()
                },
                RETENTION_HOURS,
            )
        })
        .expect("merged query");

    assert_eq!(result.events, [active, retired_known]);
    assert_eq!(result.skipped_unknown_types, 1);
}

#[test]
fn content_selection_defaults_and_explicit_patterns_match_active_and_retired_sources() {
    let directory = TestDirectory::new("content-selection");
    let store = directory.path().join("store.sqlite");
    let retired = directory
        .path()
        .join("store.sqlite.plaintext-20260823T010000Z");
    let now = OffsetDateTime::now_utc();
    let active_app = event("evt_01K00000000000000000001008", now, 1);
    let active_content = content_event(
        "evt_01K00000000000000000001009",
        now + time::Duration::milliseconds(1),
        2,
    );
    let retired_app = event(
        "evt_01K00000000000000000001010",
        now + time::Duration::milliseconds(2),
        3,
    );
    let retired_content = content_event(
        "evt_01K00000000000000000001011",
        now + time::Duration::milliseconds(3),
        4,
    );
    StoreWriter::open(&store)
        .and_then(|mut writer| writer.append_batch(&[active_app, active_content]))
        .expect("active content fixtures");
    StoreWriter::open(&retired)
        .and_then(|mut writer| writer.append_batch(&[retired_app, retired_content]))
        .expect("retired content fixtures");
    let reader = StoreReader::open(&store).expect("merged reader");

    let default = reader
        .query(&QueryFilter::default(), RETENTION_HOURS)
        .expect("default query");
    assert_eq!(default.events.len(), 2);
    assert!(
        default
            .events
            .iter()
            .all(|event| event.event_type == "app.launch")
    );

    for types in [
        vec!["content.snapshot".to_owned()],
        vec!["content.*".to_owned()],
    ] {
        let explicit = reader
            .query(
                &QueryFilter {
                    types,
                    ..QueryFilter::default()
                },
                RETENTION_HOURS,
            )
            .expect("explicit content query");
        assert_eq!(explicit.events.len(), 2);
        assert!(
            explicit
                .events
                .iter()
                .all(|event| event.event_type == "content.snapshot" && event.version == 3)
        );
    }

    for types in [
        vec!["app.*".to_owned(), "content.snapshot".to_owned()],
        vec!["*".to_owned()],
    ] {
        let all = reader
            .query(
                &QueryFilter {
                    types,
                    ..QueryFilter::default()
                },
                RETENTION_HOURS,
            )
            .expect("combined content query");
        assert_eq!(all.events.len(), 4);
    }

    let metadata = reader
        .query_metadata(&MetadataFilter {
            since: None,
            until: None,
            types: vec!["content.snapshot".to_owned()],
            app: None,
            bundle_id: None,
            configured_retention_hours: RETENTION_HOURS,
        })
        .expect("content metadata across sources");
    assert_eq!(metadata.len(), 2);
    assert!(metadata.windows(2).all(|pair| pair[0].ts <= pair[1].ts));
}

#[test]
fn v2_content_snapshots_remain_lossless_across_active_retired_and_export_reads() {
    let directory = TestDirectory::new("content-v2-compatibility");
    let store = directory.path().join("store.sqlite");
    let retired = directory
        .path()
        .join("store.sqlite.plaintext-20260823T020000Z");
    let snapshot = directory.path().join("snapshot.sqlite");
    let now = OffsetDateTime::now_utc();
    let current = content_event("evt_01K00000000000000000001013", now, 1);
    let active_legacy = legacy_content_event(
        "evt_01K00000000000000000001014",
        now + time::Duration::milliseconds(1),
        2,
        true,
    );
    let retired_legacy = legacy_content_event(
        "evt_01K00000000000000000001015",
        now + time::Duration::milliseconds(2),
        3,
        false,
    );
    StoreWriter::open(&store)
        .and_then(|mut writer| writer.append_batch(&[current, active_legacy.clone()]))
        .expect("active v2/v3 fixtures");
    StoreWriter::open(&retired)
        .and_then(|mut writer| writer.append(&retired_legacy))
        .expect("retired v2 fixture");
    let retired_data_before = stored_data_json(&retired, &retired_legacy.id);

    let reader = StoreReader::open(&store).expect("merged reader");
    let result = reader
        .query(
            &QueryFilter {
                types: vec!["content.snapshot".to_owned()],
                ..QueryFilter::default()
            },
            RETENTION_HOURS,
        )
        .expect("v2/v3 merged query");
    assert_eq!(
        result
            .events
            .iter()
            .map(|event| event.version)
            .collect::<Vec<_>>(),
        [3, 2, 2]
    );
    assert_eq!(result.events[1], active_legacy);
    assert_eq!(result.events[2], retired_legacy);
    let retained_json = serde_json::to_value(&result.events[2]).expect("serialize retained v2");
    assert_eq!(retained_json["v"], 2);
    assert_eq!(retained_json["data"]["complete"], false);
    assert!(retained_json["data"].get("cutoff").is_none());

    let metadata = reader
        .query_metadata(&MetadataFilter {
            since: None,
            until: None,
            types: vec!["content.snapshot".to_owned()],
            app: None,
            bundle_id: None,
            configured_retention_hours: RETENTION_HOURS,
        })
        .expect("v2/v3 metadata query");
    assert_eq!(metadata.len(), 3);

    export_plain_sqlite(
        &store,
        None,
        &QueryFilter {
            types: vec!["content.snapshot".to_owned()],
            ..QueryFilter::default()
        },
        RETENTION_HOURS,
        &snapshot,
    )
    .expect("v2/v3 SQLite export");
    assert_eq!(
        stored_data_json(&snapshot, &retired_legacy.id),
        retired_data_before,
        "SQLite export must copy the retained v2 payload verbatim"
    );
    let exported = StoreReader::open(&snapshot)
        .and_then(|reader| {
            reader.query(
                &QueryFilter {
                    types: vec!["content.snapshot".to_owned()],
                    ..QueryFilter::default()
                },
                RETENTION_HOURS,
            )
        })
        .expect("query SQLite export");
    assert_eq!(
        exported
            .events
            .iter()
            .map(|event| event.version)
            .collect::<Vec<_>>(),
        [3, 2, 2]
    );
}

#[test]
fn metadata_query_does_not_read_payload_element_or_redaction_json() {
    let directory = TestDirectory::new("metadata-projection");
    let store = directory.path().join("store.sqlite");
    let content = content_event(
        "evt_01K00000000000000000001012",
        OffsetDateTime::now_utc(),
        1,
    );
    StoreWriter::open(&store)
        .and_then(|mut writer| writer.append(&content))
        .expect("content metadata fixture");
    update_row(
        &store,
        &content.id,
        "data_json = '{', element_json = '{', redaction_json = '{'",
    );

    let metadata = StoreReader::open(&store)
        .and_then(|reader| {
            reader.query_metadata(&MetadataFilter {
                since: None,
                until: None,
                types: vec!["content.snapshot".to_owned()],
                app: None,
                bundle_id: None,
                configured_retention_hours: RETENTION_HOURS,
            })
        })
        .expect("metadata projection must not decode excluded JSON columns");

    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].id, content.id);
}

fn update_row(store: &Path, id: &str, assignment: &str) {
    rusqlite::Connection::open(store)
        .expect("open fixture database")
        .execute(
            &format!("UPDATE events SET {assignment} WHERE id = ?1"),
            [id],
        )
        .expect("modify fixture row");
}

fn stored_data_json(store: &Path, id: &str) -> String {
    rusqlite::Connection::open(store)
        .expect("open fixture database")
        .query_row("SELECT data_json FROM events WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .expect("read fixture payload")
}

fn event(id: &str, at: OffsetDateTime, mono_ns: u64) -> Event {
    Event {
        version: 1,
        id: id.to_owned(),
        ts: format_timestamp(at),
        mono_ns,
        source: "test.store".to_owned(),
        event_type: "app.launch".to_owned(),
        app: App {
            name: "Example".to_owned(),
            bundle_id: Some("dev.example.App".to_owned()),
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

fn content_event(id: &str, at: OffsetDateTime, mono_ns: u64) -> Event {
    let text = "Visible snapshot";
    Event {
        version: 3,
        id: id.to_owned(),
        ts: format_timestamp(at),
        mono_ns,
        source: "macos.ax".to_owned(),
        event_type: "content.snapshot".to_owned(),
        app: App {
            name: "Example".to_owned(),
            bundle_id: Some("dev.example.App".to_owned()),
            pid: Some(1),
        },
        window: Some(Window {
            title: Some("Example".to_owned()),
            id: Some(1),
        }),
        element: None,
        data: EventData::ContentSnapshot(ContentSnapshotData::new(
            Some(text.to_owned()),
            text.chars().count() as u64,
            None,
            ContentSnapshotTrigger::Settle,
        )),
        redaction: Redaction {
            applied: false,
            rules: Vec::new(),
        },
    }
}

fn legacy_content_event(id: &str, at: OffsetDateTime, mono_ns: u64, complete: bool) -> Event {
    let mut value = serde_json::to_value(content_event(id, at, mono_ns))
        .expect("serialize current content fixture");
    value["v"] = serde_json::json!(2);
    value["data"] = serde_json::json!({
        "text": "Visible snapshot",
        "chars": 16,
        "complete": complete,
        "trigger": "settle"
    });
    serde_json::from_value(value).expect("deserialize retained v2 content fixture")
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "zanei-store-query-report-{label}-{}-{id}",
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
