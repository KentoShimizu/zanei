use crate::schema::{Event, EventData};

/// Text content remains useful at this size while bounding per-field redaction,
/// serialization, and batch-memory cost.
pub const TEXT_FIELD_MAX_BYTES: usize = 64 * 1024;

/// URLs and titles are contextual metadata; 4 KiB covers normal values while
/// rejecting abnormal OS or application payloads before persistence.
pub const URL_TITLE_FIELD_MAX_BYTES: usize = 4 * 1024;

pub(crate) fn enforce_size_limits(event: &mut Event) {
    let mut truncated = false;

    if let Some(window) = &mut event.window {
        truncated |= drop_oversized(&mut window.title, URL_TITLE_FIELD_MAX_BYTES);
    }
    if let Some(element) = &mut event.element {
        truncated |= drop_oversized(&mut element.title, URL_TITLE_FIELD_MAX_BYTES);
        truncated |= drop_oversized(&mut element.value, TEXT_FIELD_MAX_BYTES);
    }

    truncated |= match &mut event.data {
        EventData::WindowTitle(data) => {
            drop_oversized(&mut data.prev_title, URL_TITLE_FIELD_MAX_BYTES)
        }
        EventData::UiValue(data) => drop_oversized(&mut data.text, TEXT_FIELD_MAX_BYTES),
        EventData::InputKey(data) => drop_oversized(&mut data.text, TEXT_FIELD_MAX_BYTES),
        EventData::BrowserNavigate(data) => {
            drop_oversized(data.url.as_option_mut(), URL_TITLE_FIELD_MAX_BYTES)
                | drop_oversized(&mut data.tab_title, URL_TITLE_FIELD_MAX_BYTES)
        }
        EventData::ClipboardCopy(data) => drop_oversized(&mut data.text, TEXT_FIELD_MAX_BYTES),
        EventData::ClipboardPaste(data) => drop_oversized(&mut data.text, TEXT_FIELD_MAX_BYTES),
        _ => false,
    };

    if truncated {
        event.mark_truncated();
    }
}

fn drop_oversized(value: &mut Option<String>, max_bytes: usize) -> bool {
    if value.as_ref().is_some_and(|value| value.len() > max_bytes) {
        *value = None;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::schema::{
        App, BrowserMode, BrowserNavigateData, ClipboardCopyData, ClipboardOrigin,
        ClipboardPasteData, ContentKind, Element, Event, EventData, FieldKind, InputKeyData,
        InputKeyKind, Redaction, UiValueData, Window, WindowTitleData,
    };

    use super::*;

    #[test]
    fn limits_every_text_payload_without_changing_length_metadata() {
        let oversized = Some("x".repeat(TEXT_FIELD_MAX_BYTES + 1));
        let payloads = [
            EventData::UiValue(UiValueData {
                field_kind: Some(FieldKind::Text),
                value_len: Some(99_999),
                text: oversized.clone(),
            }),
            EventData::InputKey(InputKeyData {
                kind: InputKeyKind::Text,
                modifiers: Vec::new(),
                count: 1,
                combo: None,
                text: oversized.clone(),
                field_kind: Some(FieldKind::Text),
            }),
            EventData::ClipboardCopy(ClipboardCopyData {
                origin: ClipboardOrigin::CopyShortcut,
                content_kind: ContentKind::Text,
                size_bytes: Some(99_999),
                text: oversized.clone(),
            }),
            EventData::ClipboardPaste(ClipboardPasteData {
                content_kind: ContentKind::Text,
                size_bytes: Some(99_999),
                text: oversized,
                field_kind: Some(FieldKind::Text),
            }),
        ];

        for payload in payloads {
            let mut event = event(payload);
            enforce_size_limits(&mut event);

            let (text, length) = match &event.data {
                EventData::UiValue(data) => (&data.text, data.value_len),
                EventData::InputKey(data) => (&data.text, None),
                EventData::ClipboardCopy(data) => (&data.text, data.size_bytes),
                EventData::ClipboardPaste(data) => (&data.text, data.size_bytes),
                _ => unreachable!("fixture contains only text payloads"),
            };
            assert_eq!(text, &None);
            assert!(length.is_none_or(|value| value == 99_999));
            assert!(event.is_truncated());
        }
    }

    #[test]
    fn limits_envelope_and_payload_titles_and_browser_url() {
        let oversized = "x".repeat(URL_TITLE_FIELD_MAX_BYTES + 1);
        let mut title_event = event(EventData::WindowTitle(WindowTitleData {
            prev_title: Some(oversized.clone()),
        }));
        title_event.window = Some(Window {
            title: Some(oversized.clone()),
            id: Some(1),
        });
        title_event.element = Some(Element {
            role: Some("AXButton".to_owned()),
            title: Some(oversized.clone()),
            value: Some("x".repeat(TEXT_FIELD_MAX_BYTES + 1)),
        });
        enforce_size_limits(&mut title_event);

        assert_eq!(
            title_event
                .window
                .as_ref()
                .and_then(|value| value.title.as_ref()),
            None
        );
        assert_eq!(
            title_event
                .element
                .as_ref()
                .and_then(|value| value.title.as_ref()),
            None
        );
        assert_eq!(
            title_event
                .element
                .as_ref()
                .and_then(|value| value.value.as_ref()),
            None
        );
        let EventData::WindowTitle(title) = &title_event.data else {
            panic!("expected window.title");
        };
        assert_eq!(title.prev_title, None);
        assert!(title_event.is_truncated());

        let mut browser_event = event(EventData::BrowserNavigate(BrowserNavigateData {
            url: oversized.clone().into(),
            tab_title: Some(oversized),
            mode: BrowserMode::Normal,
            transition: None,
        }));
        enforce_size_limits(&mut browser_event);
        let EventData::BrowserNavigate(browser) = &browser_event.data else {
            panic!("expected browser.navigate");
        };
        assert_eq!(browser.url.as_deref(), None);
        assert_eq!(browser.tab_title, None);
        assert!(browser_event.is_truncated());
    }

    fn event(data: EventData) -> Event {
        Event {
            version: 1,
            id: "evt_01J00000000000000000000000".to_owned(),
            ts: "2026-08-16T00:00:00.000Z".to_owned(),
            mono_ns: 1,
            source: "test.normalize".to_owned(),
            event_type: data.event_type().to_owned(),
            app: App {
                name: "Example".to_owned(),
                bundle_id: Some("dev.example.App".to_owned()),
                pid: Some(1),
            },
            window: None,
            element: None,
            data,
            redaction: Redaction {
                applied: false,
                rules: Vec::new(),
            },
        }
    }
}
