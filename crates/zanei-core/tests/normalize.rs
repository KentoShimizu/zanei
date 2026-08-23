use time::{Duration, OffsetDateTime};
use zanei_core::normalize::{Normalizer, TEXT_FIELD_MAX_BYTES, URL_TITLE_FIELD_MAX_BYTES};
use zanei_core::schema::{
    App, Element, EventData, FieldKind, InputKeyData, InputKeyKind, InputScrollData, RawEvent,
    ScrollDirection, UiValueData, Window, WindowTitleData,
};

#[test]
fn input_keys_coalesce_at_the_two_second_gap_boundary() {
    let mut normalizer = Normalizer::new();
    normalizer
        .push_at(key("a"), at(0), 0)
        .expect("first key should buffer");
    normalizer
        .push_at(key("b"), at(2), 2_000_000_000)
        .expect("boundary key should coalesce");
    let events = normalizer.flush();

    assert_eq!(events.len(), 1);
    let EventData::InputKey(data) = &events[0].data else {
        panic!("expected input.key");
    };
    assert_eq!(data.count, 2);
    assert_eq!(data.text.as_deref(), Some("ab"));
}

#[test]
fn input_keys_keep_text_captured_after_ime_is_disabled() {
    let mut normalizer = Normalizer::new();
    normalizer
        .push_at(key_with_text(None), at(0), 0)
        .expect("IME key should buffer without text");
    normalizer
        .push_at(key_with_text(Some("a")), at(1), 1_000_000_000)
        .expect("direct key should coalesce");
    let events = normalizer.flush();

    let EventData::InputKey(data) = &events[0].data else {
        panic!("expected input.key");
    };
    assert_eq!(data.count, 2);
    assert_eq!(data.text.as_deref(), Some("a"));
}

#[test]
fn scrolls_sum_amount_and_count_within_one_second() {
    let mut normalizer = Normalizer::new();
    normalizer
        .push_at(scroll(2.5), at(0), 0)
        .expect("first scroll should buffer");
    normalizer
        .push_at(scroll(1.5), at(1), 1_000_000_000)
        .expect("boundary scroll should coalesce");
    let events = normalizer.flush();

    let EventData::InputScroll(data) = &events[0].data else {
        panic!("expected input.scroll");
    };
    assert_eq!(data.amount, 4.0);
    assert_eq!(data.count, 2);
}

#[test]
fn window_title_debounce_keeps_only_the_final_event() {
    let mut normalizer = Normalizer::new();
    normalizer
        .push_at(window_title("Draft", None), at(0), 0)
        .expect("first title should buffer");
    normalizer
        .push_at(
            window_title("Final", Some("Draft")),
            at_millis(500),
            500_000_000,
        )
        .expect("boundary title should replace");
    let events = normalizer.flush();

    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]
            .window
            .as_ref()
            .and_then(|window| window.title.as_deref()),
        Some("Final")
    );
}

#[test]
fn ui_value_is_not_coalesced_by_the_core_normalizer() {
    let mut normalizer = Normalizer::new();
    let first = normalizer
        .push_at(ui_value("a"), at(0), 0)
        .expect("first value should buffer");
    let second = normalizer
        .push_at(ui_value("ab"), at(1), 1_000_000_000)
        .expect("second value should emit independently");
    let events = [first, second].concat();

    assert_eq!(events.len(), 2);
    assert_eq!(
        events[1]
            .element
            .as_ref()
            .and_then(|element| element.value.as_deref()),
        None
    );
    let EventData::UiValue(data) = &events[1].data else {
        panic!("expected ui.value");
    };
    assert_eq!(data.value_len, Some(2));
    assert_eq!(data.text.as_deref(), Some("ab"));
}

#[test]
fn oversized_ui_value_text_is_dropped_without_losing_value_length() {
    let text = "界".repeat(TEXT_FIELD_MAX_BYTES / "界".len() + 1);
    let value_len = 90_000;
    let mut raw = ui_value(&text);
    let EventData::UiValue(data) = &mut raw.data else {
        panic!("expected ui.value");
    };
    data.value_len = Some(value_len);
    let mut normalizer = Normalizer::new();

    let events = normalizer
        .push_at(raw, at(0), 0)
        .expect("oversized text should produce a metadata-only event");

    let event = &events[0];
    let EventData::UiValue(data) = &event.data else {
        panic!("expected ui.value");
    };
    assert_eq!(data.text, None);
    assert_eq!(data.value_len, Some(value_len));
    assert!(event.is_truncated());
    let encoded = serde_json::to_value(&event.event).expect("truncated event should serialize");
    assert_eq!(encoded["truncated"], true);
    assert_eq!(
        encoded["redaction"]["rules"],
        serde_json::json!(["size_limit"])
    );
}

#[test]
fn byte_limit_keeps_exact_boundaries_and_drops_the_first_excess_byte() {
    let mut normalizer = Normalizer::new();
    let exact = normalizer
        .push_at(ui_value(&"x".repeat(TEXT_FIELD_MAX_BYTES)), at(0), 0)
        .expect("exact text boundary");
    let excess = normalizer
        .push_at(
            window_title(&"x".repeat(URL_TITLE_FIELD_MAX_BYTES + 1), None),
            at(1),
            1_000_000_000,
        )
        .expect("oversized title");

    let EventData::UiValue(exact_data) = &exact[0].data else {
        panic!("expected ui.value");
    };
    assert_eq!(
        exact_data.text.as_ref().map(String::len),
        Some(TEXT_FIELD_MAX_BYTES)
    );
    assert!(!exact[0].is_truncated());
    assert_eq!(
        excess[0]
            .window
            .as_ref()
            .and_then(|window| window.title.as_ref()),
        None
    );
    assert!(excess[0].is_truncated());
}

#[test]
fn coalesced_input_text_is_dropped_when_the_combined_bytes_exceed_the_limit() {
    let chunk = "x".repeat(TEXT_FIELD_MAX_BYTES / 2 + 1);
    let mut normalizer = Normalizer::new();
    normalizer
        .push_at(key(&chunk), at(0), 0)
        .expect("first bounded key chunk");
    normalizer
        .push_at(key(&chunk), at(1), 1_000_000_000)
        .expect("second bounded key chunk");

    let events = normalizer.flush();
    let EventData::InputKey(data) = &events[0].data else {
        panic!("expected input.key");
    };
    assert_eq!(data.count, 2);
    assert_eq!(data.text, None);
    assert!(events[0].is_truncated());
}

fn base(event_type: &str, data: EventData) -> RawEvent {
    RawEvent {
        source: "macos.eventtap".to_owned(),
        event_type: event_type.to_owned(),
        app: App {
            name: "Editor".to_owned(),
            bundle_id: Some("com.example.Editor".to_owned()),
            pid: Some(7),
        },
        window: Some(Window {
            title: Some("Document".to_owned()),
            id: Some(10),
        }),
        element: None,
        data,
        capture_context: Default::default(),
    }
}

fn key(text: &str) -> RawEvent {
    key_with_text(Some(text))
}

fn key_with_text(text: Option<&str>) -> RawEvent {
    base(
        "input.key",
        EventData::InputKey(InputKeyData {
            kind: InputKeyKind::Text,
            modifiers: Vec::new(),
            count: 1,
            combo: None,
            text: text.map(str::to_owned),
            field_kind: Some(FieldKind::Text),
        }),
    )
}

fn scroll(amount: f64) -> RawEvent {
    base(
        "input.scroll",
        EventData::InputScroll(InputScrollData {
            direction: ScrollDirection::Down,
            amount,
            count: 1,
        }),
    )
}

fn window_title(title: &str, previous: Option<&str>) -> RawEvent {
    let mut raw = base(
        "window.title",
        EventData::WindowTitle(WindowTitleData {
            prev_title: previous.map(str::to_owned),
        }),
    );
    raw.source = "macos.ax".to_owned();
    raw.window.as_mut().expect("fixture window").title = Some(title.to_owned());
    raw
}

fn ui_value(value: &str) -> RawEvent {
    let mut raw = base(
        "ui.value",
        EventData::UiValue(UiValueData {
            field_kind: Some(FieldKind::Text),
            value_len: Some(value.len() as u64),
            text: Some(value.to_owned()),
        }),
    );
    raw.source = "macos.ax".to_owned();
    raw.element = Some(Element {
        role: Some("AXTextField".to_owned()),
        title: Some("Body".to_owned()),
        value: None,
    });
    raw
}

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::seconds(seconds)
}

fn at_millis(milliseconds: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::milliseconds(milliseconds)
}
