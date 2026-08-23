use time::OffsetDateTime;
use zanei_core::config::{FilterConfig, RedactorKind, ScopedFilterConfig};
use zanei_core::normalize::{NormalizedEvent, Normalizer};
use zanei_core::privacy::PrivacyFilter;
use zanei_core::schema::{
    App, CaptureContext, ClipboardCopyData, ClipboardOrigin, ClipboardPasteData, ContentKind,
    Element, Event, EventData, FieldKind, InputKeyData, InputKeyKind, RawEvent, Redaction,
    UiValueData, Window,
};

#[test]
fn text_scope_nulls_every_body_field_but_keeps_facts_and_events() {
    let filter = PrivacyFilter::new(FilterConfig {
        text_content: ScopedFilterConfig {
            exclude_apps: vec!["dev.example.Editor".to_owned()],
            ..ScopedFilterConfig::default()
        },
        ..FilterConfig::default()
    });
    let payloads = [
        EventData::InputKey(InputKeyData {
            kind: InputKeyKind::Shortcut,
            modifiers: Vec::new(),
            count: 2,
            combo: Some("Cmd+V".to_owned()),
            text: Some("typed".to_owned()),
            field_kind: Some(FieldKind::Text),
        }),
        EventData::UiValue(UiValueData {
            field_kind: Some(FieldKind::Text),
            value_len: Some(5),
            text: Some("typed".to_owned()),
        }),
        EventData::ClipboardCopy(ClipboardCopyData {
            origin: ClipboardOrigin::CopyShortcut,
            content_kind: ContentKind::Text,
            size_bytes: Some(5),
            text: Some("typed".to_owned()),
        }),
        EventData::ClipboardPaste(ClipboardPasteData {
            content_kind: ContentKind::Text,
            size_bytes: Some(5),
            text: Some("typed".to_owned()),
            field_kind: Some(FieldKind::Text),
        }),
    ];

    for payload in payloads {
        let event = filter
            .process(normalized(payload, None))
            .expect("scoped text denial keeps the event");
        assert_eq!(event.element.and_then(|element| element.value), None);
        match event.data {
            EventData::InputKey(data) => {
                assert_eq!(data.text, None);
                assert_eq!(data.combo.as_deref(), Some("Cmd+V"));
                assert_eq!(data.field_kind, Some(FieldKind::Text));
                assert_eq!(data.count, 2);
            }
            EventData::UiValue(data) => {
                assert_eq!(data.text, None);
                assert_eq!(data.value_len, Some(5));
                assert_eq!(data.field_kind, Some(FieldKind::Text));
            }
            EventData::ClipboardCopy(data) => {
                assert_eq!(data.text, None);
                assert_eq!(data.size_bytes, None);
                assert_eq!(data.content_kind, ContentKind::Text);
                assert_eq!(data.origin, ClipboardOrigin::CopyShortcut);
            }
            EventData::ClipboardPaste(data) => {
                assert_eq!(data.text, None);
                assert_eq!(data.size_bytes, None);
                assert_eq!(data.field_kind, Some(FieldKind::Text));
            }
            _ => panic!("fixture contains only text-bearing variants"),
        }
    }
}

#[test]
fn global_and_scoped_app_and_host_rules_run_in_privacy_order() {
    let globally_denied = PrivacyFilter::new(FilterConfig {
        exclude_apps: vec!["dev.example.Editor".to_owned()],
        ..FilterConfig::default()
    });
    assert!(
        globally_denied
            .process(normalized(input_text("secret"), None))
            .is_none()
    );

    let scoped_host = PrivacyFilter::new(FilterConfig {
        text_content: ScopedFilterConfig {
            exclude_apps: Vec::new(),
            exclude_websites: vec!["private.example".to_owned()],
            ..ScopedFilterConfig::default()
        },
        ..FilterConfig::default()
    });
    let event = scoped_host
        .process(normalized_for(
            input_text("secret"),
            Some("api.private.example"),
            chrome_app(),
        ))
        .expect("host scope keeps the event");
    let EventData::InputKey(data) = event.data else {
        panic!("expected input.key");
    };
    assert_eq!(data.text, None);

    let globally_denied_host = PrivacyFilter::new(FilterConfig {
        exclude_websites: vec!["private.example".to_owned()],
        text_content: ScopedFilterConfig {
            exclude_apps: Vec::new(),
            ..ScopedFilterConfig::default()
        },
        ..FilterConfig::default()
    });
    let event = globally_denied_host
        .process(normalized_for(
            input_text("secret"),
            Some("private.example"),
            chrome_app(),
        ))
        .expect("global website policy nulls non-browser text rather than hiding the fact");
    let EventData::InputKey(data) = event.data else {
        panic!("expected input.key");
    };
    assert_eq!(data.text, None);
}

#[test]
fn snapshot_scope_api_and_redaction_use_the_same_context() {
    let filter = PrivacyFilter::new(FilterConfig {
        content_snapshot: ScopedFilterConfig {
            exclude_apps: Vec::new(),
            exclude_websites: vec!["private.example".to_owned()],
            ..ScopedFilterConfig::default()
        },
        redactors: vec![RedactorKind::Email],
        ..FilterConfig::default()
    });
    let app = chrome_app();
    assert!(filter.content_snapshot_is_allowed(&app, Some("public.example")));
    assert!(!filter.content_snapshot_is_allowed(&app, Some("private.example")));
    assert!(!filter.content_snapshot_is_allowed(&app, None));

    let redacted = filter
        .process(normalized_for(
            EventData::UiValue(UiValueData {
                field_kind: Some(FieldKind::Email),
                value_len: Some(17),
                text: Some("alice@example.com".to_owned()),
            }),
            Some("public.example"),
            chrome_app(),
        ))
        .expect("event passes");
    let EventData::UiValue(data) = redacted.data else {
        panic!("expected ui.value");
    };
    assert_eq!(data.text.as_deref(), Some("[REDACTED:email]"));
    assert_eq!(redacted.redaction.rules, ["email"]);
}

#[test]
fn coalescing_never_crosses_window_or_website_host_context() {
    let mut normalizer = Normalizer::new();
    let now = OffsetDateTime::now_utc();
    assert!(
        normalizer
            .push_at(raw_input(1, "one.example"), now, 1)
            .expect("first input")
            .is_empty()
    );
    assert!(
        normalizer
            .push_at(raw_input(1, "two.example"), now, 2)
            .expect("different host")
            .is_empty()
    );
    assert!(
        normalizer
            .push_at(raw_input(2, "two.example"), now, 3)
            .expect("different window")
            .is_empty()
    );

    let events = normalizer.flush();
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0].capture_context.website_host.as_deref(),
        Some("one.example")
    );
    assert_eq!(
        events[1].capture_context.website_host.as_deref(),
        Some("two.example")
    );
    assert_eq!(
        events[2].window.as_ref().and_then(|window| window.id),
        Some(2)
    );
}

fn normalized(data: EventData, website_host: Option<&str>) -> NormalizedEvent {
    normalized_for(data, website_host, app())
}

fn normalized_for(data: EventData, website_host: Option<&str>, app: App) -> NormalizedEvent {
    let event_type = data.event_type().to_owned();
    NormalizedEvent {
        event: Event {
            version: 1,
            id: "evt_01K00000000000000000002001".to_owned(),
            ts: "2026-08-23T00:00:00.000Z".to_owned(),
            mono_ns: 1,
            source: "test.privacy".to_owned(),
            event_type,
            app,
            window: None,
            element: Some(Element {
                role: Some("AXTextField".to_owned()),
                title: None,
                value: Some("element body".to_owned()),
            }),
            data,
            redaction: Redaction {
                applied: false,
                rules: Vec::new(),
            },
        },
        capture_context: CaptureContext {
            website_host: website_host.map(str::to_owned),
        },
    }
}

fn input_text(text: &str) -> EventData {
    EventData::InputKey(InputKeyData {
        kind: InputKeyKind::Text,
        modifiers: Vec::new(),
        count: 1,
        combo: None,
        text: Some(text.to_owned()),
        field_kind: Some(FieldKind::Text),
    })
}

fn app() -> App {
    App {
        name: "Editor".to_owned(),
        bundle_id: Some("dev.example.Editor".to_owned()),
        pid: Some(7),
    }
}

fn chrome_app() -> App {
    App {
        name: "Google Chrome".to_owned(),
        bundle_id: Some("com.google.Chrome".to_owned()),
        pid: Some(7),
    }
}

fn raw_input(window_id: i64, website_host: &str) -> RawEvent {
    RawEvent {
        source: "test.privacy".to_owned(),
        event_type: "input.key".to_owned(),
        app: app(),
        window: Some(Window {
            title: Some("Document".to_owned()),
            id: Some(window_id),
        }),
        element: None,
        data: input_text("x"),
        capture_context: CaptureContext {
            website_host: Some(website_host.to_owned()),
        },
    }
}
