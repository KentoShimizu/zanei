use serde_json::{Value, json};
use std::fs;
use time::{Duration, OffsetDateTime};
use zanei_core::normalize::format_timestamp;
use zanei_core::schema::{
    App, ContentSnapshotData, ContentSnapshotTrigger, Event, EventData, Redaction, Window,
};
use zanei_core::store::StoreWriter;
mod support;
use support::Fixture;

fn page(cursor: Value, upper_bound: Value, limit: usize) -> Value {
    json!({"protocol_version":1,"kind":"page","cursor":cursor,"upper_bound":upper_bound,"limit":limit})
}
fn run(f: &Fixture, input: Value) -> (Value, usize) {
    raw(f, input.to_string().into_bytes())
}
fn raw(f: &Fixture, input: Vec<u8>) -> (Value, usize) {
    let output = f
        .command()
        .args(["context-read", "--verbose"])
        .write_stdin(input)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["protocol_version"], 1);
    (result, output.stdout.len())
}
fn event(index: usize, text: Option<&str>) -> Event {
    let data = EventData::ContentSnapshot(ContentSnapshotData::new(
        text.map(str::to_owned),
        text.map_or(0, |t| t.chars().count() as u64),
        None,
        ContentSnapshotTrigger::Settle,
    ));
    Event {
        version: 3,
        id: format!("evt_{index:026}"),
        ts: format_timestamp(OffsetDateTime::now_utc()),
        mono_ns: index as u64,
        source: "test.context".into(),
        event_type: data.event_type().into(),
        app: App {
            name: "Test".into(),
            bundle_id: Some("dev.example.test".into()),
            pid: Some(1),
        },
        window: Some(Window {
            title: Some("Window".into()),
            id: Some(2),
        }),
        element: None,
        data,
        redaction: Redaction {
            applied: true,
            rules: vec!["size_limit".into()],
        },
    }
}
fn origin(p: &Value, index: usize, field: &str) -> Value {
    let row = &p["observations"][index];
    json!({"store_identity":p["store_identity"],"append_sequence":row["append_sequence"],"event_id":row["id"],"observed_at":row["ts"],"field":field})
}
fn evidence(origin: Value, start: Value, end: Value) -> Value {
    json!({"protocol_version":1,"kind":"evidence","origin":origin,"start":start,"end":end})
}
#[test]
fn initial_page_fixed_upper_continuation_and_active_store_only() {
    let f = Fixture::empty();
    f.open_writer()
        .append_batch(&[event(1, Some("one")), event(2, Some("two"))])
        .unwrap();
    let retired = support::set_aside_store_path(&f.store, 1);
    StoreWriter::open(retired)
        .unwrap()
        .append(&event(999, Some("retired")))
        .unwrap();
    let (first, _) = run(&f, page(Value::Null, Value::Null, 1));
    assert_eq!(first["kind"], "page");
    assert_eq!(first["observations"].as_array().unwrap().len(), 1);
    assert_eq!(first["coverage"], json!({"after":0,"through":1}));
    assert_eq!(first["has_more"], true);
    f.open_writer().append(&event(3, Some("new"))).unwrap();
    let (second, _) = run(
        &f,
        page(
            first["next_cursor"].clone(),
            first["upper_bound"].clone(),
            256,
        ),
    );
    assert_eq!(second["store_identity"], first["store_identity"]);
    assert_eq!(second["coverage"], json!({"after":1,"through":2}));
    assert_eq!(second["has_more"], false);
    let (new, _) = run(&f, page(second["next_cursor"].clone(), Value::Null, 256));
    assert_eq!(new["coverage"], json!({"after":2,"through":3}));
    assert_eq!(new["observations"].as_array().unwrap().len(), 1);
}
#[test]
fn page_escaping_budget_adopts_maximal_prefix_without_skips() {
    let f = Fixture::empty();
    let rows: Vec<Event> = (1..=256)
        .map(|i| {
            let mut e = event(i, Some("body must never enter observations"));
            e.app.name = "\0".repeat(1024);
            e.app.bundle_id = Some("\0".repeat(1024));
            e.window.as_mut().unwrap().title = Some("\0".repeat(1024));
            e
        })
        .collect();
    f.open_writer().append_batch(&rows).unwrap();
    let (mut p, size) = run(&f, page(Value::Null, Value::Null, 256));
    assert!(size <= 512 * 1024);
    assert_eq!(p["has_more"], true);
    let count = p["observations"].as_array().unwrap().len();
    assert!((1..256).contains(&count));
    assert_eq!(p["coverage"]["through"], count);
    // Asking for the next larger prefix still selects the same maximum.
    let (max, _) = run(&f, page(Value::Null, p["upper_bound"].clone(), count + 1));
    assert_eq!(max, p);
    let mut seen = 0;
    loop {
        for row in p["observations"].as_array().unwrap() {
            seen += 1;
            assert_eq!(row["append_sequence"], seen);
            assert!(row.get("data").is_none());
        }
        if p["has_more"] == false {
            break;
        }
        let (next, bytes) = run(
            &f,
            page(p["next_cursor"].clone(), p["upper_bound"].clone(), 256),
        );
        assert!(bytes <= 512 * 1024);
        assert_eq!(next["coverage"]["after"], seen);
        p = next;
    }
    assert_eq!(seen, 256);
}
#[test]
fn giant_evidence_utf8_nul_escaping_continues_without_loss_and_preserves_metadata() {
    let f = Fixture::empty();
    let original = "あ😀\0\"\\".repeat(25_000);
    f.open_writer().append(&event(1, Some(&original))).unwrap();
    let (p, _) = run(&f, page(Value::Null, Value::Null, 256));
    let o = origin(&p, 0, "text");
    let (mut start, mut end) = (json!(0), Value::Null);
    let mut received = String::new();
    loop {
        let (r, bytes) = run(&f, evidence(o.clone(), start.clone(), end.clone()));
        assert_eq!(r["kind"], "evidence");
        assert!(bytes <= 64 * 1024);
        assert_eq!(r["origin"], o);
        assert_eq!(r["metadata"]["event_type"], "content.snapshot");
        assert_eq!(r["metadata"]["payload_without_text"]["text"], Value::Null);
        assert_eq!(
            r["metadata"]["payload_without_text"]["chars"],
            original.chars().count()
        );
        assert_eq!(r["metadata"]["redaction_applied"], true);
        assert_eq!(r["metadata"]["truncated"], true);
        let c = &r["content"];
        assert_eq!(c["start"], start);
        assert_eq!(c["total_bytes"], original.len());
        let text = c["text"].as_str().unwrap();
        assert!(!text.is_empty());
        received.push_str(text);
        assert_eq!(c["end"], received.len());
        if c["remaining"].is_null() {
            break;
        }
        start = c["remaining"][0].clone();
        end = c["remaining"][1].clone();
    }
    assert_eq!(received, original);
    let (slice, _) = run(&f, evidence(o.clone(), json!(3), json!(7)));
    assert_eq!(slice["content"]["text"], "😀");
    let (invalid, _) = run(&f, evidence(o, json!(4), json!(7)));
    assert_eq!(invalid["kind"], "invalid_request");
}
#[test]
fn absent_empty_and_omitted_metadata_have_distinct_meanings() {
    let f = Fixture::empty();
    let mut absent = event(1, None);
    absent.app.name = "A".repeat(1025);
    absent.app.bundle_id = None;
    f.open_writer()
        .append_batch(&[absent, event(2, Some(""))])
        .unwrap();
    let (p, _) = run(&f, page(Value::Null, Value::Null, 256));
    assert_eq!(
        p["observations"][0]["app_name"],
        json!({"kind":"omitted","utf8_bytes":1025})
    );
    assert_eq!(p["observations"][0]["bundle_id"]["kind"], "absent");
    let (a, _) = run(&f, evidence(origin(&p, 0, "text"), json!(0), Value::Null));
    assert_eq!(a["content"], json!({"kind":"absent"}));
    let (b, _) = run(&f, evidence(origin(&p, 1, "text"), json!(0), Value::Null));
    assert_eq!(
        b["content"],
        json!({"kind":"text","text":"","start":0,"end":0,"total_bytes":0,"remaining":null})
    );
}
#[test]
fn expired_rows_emit_gap_and_expired_evidence_and_binding_mismatch_is_denied() {
    let f = Fixture::empty();
    let mut old = event(1, Some("old"));
    old.ts = format_timestamp(OffsetDateTime::now_utc() - Duration::hours(25));
    f.open_writer()
        .append_batch(&[old.clone(), event(2, Some("current"))])
        .unwrap();
    fs::write(&f.config, "[output]\nretention_hours=24\n").unwrap();
    let (gap, _) = run(&f, page(Value::Null, Value::Null, 256));
    assert_eq!(gap["kind"], "gap");
    assert_eq!(gap["reason"], "retention_or_deletion");
    assert_eq!(gap["affected_range"], json!({"after":0,"through":1}));
    let (p, _) = run(
        &f,
        page(
            gap["resume_cursor"].clone(),
            gap["upper_bound"].clone(),
            256,
        ),
    );
    assert_eq!(p["coverage"], json!({"after":1,"through":2}));
    let mut o = origin(&p, 0, "text");
    o["append_sequence"] = json!(1);
    o["event_id"] = json!(old.id);
    o["observed_at"] = json!(old.ts);
    assert_eq!(
        run(&f, evidence(o, json!(0), Value::Null)).0["kind"],
        "expired"
    );
    for field in ["store_identity", "event_id", "observed_at"] {
        let mut o = origin(&p, 0, "text");
        o[field] = json!(if field == "observed_at" {
            "2020-01-01T00:00:00Z"
        } else {
            "wrong"
        });
        assert_eq!(
            run(&f, evidence(o, json!(0), Value::Null)).0["kind"],
            "denied"
        );
    }
    let other = Fixture::empty();
    let (reset, _) = run(
        &other,
        page(p["next_cursor"].clone(), p["upper_bound"].clone(), 256),
    );
    assert_eq!(reset["kind"], "gap");
    assert_eq!(reset["reason"], "store_changed");
    assert_eq!(reset["affected_range"], json!({"after":2,"through":2}));
    assert_ne!(reset["store_identity"], p["store_identity"]);
}
#[test]
fn closed_requests_reject_invalid_versions_cursors_fields_and_oversize_input() {
    let f = Fixture::empty();
    let valid = page(Value::Null, Value::Null, 1);
    let mut bad_version = valid.clone();
    bad_version["protocol_version"] = json!(2);
    let (r, _) = run(&f, bad_version);
    assert_eq!(
        r,
        json!({"protocol_version":1,"kind":"incompatible","reason":"protocol","version":2})
    );
    let mut extra = valid.clone();
    extra["path"] = json!("SECRET");
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("cursor");
    for input in [
        extra.to_string().into_bytes(),
        missing.to_string().into_bytes(),
        b"{}{}".to_vec(),
        vec![0xff],
        vec![b' '; 8193],
        page(json!("v2:{}"), Value::Null, 1)
            .to_string()
            .into_bytes(),
        page(json!("v1:{}"), Value::Null, 1)
            .to_string()
            .into_bytes(),
        page(Value::Null, Value::Null, 0).to_string().into_bytes(),
        page(Value::Null, Value::Null, 257).to_string().into_bytes(),
    ] {
        assert_eq!(raw(&f, input).0["kind"], "invalid_request");
    }
    let mut padded = valid.to_string().into_bytes();
    padded.resize(8192, b' ');
    assert_eq!(raw(&f, padded).0["kind"], "page");
}
#[test]
fn strict_config_and_key_failures_are_typed_without_disclosure_or_key_creation() {
    let f = Fixture::empty();
    let request = page(Value::Null, Value::Null, 1);
    for config in ["[output]\nretention_hours=0", "not toml SECRET"] {
        fs::write(&f.config, config).unwrap();
        assert_eq!(
            run(&f, request.clone()).0,
            json!({"protocol_version":1,"kind":"unavailable","reason":"config"})
        );
    }
    fs::remove_file(&f.config).unwrap();
    assert_eq!(run(&f, request.clone()).0["reason"], "config");
    fs::write(&f.config, "").unwrap();
    fs::remove_file(&f.key_file).unwrap();
    assert_eq!(run(&f, request.clone()).0["reason"], "key");
    assert!(!f.key_file.exists());
    fs::write(&f.key_file, "SECRET invalid key").unwrap();
    assert_eq!(run(&f, request.clone()).0["reason"], "key");
    let output = f
        .command()
        .env("ZANEI_KEYCHAIN_NO_PROMPT", "SECRET invalid")
        .arg("context-read")
        .write_stdin(request.to_string())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["reason"],
        "key"
    );
}

#[test]
fn unavailable_store_and_schema_corruption_are_closed_failures() {
    let f = Fixture::uninitialized();
    fs::write(&f.config, "").unwrap();
    let request = page(Value::Null, Value::Null, 1);
    assert_eq!(run(&f, request.clone()).0["reason"], "store");
    assert!(!f.store.exists());
    StoreWriter::open(&f.store).unwrap();
    let db = rusqlite::Connection::open(&f.store).unwrap();
    db.execute("UPDATE meta SET schema_version=999", [])
        .unwrap();
    assert_eq!(
        run(&f, request.clone()).0,
        json!({"protocol_version":1,"kind":"incompatible","reason":"store_schema","version":999})
    );
    drop(db);
    fs::write(&f.store, b"SECRET not a database").unwrap();
    assert_eq!(
        run(&f, request).0,
        json!({"protocol_version":1,"kind":"incompatible","reason":"store_corrupt","version":null})
    );
}

#[test]
fn explicit_paths_are_required_and_origin_fields_are_closed() {
    let f = Fixture::empty();
    let output = assert_cmd::Command::cargo_bin("zanei")
        .unwrap()
        .env("ZANEI_STORE_KEY_FILE", &f.key_file)
        .arg("context-read")
        .write_stdin(page(Value::Null, Value::Null, 1).to_string())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["kind"],
        "invalid_request"
    );
    f.open_writer().append(&event(1, Some("value"))).unwrap();
    let (p, _) = run(&f, page(Value::Null, Value::Null, 1));
    for field in ["arbitrary.pointer", "unknown"] {
        assert_eq!(
            run(&f, evidence(origin(&p, 0, field), json!(0), Value::Null)).0["kind"],
            "invalid_request"
        );
    }
    let mut o = origin(&p, 0, "text");
    o["path"] = json!("SECRET");
    assert_eq!(
        run(&f, evidence(o, json!(0), Value::Null)).0["kind"],
        "invalid_request"
    );
}
