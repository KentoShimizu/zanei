use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use time::OffsetDateTime;
use zanei_core::normalize::format_timestamp;
use zanei_core::schema::{App, EmptyData, Event, EventData, Redaction};
use zanei_core::store::{QueryFilter, StoreError, StoreReader, StoreWriter};

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

fn update_row(store: &Path, id: &str, assignment: &str) {
    rusqlite::Connection::open(store)
        .expect("open fixture database")
        .execute(
            &format!("UPDATE events SET {assignment} WHERE id = ?1"),
            [id],
        )
        .expect("modify fixture row");
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
