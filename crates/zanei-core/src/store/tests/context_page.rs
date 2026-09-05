use rusqlite::Connection;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use super::super::{
    ContextCursor, ContextGap, ContextGapReason, ContextPage, ContextPageError, ContextPageRequest,
    ContextPageResult, ContextText, MAX_CONTEXT_PAGE_ROWS,
};
use super::{
    PurgeFilter, QueryFilter, StoreFormat, StoreKey, StoreReader, StoreWriter, TestDatabase,
    app_launch, browser_navigate, export_plain_sqlite, running_state,
};

const NOW: &str = "2026-09-06T10:00:00.000Z";

fn now() -> OffsetDateTime {
    OffsetDateTime::parse(NOW, &Rfc3339).unwrap()
}
fn event(n: usize, ts: &str) -> crate::schema::Event {
    app_launch(&format!("evt_{n:026}"), ts, "Notes", "com.apple.Notes")
}
fn reader(db: &TestDatabase) -> StoreReader {
    StoreReader::open_known(db.path(), StoreFormat::Plaintext, None).unwrap()
}
fn request(limit: usize) -> ContextPageRequest {
    ContextPageRequest {
        limit,
        ..Default::default()
    }
}
fn read(
    reader: &StoreReader,
    request: &ContextPageRequest,
    at: OffsetDateTime,
) -> ContextPageResult {
    reader.read_context_page_at(request, 24, at).unwrap()
}
fn page(result: ContextPageResult) -> ContextPage {
    match result {
        ContextPageResult::Page(page) => page,
        other => panic!("expected Page: {other:?}"),
    }
}
fn gap(result: ContextPageResult) -> ContextGap {
    match result {
        ContextPageResult::Gap(gap) => gap,
        other => panic!("expected Gap: {other:?}"),
    }
}
fn next(page: &ContextPage, limit: usize) -> ContextPageRequest {
    ContextPageRequest {
        cursor: Some(page.next_cursor.clone()),
        upper_bound: Some(page.upper_bound.clone()),
        limit,
    }
}
fn resume(gap: &ContextGap, limit: usize) -> ContextPageRequest {
    ContextPageRequest {
        cursor: Some(gap.resume_cursor.clone()),
        upper_bound: Some(gap.upper_bound.clone()),
        limit,
    }
}

#[test]
fn same_time_pages_replay_and_late_appends_wait_for_next_upper_bound() {
    let db = TestDatabase::new("context-fixed-upper");
    let mut writer = StoreWriter::open(db.path()).unwrap();
    writer
        .append_batch(&(1..=5).rev().map(|n| event(n, NOW)).collect::<Vec<_>>())
        .unwrap();
    let reader = reader(&db);
    let first = page(read(&reader, &request(2), now()));
    assert_eq!(
        first
            .observations
            .iter()
            .map(|o| o.id.clone())
            .collect::<Vec<_>>(),
        [event(5, NOW).id, event(4, NOW).id]
    );
    let continuation = next(&first, 2);
    let second = page(read(&reader, &continuation, now()));
    writer
        .append(&event(6, "2026-09-06T09:00:00.000Z"))
        .unwrap();
    assert_eq!(page(read(&reader, &continuation, now())), second);
    let serialized = serde_json::to_string(&second.next_cursor).unwrap();
    let decoded: ContextCursor = serde_json::from_str(&serialized).unwrap();
    let third = page(read(
        &reader,
        &ContextPageRequest {
            cursor: Some(decoded),
            ..next(&second, 2)
        },
        now(),
    ));
    assert_eq!(third.observations.len(), 1);
    assert!(!third.has_more);
    let late = page(read(
        &reader,
        &ContextPageRequest {
            cursor: Some(third.next_cursor.clone()),
            ..request(2)
        },
        now(),
    ));
    assert_eq!(late.observations[0].id, event(6, NOW).id);
    assert_eq!(late.coverage.after, 5);
    assert_eq!(late.coverage.through, 6);
    // Timestamp query keeps its original semantics and sees the late event first.
    assert_eq!(
        reader.query(&QueryFilter::default(), 1000).unwrap().events[0].id,
        event(6, NOW).id
    );
}

#[test]
fn writer_accepted_long_fractional_timestamp_survives_page_and_cursor_replay() {
    let db = TestDatabase::new("context-long-timestamp");
    let mut writer = StoreWriter::open(db.path()).unwrap();
    let timestamp = format!("2026-09-06T10:00:00.{}Z", "0".repeat(100));
    let original = event(1, &timestamp);
    writer.append(&original).unwrap();
    writer.append(&event(2, NOW)).unwrap();
    let reader = reader(&db);
    let first = page(read(&reader, &request(1), now()));
    assert_eq!(first.observations[0].ts, original.ts);
    let continuation = ContextPageRequest {
        cursor: Some(
            serde_json::from_str(&serde_json::to_string(&first.next_cursor).unwrap()).unwrap(),
        ),
        ..next(&first, 1)
    };
    let second = page(read(&reader, &continuation, now()));
    assert_eq!(second.observations[0].id, event(2, NOW).id);
    assert_eq!(page(read(&reader, &continuation, now())), second);
}

#[test]
fn middle_and_tail_deletions_have_explicit_ranges_and_resume_without_skipping() {
    let db = TestDatabase::new("context-purge");
    let mut writer = StoreWriter::open(db.path()).unwrap();
    let mut events: Vec<_> = (1..=5).map(|n| event(n, NOW)).collect();
    events[1].app.bundle_id = Some("delete.me".into());
    events[2].app.bundle_id = Some("delete.me".into());
    writer.append_batch(&events).unwrap();
    let reader = reader(&db);
    let first = page(read(&reader, &request(1), now()));
    writer
        .purge(&PurgeFilter {
            bundle_id: Some("delete.me".into()),
            ..PurgeFilter::all()
        })
        .unwrap();
    let missing = gap(read(&reader, &next(&first, 4), now()));
    assert_eq!(missing.reason, ContextGapReason::RetentionOrDeletion);
    assert_eq!(
        (missing.affected_range.after, missing.affected_range.through),
        (1, 3)
    );
    assert_eq!(gap(read(&reader, &next(&first, 4), now())), missing);
    let remaining = page(read(&reader, &resume(&missing, 4), now()));
    assert_eq!(
        remaining
            .observations
            .iter()
            .map(|o| o.append_sequence)
            .collect::<Vec<_>>(),
        [4, 5]
    );
    writer.purge(&PurgeFilter::all()).unwrap();
    // With no prior cursor the full removed history is reported, not empty success.
    let deleted = gap(read(&reader, &request(4), now()));
    assert_eq!(
        (deleted.affected_range.after, deleted.affected_range.through),
        (0, 5)
    );
    let empty = page(read(&reader, &resume(&deleted, 4), now()));
    assert!(empty.observations.is_empty());
    writer.append(&event(6, NOW)).unwrap();
    let appended = page(read(
        &reader,
        &ContextPageRequest {
            cursor: Some(empty.next_cursor),
            ..request(4)
        },
        now(),
    ));
    assert_eq!(appended.observations[0].append_sequence, 6);
}

#[test]
fn expiry_on_replay_is_a_gap_but_expired_consumed_anchor_does_not_hide_unread_rows() {
    let db = TestDatabase::new("context-expiry");
    let mut writer = StoreWriter::open(db.path()).unwrap();
    writer
        .append_batch(&[event(1, "2026-09-05T10:00:00.000Z"), event(2, NOW)])
        .unwrap();
    let reader = reader(&db);
    let original = page(read(&reader, &request(2), now()));
    assert_eq!(original.observations.len(), 2); // cutoff equality is retained.
    let later = now() + Duration::seconds(1);
    let replay = ContextPageRequest {
        upper_bound: Some(original.upper_bound),
        ..request(2)
    };
    let expired = gap(read(&reader, &replay, later));
    assert_eq!(
        (expired.affected_range.after, expired.affected_range.through),
        (0, 1)
    );
    assert_eq!(
        page(read(&reader, &resume(&expired, 2), later)).observations[0].append_sequence,
        2
    );
    let first = page(read(&reader, &request(1), now()));
    writer.purge_retention(later, 24).unwrap();
    let second = page(read(&reader, &next(&first, 1), later));
    assert_eq!(second.observations[0].append_sequence, 2);
}

#[test]
fn bounded_scans_split_fresh_and_expired_groups_using_the_writer_timestamp_parser() {
    let db = TestDatabase::new("context-retention-groups");
    let mut writer = StoreWriter::open(db.path()).unwrap();
    let expired = "2026-09-05t09:00:00z";
    let long_fraction = format!("2026-09-06T10:00:00.{}Z", "0".repeat(100));
    writer
        .append_batch(&[
            event(1, "2026-09-06t10:00:00z"),
            event(2, expired),
            event(3, expired),
            event(4, expired),
            event(5, "2026-09-06T12:00:00+02:00"),
            event(6, &long_fraction),
            event(7, expired),
        ])
        .unwrap();
    let reader = reader(&db);
    let first = page(read(&reader, &request(2), now()));
    assert_eq!(first.observations[0].id, event(1, NOW).id);
    assert_eq!((first.coverage.after, first.coverage.through), (0, 1));
    // Three expired rows require two bounded calls; neither skips fresh row 5.
    let expired_a = gap(read(&reader, &next(&first, 2), now()));
    assert_eq!(
        (
            expired_a.affected_range.after,
            expired_a.affected_range.through
        ),
        (1, 3)
    );
    let expired_b = gap(read(&reader, &resume(&expired_a, 2), now()));
    assert_eq!(
        (
            expired_b.affected_range.after,
            expired_b.affected_range.through
        ),
        (3, 4)
    );
    let fresh = page(read(&reader, &resume(&expired_b, 2), now()));
    assert_eq!(
        fresh
            .observations
            .iter()
            .map(|o| o.append_sequence)
            .collect::<Vec<_>>(),
        [5, 6]
    );
    assert_eq!((fresh.coverage.after, fresh.coverage.through), (4, 6));
    let tail = gap(read(&reader, &next(&fresh, 2), now()));
    assert_eq!(
        (tail.affected_range.after, tail.affected_range.through),
        (6, 7)
    );
    let completed = page(read(&reader, &resume(&tail, 2), now()));
    assert!(completed.observations.is_empty());
    assert!(!completed.has_more);
}

#[test]
fn active_daemon_retention_is_applied_in_the_read_snapshot() {
    let db = TestDatabase::new("context-daemon-retention");
    let mut writer = StoreWriter::open(db.path()).unwrap();
    writer
        .append(&event(1, "2026-09-06T10:00:00.000+02:00"))
        .unwrap();
    writer.write_daemon_state(&running_state(now(), 1)).unwrap();
    assert_eq!(
        gap(read(&reader(&db), &request(2), now()))
            .affected_range
            .through,
        1
    );
}

#[test]
fn huge_metadata_is_explicitly_omitted_and_body_is_never_decoded() {
    let db = TestDatabase::new("context-reference");
    let mut writer = StoreWriter::open(db.path()).unwrap();
    let mut event = browser_navigate("evt_00000000000000000000000001", NOW);
    event.window.as_mut().unwrap().title = Some("界".repeat(1_000_000));
    event.app.name = "a".repeat(2048);
    writer.append(&event).unwrap();
    // A corrupt/unknown raw body must not enter the reference-only decoder.
    Connection::open(db.path())
        .unwrap()
        .execute(
            "UPDATE events SET data_json=?1, type='future.unknown'",
            ["!".repeat(4_000_000)],
        )
        .unwrap();
    let result = page(read(&reader(&db), &request(2), now()));
    let observation = &result.observations[0];
    assert_eq!(observation.id, event.id);
    assert_eq!(
        observation.window_title,
        ContextText::Omitted {
            utf8_bytes: 3_000_000
        }
    );
    assert_eq!(
        observation.app_name,
        ContextText::Omitted { utf8_bytes: 2048 }
    );
    assert_eq!(
        observation.bundle_id,
        ContextText::Value(event.app.bundle_id.unwrap())
    );
    assert_eq!(
        observation.event_type,
        ContextText::Value("future.unknown".into())
    );
    assert_eq!(observation.source, ContextText::Value(event.source));
    assert!(format!("{result:?}").len() < 4096);
}

#[test]
fn exported_identity_and_observable_restore_regressions_require_explicit_restart() {
    let db = TestDatabase::new("context-restore");
    let export = TestDatabase::new("context-export");
    let mut writer = StoreWriter::open(db.path()).unwrap();
    writer
        .append_batch(&[event(1, NOW), event(2, NOW)])
        .unwrap();
    let reader = reader(&db);
    let original = page(read(&reader, &request(1), now()));
    export_plain_sqlite(
        db.path(),
        None,
        &QueryFilter::default(),
        1000,
        export.path(),
    )
    .unwrap();
    StoreWriter::open(export.path())
        .unwrap()
        .append(&event(5, NOW))
        .unwrap();
    let exported_reader = StoreReader::open(export.path()).unwrap();
    let cursor_only = gap(read(
        &exported_reader,
        &ContextPageRequest {
            cursor: Some(original.next_cursor.clone()),
            ..request(1)
        },
        now(),
    ));
    // The affected range belongs to the old store, never the new store's head.
    assert_eq!(
        (
            cursor_only.affected_range.after,
            cursor_only.affected_range.through
        ),
        (1, 2)
    );
    let changed = gap(read(&exported_reader, &next(&original, 1), now()));
    assert_eq!(changed.reason, ContextGapReason::StoreChanged);
    assert_eq!(
        page(read(&exported_reader, &resume(&changed, 2), now()))
            .observations
            .len(),
        2
    );
    // Simulate a restored earlier snapshot with the same identity, retaining the
    // cursor anchor but lowering the durable high water.
    let raw = Connection::open(db.path()).unwrap();
    raw.execute_batch("DELETE FROM events WHERE append_sequence=2; UPDATE sqlite_sequence SET seq=1 WHERE name='events';").unwrap();
    let rollback = gap(read(
        &reader,
        &ContextPageRequest {
            cursor: Some(original.next_cursor.clone()),
            ..request(2)
        },
        now(),
    ));
    assert_eq!(rollback.reason, ContextGapReason::ContinuityUnknown);
    assert!(rollback.affected_range.through >= rollback.affected_range.after);
    // Restore followed by enough new appends can regain the old high water;
    // changing a surviving anchor still exposes that rewrite.
    raw.execute(
        "UPDATE events SET id=?1 WHERE append_sequence=1",
        [event(3, NOW).id],
    )
    .unwrap();
    writer.append(&event(4, NOW)).unwrap();
    assert_eq!(
        gap(read(&reader, &next(&original, 1), now())).reason,
        ContextGapReason::ContinuityUnknown
    );
}

#[test]
fn invalid_selectors_fail_and_old_schema_is_incompatible_without_mutation() {
    let db = TestDatabase::new("context-invalid");
    let mut writer = StoreWriter::open(db.path()).unwrap();
    writer.append(&event(1, NOW)).unwrap();
    let reader = reader(&db);
    for limit in [0, MAX_CONTEXT_PAGE_ROWS + 1, usize::MAX] {
        assert!(matches!(
            reader.read_context_page_at(&request(limit), 24, now()),
            Err(ContextPageError::InvalidRequest(_))
        ));
    }
    let original = page(read(&reader, &request(1), now()));
    let mut value = serde_json::to_value(&original.next_cursor).unwrap();
    value["sequence"] = serde_json::json!(u64::MAX);
    let bad = ContextPageRequest {
        cursor: Some(serde_json::from_value(value).unwrap()),
        ..request(1)
    };
    assert!(matches!(
        reader.read_context_page_at(&bad, 24, now()),
        Err(ContextPageError::InvalidRequest(_))
    ));
    let raw = Connection::open(db.path()).unwrap();
    raw.execute_batch("UPDATE meta SET schema_version=7;")
        .unwrap();
    assert_eq!(
        read(&reader, &request(1), now()),
        ContextPageResult::Incompatible { version: 7 }
    );
    assert_eq!(
        raw.query_row("SELECT schema_version FROM meta", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        7
    );
}

#[test]
fn encrypted_store_empty_success_and_independent_writer_appends() {
    let db = TestDatabase::new("context-encrypted");
    let key = StoreKey::generate().unwrap();
    let mut writer = StoreWriter::open_with_key(db.path(), Some(&key)).unwrap();
    let reader = StoreReader::open_known(db.path(), StoreFormat::Encrypted, Some(&key)).unwrap();
    let empty = page(read(&reader, &request(1), now()));
    assert_eq!(
        (empty.coverage.after, empty.coverage.through, empty.has_more),
        (0, 0, false)
    );
    writer.append(&event(1, NOW)).unwrap();
    let first = page(read(
        &reader,
        &ContextPageRequest {
            cursor: Some(empty.next_cursor),
            ..request(1)
        },
        now(),
    ));
    let mut second_writer =
        StoreWriter::open_known(db.path(), StoreFormat::Encrypted, Some(&key)).unwrap();
    second_writer.append(&event(2, NOW)).unwrap();
    assert!(
        page(read(&reader, &next(&first, 1), now()))
            .observations
            .is_empty()
    );
    assert_eq!(
        page(read(
            &reader,
            &ContextPageRequest {
                cursor: Some(first.next_cursor),
                ..request(1)
            },
            now()
        ))
        .observations[0]
            .append_sequence,
        2
    );
}
