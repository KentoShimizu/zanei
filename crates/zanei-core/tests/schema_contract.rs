use serde_json::{Value, json};
use time::OffsetDateTime;
use zanei_core::config::{FilterConfig, RedactorKind};
use zanei_core::normalize::{CONTENT_SNAPSHOT_SAFETY_MAX_BYTES, normalize};
use zanei_core::privacy::PrivacyFilter;
use zanei_core::schema::{
    App, ContentSnapshotData, ContentSnapshotTrigger, Event, EventData, KNOWN_EVENT_TYPES,
    RawEvent, Window,
};

const EVENT_SCHEMA: &str = include_str!("../../../docs/public/schema/event.schema.json");

#[test]
fn all_fourteen_event_types_match_the_json_schema_and_round_trip() {
    let validator = validator();
    let fixtures = fixtures();

    assert_eq!(fixtures.len(), KNOWN_EVENT_TYPES.len());
    for fixture in fixtures {
        assert_acceptance(&validator, fixture.event_type, &fixture.valid, true);
        assert_acceptance(
            &validator,
            &format!("invalid {} payload", fixture.event_type),
            &fixture.invalid,
            false,
        );

        let event: Event = serde_json::from_value(fixture.valid.clone())
            .unwrap_or_else(|error| panic!("{} did not deserialize: {error}", fixture.event_type));
        let encoded = serde_json::to_value(&event)
            .unwrap_or_else(|error| panic!("{} did not serialize: {error}", fixture.event_type));
        assert_eq!(
            encoded, fixture.valid,
            "{} changed on round-trip",
            fixture.event_type
        );
    }
}

#[test]
fn schema_and_rust_accept_the_same_boundary_corpus() {
    let validator = validator();
    let valid_launch = envelope("app.launch", json!({}));

    let mut unknown_envelope_field = valid_launch.clone();
    unknown_envelope_field["unexpected"] = json!(true);
    let mut unknown_app_field = valid_launch.clone();
    unknown_app_field["app"]["unexpected"] = json!(true);
    let mut unknown_redaction_field = valid_launch.clone();
    unknown_redaction_field["redaction"]["unexpected"] = json!(true);
    let mut unknown_window_field = envelope("window.focus", json!({}));
    unknown_window_field["window"]["unexpected"] = json!(true);
    let mut unknown_element_field = envelope("ui.focus", json!({ "field_kind": null }));
    unknown_element_field["element"]["unexpected"] = json!(true);
    let mut missing_truncated = valid_launch.clone();
    missing_truncated
        .as_object_mut()
        .expect("envelope object")
        .remove("truncated");
    let mut invalid_timestamp = valid_launch.clone();
    invalid_timestamp["ts"] = json!("not-rfc3339");
    let mut invalid_mono_ns = valid_launch.clone();
    invalid_mono_ns["mono_ns"] = json!(-1);
    let mut maximum_integers = envelope(
        "ui.click",
        json!({ "button": "left", "click_count": u64::MAX }),
    );
    maximum_integers["mono_ns"] = json!(u64::MAX);
    maximum_integers["app"]["pid"] = json!(i64::MAX);
    maximum_integers["window"]["id"] = json!(i64::MIN);
    let mut invalid_ulid = valid_launch.clone();
    invalid_ulid["id"] = json!("evt_Z0000000000000000000000000");
    let unknown_type = envelope("future.event", json!({}));
    let mut content_with_v1 = envelope(
        "content.snapshot",
        json!({ "text": "Visible", "chars": 7, "complete": true, "trigger": "settle" }),
    );
    content_with_v1["v"] = json!(1);
    let mut existing_with_v2 = envelope("app.launch", json!({}));
    existing_with_v2["v"] = json!(2);

    let mut app_activate_without_window = envelope("app.activate", json!({}));
    app_activate_without_window["window"] = Value::Null;
    let mut app_activate_element = envelope("app.activate", json!({}));
    app_activate_element["element"] = element();
    let mut app_launch_window = valid_launch.clone();
    app_launch_window["window"] = window();
    let mut window_without_context = envelope("window.focus", json!({}));
    window_without_context["window"] = Value::Null;
    let mut ui_without_element = envelope("ui.focus", json!({ "field_kind": null }));
    ui_without_element["element"] = Value::Null;
    let mut input_with_element = envelope(
        "input.key",
        json!({
            "kind": "text", "modifiers": [], "count": 1,
            "combo": null, "text": null, "field_kind": null
        }),
    );
    input_with_element["element"] = element();

    let mut duplicate_modifiers = envelope(
        "input.key",
        json!({
            "kind": "shortcut", "modifiers": ["cmd", "cmd"], "count": 1,
            "combo": "cmd+s", "text": null, "field_kind": null
        }),
    );
    duplicate_modifiers["app"]["pid"] = json!(i64::MIN);

    let mut truncated_without_rule = envelope("app.launch", json!({}));
    truncated_without_rule["truncated"] = json!(true);
    let mut rule_without_truncated = envelope("app.launch", json!({}));
    rule_without_truncated["redaction"] = json!({ "applied": true, "rules": ["size_limit"] });
    let null_browser_url = envelope(
        "browser.navigate",
        json!({
            "url": null, "tab_title": "Example",
            "mode": "normal", "transition": null
        }),
    );

    let mut unknown_copy = envelope(
        "clipboard.copy",
        json!({
            "origin": "unknown", "content_kind": "text",
            "size_bytes": null, "text": null
        }),
    );
    unknown_copy["app"] = json!({ "name": "Unknown", "bundle_id": null, "pid": null });
    unknown_copy["window"] = Value::Null;
    let mut attributed_unknown_copy = unknown_copy.clone();
    attributed_unknown_copy["app"] =
        json!({ "name": "Frontmost", "bundle_id": "dev.example.App", "pid": 42 });
    let mut body_on_unknown_copy = unknown_copy.clone();
    body_on_unknown_copy["data"]["text"] = json!("private");

    let cases = [
        ("closed valid envelope", valid_launch, true),
        ("unknown envelope field", unknown_envelope_field, false),
        ("unknown app field", unknown_app_field, false),
        ("unknown redaction field", unknown_redaction_field, false),
        ("unknown window field", unknown_window_field, false),
        ("unknown element field", unknown_element_field, false),
        ("missing truncated", missing_truncated, false),
        ("invalid timestamp", invalid_timestamp, false),
        ("negative u64", invalid_mono_ns, false),
        ("integer boundaries", maximum_integers, true),
        ("ULID overflow", invalid_ulid, false),
        ("unknown event type", unknown_type, false),
        ("content snapshot with v1", content_with_v1, false),
        ("existing event with v2", existing_with_v2, false),
        (
            "app.activate without window",
            app_activate_without_window,
            true,
        ),
        ("app.activate element", app_activate_element, false),
        ("app.launch window", app_launch_window, false),
        ("window event without window", window_without_context, false),
        ("ui event without element", ui_without_element, false),
        ("input event with element", input_with_element, false),
        ("duplicate modifiers", duplicate_modifiers, false),
        (
            "truncated without size_limit",
            truncated_without_rule,
            false,
        ),
        (
            "size_limit without truncated",
            rule_without_truncated,
            false,
        ),
        (
            "null browser URL without truncation",
            null_browser_url,
            false,
        ),
        ("unattributed clipboard copy", unknown_copy, true),
        (
            "attributed unknown clipboard copy",
            attributed_unknown_copy,
            false,
        ),
        (
            "body on unknown clipboard copy",
            body_on_unknown_copy,
            false,
        ),
    ];

    for (name, value, expected) in cases {
        assert_acceptance(&validator, name, &value, expected);
    }
}

#[test]
fn unordered_unique_modifiers_are_accepted_and_normalized() {
    let validator = validator();
    let unordered = envelope(
        "input.key",
        json!({
            "kind": "shortcut",
            "modifiers": ["fn", "ctrl", "opt", "shift", "cmd"],
            "count": 1,
            "combo": "cmd+shift+opt+ctrl+fn+s",
            "text": null,
            "field_kind": null
        }),
    );

    assert!(validator.is_valid(&unordered));
    let mut event: Event =
        serde_json::from_value(unordered).expect("unique modifiers should deserialize");
    let normalized = serde_json::to_value(&event).expect("normalized event should serialize");

    assert_eq!(
        normalized["data"]["modifiers"],
        json!(["cmd", "shift", "opt", "ctrl", "fn"])
    );

    let EventData::InputKey(data) = &mut event.data else {
        panic!("expected input.key data");
    };
    data.modifiers.reverse();
    let error =
        serde_json::to_value(event).expect_err("non-canonical modifiers must not serialize");
    assert!(
        error
            .to_string()
            .contains("modifiers must be sorted and unique")
    );
}

#[test]
fn truncated_browser_url_round_trips_with_the_size_limit_rule() {
    let validator = validator();
    let mut truncated = envelope(
        "browser.navigate",
        json!({
            "url": null, "tab_title": "Example",
            "mode": "normal", "transition": null
        }),
    );
    truncated["truncated"] = json!(true);
    truncated["redaction"] = json!({ "applied": true, "rules": ["size_limit"] });

    assert_acceptance(&validator, "truncated browser URL", &truncated, true);
    let event: Event = serde_json::from_value(truncated.clone()).expect("consistent marker");
    assert!(event.is_truncated());
    assert_eq!(serde_json::to_value(event).expect("round trip"), truncated);
}

#[test]
fn serialization_rejects_mismatched_type_version_pairs() {
    let mut existing: Event =
        serde_json::from_value(envelope("app.launch", json!({}))).expect("existing event");
    existing.version = 2;
    assert!(serde_json::to_value(existing).is_err());

    let mut content: Event = serde_json::from_value(envelope(
        "content.snapshot",
        json!({ "text": "Visible", "chars": 7, "complete": true, "trigger": "settle" }),
    ))
    .expect("content event");
    content.version = 1;
    assert!(serde_json::to_value(content).is_err());
}

#[test]
fn content_snapshot_redaction_and_independent_size_boundaries_are_preserved() {
    let original = "Contact alice@example.com";
    let filter = PrivacyFilter::new(FilterConfig {
        redactors: vec![RedactorKind::Email],
        ..Default::default()
    });
    let redacted = filter
        .process(normalized_snapshot(original.to_owned(), true))
        .expect("snapshot remains in scope");
    let EventData::ContentSnapshot(data) = redacted.data else {
        panic!("expected content.snapshot");
    };
    assert_eq!(data.text.as_deref(), Some("Contact [REDACTED:email]"));
    assert_eq!(data.chars, original.chars().count() as u64);
    assert!(data.complete);
    assert_eq!(redacted.redaction.rules, ["email"]);

    for bytes in [32 * 1024, 32 * 1024 + 1, CONTENT_SNAPSHOT_SAFETY_MAX_BYTES] {
        let event = normalized_snapshot("x".repeat(bytes), bytes <= 32 * 1024).event;
        let EventData::ContentSnapshot(data) = &event.data else {
            panic!("expected content.snapshot");
        };
        assert_eq!(data.text.as_ref().map(String::len), Some(bytes));
        assert_eq!(data.chars, bytes as u64);
        assert_eq!(data.complete, bytes <= 32 * 1024);
        assert!(!event.is_truncated());
    }

    let oversized_bytes = CONTENT_SNAPSHOT_SAFETY_MAX_BYTES + 1;
    let event = normalized_snapshot("x".repeat(oversized_bytes), false).event;
    let EventData::ContentSnapshot(data) = &event.data else {
        panic!("expected content.snapshot");
    };
    assert_eq!(data.text, None);
    assert_eq!(data.chars, oversized_bytes as u64);
    assert!(!data.complete);
    assert!(event.is_truncated());
}

fn normalized_snapshot(text: String, complete: bool) -> zanei_core::normalize::NormalizedEvent {
    let chars = text.chars().count() as u64;
    normalize(
        RawEvent {
            observed_at: None,
            source: "macos.ax".to_owned(),
            event_type: "content.snapshot".to_owned(),
            app: App {
                name: "Example".to_owned(),
                bundle_id: Some("com.example.App".to_owned()),
                pid: Some(42),
            },
            window: Some(Window {
                title: Some("Example window".to_owned()),
                id: Some(7),
            }),
            element: None,
            data: EventData::ContentSnapshot(ContentSnapshotData {
                text: Some(text),
                chars,
                complete,
                trigger: ContentSnapshotTrigger::Settle,
            }),
            capture_context: Default::default(),
        },
        OffsetDateTime::UNIX_EPOCH,
        1,
    )
    .expect("valid content snapshot")
}

fn validator() -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(EVENT_SCHEMA).expect("schema must be JSON");
    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .expect("schema must compile")
}

fn assert_acceptance(validator: &jsonschema::Validator, name: &str, value: &Value, expected: bool) {
    let schema_accepts = validator.is_valid(value);
    let rust_accepts = serde_json::from_value::<Event>(value.clone()).is_ok();
    assert_eq!(
        schema_accepts,
        expected,
        "schema acceptance mismatch for {name}: {:?}",
        validator.iter_errors(value).collect::<Vec<_>>()
    );
    assert_eq!(
        rust_accepts, expected,
        "Rust acceptance mismatch for {name}"
    );
    assert_eq!(
        schema_accepts, rust_accepts,
        "schema and Rust disagree for {name}"
    );
}

struct Fixture {
    event_type: &'static str,
    valid: Value,
    invalid: Value,
}

fn fixtures() -> Vec<Fixture> {
    vec![
        fixture("app.activate", json!({}), json!({ "prev_bundle_id": null })),
        fixture("app.launch", json!({}), json!({ "unexpected": true })),
        fixture("app.terminate", json!({}), json!({ "unexpected": true })),
        fixture("window.focus", json!({}), json!({ "unexpected": true })),
        fixture("window.title", json!({ "prev_title": null }), json!({})),
        fixture("ui.focus", json!({ "field_kind": null }), json!({})),
        fixture(
            "ui.click",
            json!({ "button": "left", "click_count": 1 }),
            json!({ "button": "left", "click_count": 0 }),
        ),
        fixture(
            "ui.value",
            json!({ "field_kind": null, "value_len": 0, "text": null }),
            json!({ "field_kind": null, "value_len": -1, "text": null }),
        ),
        fixture(
            "input.key",
            json!({
                "kind": "text", "modifiers": [], "count": 1,
                "combo": null, "text": null, "field_kind": null
            }),
            json!({
                "kind": "text", "modifiers": [], "count": 0,
                "combo": null, "text": null, "field_kind": null
            }),
        ),
        fixture(
            "input.scroll",
            json!({ "direction": "down", "amount": 1.0, "count": 1 }),
            json!({ "direction": "down", "amount": 1.0, "count": 0 }),
        ),
        fixture(
            "browser.navigate",
            json!({
                "url": "https://example.com/path", "tab_title": null,
                "mode": "normal", "transition": "navigate"
            }),
            json!({
                "url": "https://example.com/path", "tab_title": null,
                "mode": "incognito", "transition": null
            }),
        ),
        fixture(
            "clipboard.copy",
            json!({
                "origin": "copy_shortcut", "content_kind": "text",
                "size_bytes": null, "text": null
            }),
            json!({ "content_kind": "text", "size_bytes": null, "text": null }),
        ),
        fixture(
            "clipboard.paste",
            json!({
                "content_kind": "text", "size_bytes": null,
                "text": null, "field_kind": null
            }),
            json!({ "content_kind": "text", "size_bytes": null, "text": null }),
        ),
        fixture(
            "content.snapshot",
            json!({
                "text": "Visible text", "chars": 12,
                "complete": true, "trigger": "settle"
            }),
            json!({
                "text": "Visible text", "chars": 12,
                "complete": true, "trigger": "unknown"
            }),
        ),
    ]
}

fn fixture(event_type: &'static str, valid_data: Value, invalid_data: Value) -> Fixture {
    Fixture {
        event_type,
        valid: envelope(event_type, valid_data),
        invalid: envelope(event_type, invalid_data),
    }
}

fn envelope(event_type: &str, data: Value) -> Value {
    let mut value = json!({
        "v": if event_type == "content.snapshot" { 2 } else { 1 },
        "id": format!("evt_{}", ulid::Ulid::new()),
        "ts": "2026-08-16T12:34:56.789Z",
        "mono_ns": 123456789,
        "source": "macos.ax",
        "type": event_type,
        "app": {
            "name": "Example",
            "bundle_id": "com.example.App",
            "pid": 42
        },
        "window": null,
        "element": null,
        "data": data,
        "truncated": false,
        "redaction": { "applied": false, "rules": [] }
    });

    if matches!(
        event_type,
        "app.activate"
            | "window.focus"
            | "window.title"
            | "ui.focus"
            | "ui.click"
            | "ui.value"
            | "input.key"
            | "input.scroll"
            | "browser.navigate"
            | "clipboard.copy"
            | "clipboard.paste"
            | "content.snapshot"
    ) {
        value["window"] = window();
    }
    if matches!(event_type, "ui.focus" | "ui.click" | "ui.value") {
        value["element"] = element();
    }
    value
}

fn window() -> Value {
    json!({ "title": "Example window", "id": 7 })
}

fn element() -> Value {
    json!({ "role": "AXButton", "title": "Example element", "value": null })
}
