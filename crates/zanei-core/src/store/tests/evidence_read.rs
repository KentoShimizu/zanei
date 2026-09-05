use super::super::{
    ContextPageError, EvidenceContent, EvidenceField, EvidenceOrigin, EvidenceRequest,
    EvidenceResult, SelectedEvidence, StoreFailureKind,
};
use super::{StoreFormat, StoreReader, StoreWriter, TestDatabase, clipboard_copy_shortcut};
use crate::schema::{
    ContentSnapshotCutoff, ContentSnapshotData, ContentSnapshotTrigger, EventData,
};
use rusqlite::Connection;
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

const NOW: &str = "2026-09-06T10:00:00Z";
fn now() -> OffsetDateTime {
    OffsetDateTime::parse(NOW, &Rfc3339).unwrap()
}
fn fixture(text: Option<&str>) -> (TestDatabase, StoreReader, EvidenceRequest) {
    let db = TestDatabase::new("evidence");
    let mut writer = StoreWriter::open(db.path()).unwrap();
    let mut event = clipboard_copy_shortcut("evt_00000000000000000000000001", NOW);
    if let EventData::ClipboardCopy(data) = &mut event.data {
        data.text = text.map(str::to_owned);
    }
    writer.append(&event).unwrap();
    let reader = StoreReader::open_known(db.path(), StoreFormat::Plaintext, None).unwrap();
    let request = EvidenceRequest {
        origin: EvidenceOrigin {
            store_identity: reader.append_head().unwrap().store_identity,
            append_sequence: 1,
            event_id: event.id,
            observed_at: NOW.into(),
            field: EvidenceField::Text,
        },
        start: 0,
        end: None,
        max_bytes: 4,
    };
    (db, reader, request)
}
fn evidence(result: EvidenceResult) -> SelectedEvidence {
    match result {
        EvidenceResult::Evidence(value) => *value,
        other => panic!("expected evidence: {other:?}"),
    }
}
#[test]
fn unicode_nul_and_escaping_round_trip_in_bounded_chunks() {
    let original = "A😀\0あ\"\\Z";
    let (_db, reader, mut request) = fixture(Some(original));
    let mut collected = String::new();
    loop {
        let value = evidence(reader.read_evidence_at(&request, 48, now()).unwrap());
        if let EvidenceContent::Text {
            text,
            start,
            end,
            total_bytes,
            remaining,
        } = value.content
        {
            assert_eq!(start, request.start);
            assert!(text.len() <= 4);
            assert_eq!(total_bytes, original.len() as u64);
            assert!(end > start);
            collected.push_str(&text);
            match remaining {
                Some((start, end)) => {
                    request.start = start;
                    request.end = Some(end);
                }
                None => break,
            }
        } else {
            panic!("missing text");
        }
    }
    assert_eq!(collected, original);
}
#[test]
fn giant_field_does_not_materialize_into_returned_evidence_or_details() {
    let original = "abcdef".repeat(400_000);
    let (_db, reader, mut request) = fixture(Some(&original));
    request.start = 2_000_000;
    request.end = Some(2_000_020);
    request.max_bytes = 8;
    let value = evidence(reader.read_evidence_at(&request, 48, now()).unwrap());
    assert!(
        matches!(value.content, EvidenceContent::Text { ref text,total_bytes:2_400_000,remaining:Some((2_000_008,2_000_020)),.. } if text.len()==8)
    );
    assert!(
        matches!(value.details.payload,EventData::ClipboardCopy(ref data) if data.text.is_none())
    );
}
#[test]
fn absent_and_empty_are_distinct_and_original_fields_are_selectable() {
    let (_db, reader, request) = fixture(None);
    assert_eq!(
        evidence(reader.read_evidence_at(&request, 48, now()).unwrap()).content,
        EvidenceContent::Absent
    );
    let (_db, reader, mut request) = fixture(Some(""));
    assert!(
        matches!(evidence(reader.read_evidence_at(&request,48,now()).unwrap()).content,EvidenceContent::Text { text,total_bytes:0,.. } if text.is_empty())
    );
    request.origin.field = EvidenceField::AppName;
    request.max_bytes = 16;
    assert!(
        matches!(evidence(reader.read_evidence_at(&request,48,now()).unwrap()).content,EvidenceContent::Text { text,.. } if text=="Notes")
    );
}
#[test]
fn invalid_ranges_do_not_silently_shift_or_empty_success() {
    let (_db, reader, request) = fixture(Some("😀text"));
    for (start, end, budget) in [
        (1, Some(4), 4),
        (0, Some(3), 4),
        (9, None, 4),
        (0, Some(20), 4),
        (5, Some(2), 4),
        (0, None, 3),
    ] {
        let candidate = EvidenceRequest {
            start,
            end,
            max_bytes: budget,
            ..request.clone()
        };
        assert!(matches!(
            reader.read_evidence_at(&candidate, 48, now()),
            Err(ContextPageError::InvalidRequest(_))
        ));
    }
}
#[test]
fn source_binding_is_checked_before_releasing_text() {
    let (_db, reader, request) = fixture(Some("secret"));
    for index in 0..3 {
        let mut candidate = request.clone();
        match index {
            0 => candidate.origin.store_identity = "other".into(),
            1 => candidate.origin.event_id = "other".into(),
            _ => candidate.origin.observed_at = "2026-09-06T09:00:00Z".into(),
        }
        assert_eq!(
            reader.read_evidence_at(&candidate, 48, now()).unwrap(),
            EvidenceResult::Denied
        );
    }
    let mut candidate = request;
    candidate.origin.observed_at = "2026-09-06t19:00:00.000000000000+09:00".into();
    assert!(matches!(
        reader.read_evidence_at(&candidate, 48, now()).unwrap(),
        EvidenceResult::Evidence(_)
    ));
}
#[test]
fn expired_and_deleted_rows_cannot_be_queried_again() {
    let (db, reader, request) = fixture(Some("retained"));
    assert!(matches!(
        reader
            .read_evidence_at(&request, 48, now() + Duration::hours(48))
            .unwrap(),
        EvidenceResult::Evidence(_)
    ));
    assert_eq!(
        reader
            .read_evidence_at(
                &request,
                48,
                now() + Duration::hours(48) + Duration::nanoseconds(1)
            )
            .unwrap(),
        EvidenceResult::Expired
    );
    Connection::open(db.path())
        .unwrap()
        .execute("DELETE FROM events", [])
        .unwrap();
    assert_eq!(
        reader.read_evidence_at(&request, 48, now()).unwrap(),
        EvidenceResult::Expired
    );
}
#[test]
fn snapshot_cutoff_survives_without_treating_omitted_text_as_complete() {
    let db = TestDatabase::new("evidence-snapshot");
    let mut event = clipboard_copy_shortcut("evt_00000000000000000000000001", NOW);
    event.event_type = "content.snapshot".into();
    event.version = 3;
    event.data = EventData::ContentSnapshot(ContentSnapshotData::new(
        Some("visible".into()),
        7,
        Some(ContentSnapshotCutoff::Nodes),
        ContentSnapshotTrigger::Settle,
    ));
    StoreWriter::open(db.path())
        .unwrap()
        .append(&event)
        .unwrap();
    let reader = StoreReader::open_known(db.path(), StoreFormat::Plaintext, None).unwrap();
    let request = EvidenceRequest {
        origin: EvidenceOrigin {
            store_identity: reader.append_head().unwrap().store_identity,
            append_sequence: 1,
            event_id: event.id,
            observed_at: NOW.into(),
            field: EvidenceField::Text,
        },
        start: 0,
        end: None,
        max_bytes: 16,
    };
    let value = evidence(reader.read_evidence_at(&request, 48, now()).unwrap());
    assert!(
        matches!(value.details.payload,EventData::ContentSnapshot(data) if data.cutoff()==Some(Some(ContentSnapshotCutoff::Nodes)) && data.text.is_none())
    );
    assert!(matches!(value.content,EvidenceContent::Text { text,.. } if text=="visible"));
}
#[test]
fn malformed_stored_content_is_an_error_not_absent() {
    let (db, reader, request) = fixture(Some("text"));
    Connection::open(db.path())
        .unwrap()
        .execute("UPDATE events SET data_json=?1", [r#"{"text":42}"#])
        .unwrap();
    let error = reader.read_evidence_at(&request, 48, now()).unwrap_err();
    assert!(
        matches!(error,ContextPageError::Store(ref e) if e.failure_kind()==StoreFailureKind::Corrupt)
    );
}

#[test]
fn event_size_omission_is_distinct_from_ordinary_privacy_redaction() {
    let db = TestDatabase::new("evidence-truncated");
    let mut event = clipboard_copy_shortcut("evt_00000000000000000000000001", NOW);
    event.mark_truncated();
    StoreWriter::open(db.path())
        .unwrap()
        .append(&event)
        .unwrap();
    let reader = StoreReader::open_known(db.path(), StoreFormat::Plaintext, None).unwrap();
    let mut request = EvidenceRequest {
        origin: EvidenceOrigin {
            store_identity: reader.append_head().unwrap().store_identity,
            append_sequence: 1,
            event_id: event.id.clone(),
            observed_at: NOW.into(),
            field: EvidenceField::Text,
        },
        start: 0,
        end: None,
        max_bytes: 16,
    };
    let value = evidence(reader.read_evidence_at(&request, 48, now()).unwrap());
    assert_eq!(value.content, EvidenceContent::Absent);
    assert!(value.details.truncated);
    event.id = "evt_00000000000000000000000002".into();
    event.redaction.rules = vec!["email".into()];
    event.redaction.applied = true;
    StoreWriter::open(db.path())
        .unwrap()
        .append(&event)
        .unwrap();
    request.origin.event_id = event.id;
    request.origin.append_sequence = 2;
    let value = evidence(reader.read_evidence_at(&request, 48, now()).unwrap());
    assert!(value.details.redaction_applied);
    assert!(!value.details.truncated);
}
