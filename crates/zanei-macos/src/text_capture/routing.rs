//! Canonical routing for events that may carry captured text.

use time::OffsetDateTime;
use zanei_collector::RawEvent;
use zanei_core::{
    privacy::{CHROME_BUNDLE_ID, PrivacyScope, suppress_text_content},
    schema::EventData,
};

use crate::capture_policy::{CaptureDecision, CapturePolicy};

use super::ChromeWindowKey;

pub(crate) enum TextBodyRoute {
    Send(RawEvent),
    Quarantine {
        event: RawEvent,
        key: ChromeWindowKey,
        version: u64,
        observed_at: OffsetDateTime,
    },
}

pub(crate) fn route_text_body(
    mut event: RawEvent,
    capture_policy: &CapturePolicy,
    earlier_decision: Option<&CaptureDecision>,
) -> TextBodyRoute {
    let has_body = has_text_body(&event);
    let decision = capture_policy.decision_at_send(
        PrivacyScope::TextContent,
        &event.app,
        event.window.as_ref().and_then(|window| window.id),
        earlier_decision,
    );
    event.capture_context = decision.capture_context();
    if !has_body {
        return TextBodyRoute::Send(event);
    }
    if !decision.is_allowed() {
        suppress_text_content(&mut event.data, &mut event.element);
        return TextBodyRoute::Send(event);
    }
    if event.app.bundle_id.as_deref() != Some(CHROME_BUNDLE_ID) {
        return TextBodyRoute::Send(event);
    }
    let Some((((version, pid), window_id), observed_at)) = decision
        .chrome_version()
        .zip(event.app.pid)
        .zip(event.window.as_ref().and_then(|window| window.id))
        .zip(event.observed_at)
    else {
        suppress_text_content(&mut event.data, &mut event.element);
        return TextBodyRoute::Send(event);
    };
    TextBodyRoute::Quarantine {
        event,
        key: ChromeWindowKey { pid, window_id },
        version,
        observed_at,
    }
}

fn has_text_body(event: &RawEvent) -> bool {
    data_body(event) || element_body(event)
}

fn data_body(event: &RawEvent) -> bool {
    match &event.data {
        EventData::InputKey(data) => data.text.is_some(),
        EventData::ClipboardCopy(data) => data.text.is_some() || data.size_bytes.is_some(),
        EventData::ClipboardPaste(data) => data.text.is_some() || data.size_bytes.is_some(),
        EventData::UiValue(data) => data.text.is_some(),
        EventData::AppActivate(_)
        | EventData::AppLaunch(_)
        | EventData::AppTerminate(_)
        | EventData::WindowFocus(_)
        | EventData::WindowTitle(_)
        | EventData::UiFocus(_)
        | EventData::UiClick(_)
        | EventData::InputScroll(_)
        | EventData::BrowserNavigate(_)
        | EventData::ContentSnapshot(_) => false,
    }
}

fn element_body(event: &RawEvent) -> bool {
    event
        .element
        .as_ref()
        .and_then(|element| element.value.as_ref())
        .is_some()
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;
    use zanei_core::{
        config::FilterConfig,
        privacy::CHROME_BUNDLE_ID,
        schema::{
            App, ClickButton, Element, EventData, FieldKind, UiClickData, UiFocusData, Window,
        },
    };

    use super::*;
    use crate::chrome::{ChromeEligibilityObservation, chrome_eligibility_channel};

    #[test]
    fn v3_1_ui_focus_click_bodies_route_through_suppression() {
        let mut denied_filter = FilterConfig::default();
        denied_filter
            .text_content
            .exclude_apps
            .push("dev.example.App".to_owned());
        let (_, denied_tracker) = chrome_eligibility_channel(denied_filter.clone());
        let denied_policy = CapturePolicy::new(denied_tracker, denied_filter, None);

        for data in ui_bodies() {
            let TextBodyRoute::Send(event) =
                route_text_body(ui_event("dev.example.App", data), &denied_policy, None)
            else {
                panic!("denied UI body must not be quarantined");
            };
            assert_eq!(event.element.and_then(|element| element.value), None);
        }

        let filter = FilterConfig::default();
        let (publisher, tracker) = chrome_eligibility_channel(filter.clone());
        publisher.observe(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: "https://allowed.example/".to_owned(),
            },
        );
        let chrome_policy = CapturePolicy::new(tracker, filter, None);

        for data in ui_bodies() {
            assert!(matches!(
                route_text_body(ui_event(CHROME_BUNDLE_ID, data), &chrome_policy, None),
                TextBodyRoute::Quarantine { .. }
            ));
        }

        let earlier =
            chrome_policy.decision(PrivacyScope::TextContent, &app(CHROME_BUNDLE_ID), Some(11));
        publisher.observe(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: "https://changed.example/".to_owned(),
            },
        );
        let TextBodyRoute::Quarantine { version, .. } = route_text_body(
            ui_event(
                CHROME_BUNDLE_ID,
                ui_bodies().into_iter().next().expect("ui.focus body"),
            ),
            &chrome_policy,
            Some(&earlier),
        ) else {
            panic!("allowed Chrome UI body must be quarantined");
        };
        assert_eq!(Some(version), earlier.chrome_version());
    }

    fn ui_bodies() -> [EventData; 2] {
        [
            EventData::UiFocus(UiFocusData {
                field_kind: Some(FieldKind::Text),
            }),
            EventData::UiClick(UiClickData {
                button: ClickButton::Left,
                click_count: 1,
            }),
        ]
    }

    fn ui_event(bundle_id: &str, data: EventData) -> RawEvent {
        RawEvent {
            observed_at: Some(OffsetDateTime::UNIX_EPOCH),
            source: "macos.ax".to_owned(),
            event_type: "ui.test".to_owned(),
            app: app(bundle_id),
            window: Some(Window {
                title: Some("Window".to_owned()),
                id: Some(11),
            }),
            element: Some(Element {
                role: Some("AXStaticText".to_owned()),
                title: None,
                value: Some("private".to_owned()),
            }),
            data,
            capture_context: Default::default(),
        }
    }

    fn app(bundle_id: &str) -> App {
        App {
            name: "Example".to_owned(),
            bundle_id: Some(bundle_id.to_owned()),
            pid: Some(7),
        }
    }
}
