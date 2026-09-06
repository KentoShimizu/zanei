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
        event
            .window
            .as_ref()
            .and_then(|window| window.title.as_deref()),
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
    let Some(version) = decision.chrome_version() else {
        if event.app.bundle_id.as_deref() == Some(CHROME_BUNDLE_ID) {
            suppress_text_content(&mut event.data, &mut event.element);
        }
        return TextBodyRoute::Send(event);
    };
    let Some(((pid, window_id), observed_at)) = event
        .app
        .pid
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
        config::{
            CapturePolicyConfig, FilterConfig,
            capture_policy::{BrowserMode, BrowserPolicy, IdePolicy, PolicyAction},
        },
        privacy::CHROME_BUNDLE_ID,
        schema::{
            App, ClickButton, Element, EventData, FieldKind, UiClickData, UiFocusData, Window,
        },
    };

    use super::*;
    use crate::{
        chrome::{ChromeEligibilityObservation, chrome_eligibility_channel},
        permission::SAFARI_BUNDLE_ID,
    };

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

        let earlier = chrome_policy.decision(
            PrivacyScope::TextContent,
            &app(CHROME_BUNDLE_ID),
            Some(11),
            Some("Window"),
        );
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

    #[test]
    fn safari_routes_generic_or_confirmed_body_without_crossing_modes() {
        let mut standalone = FilterConfig::default();
        standalone.text_content.exclude_apps.clear();
        let (publisher, tracker) = chrome_eligibility_channel(standalone.clone());
        let policy = CapturePolicy::new(tracker, standalone, None);
        let earlier = policy.decision(
            PrivacyScope::TextContent,
            &app(SAFARI_BUNDLE_ID),
            Some(11),
            Some("Window"),
        );
        let TextBodyRoute::Send(event) = route_text_body(
            ui_event(SAFARI_BUNDLE_ID, ui_bodies()[0].clone()),
            &policy,
            Some(&earlier),
        ) else {
            panic!("standalone Safari remains a generic body")
        };
        assert_eq!(
            event.element.and_then(|element| element.value),
            Some("private".to_owned())
        );

        policy.replace_filter(safari_filter());
        publisher.observe(
            7,
            ChromeEligibilityObservation::Safari {
                window_id: Some(11),
                url: Some("https://allowed.example/path".to_owned()),
            },
        );
        let current = policy.decision(
            PrivacyScope::TextContent,
            &app(SAFARI_BUNDLE_ID),
            Some(11),
            Some("Window"),
        );
        assert!(current.is_allowed());
        assert!(current.chrome_version().is_some());
        let TextBodyRoute::Send(stale) = route_text_body(
            ui_event(SAFARI_BUNDLE_ID, ui_bodies()[0].clone()),
            &policy,
            Some(&earlier),
        ) else {
            panic!("generic body cannot acquire a current confirmation version")
        };
        assert_eq!(stale.element.and_then(|element| element.value), None);

        assert!(matches!(
            route_text_body(
                ui_event(SAFARI_BUNDLE_ID, ui_bodies()[0].clone()),
                &policy,
                Some(&current),
            ),
            TextBodyRoute::Quarantine { .. }
        ));
    }

    fn safari_filter() -> FilterConfig {
        let mut filter = FilterConfig {
            capture_policy: Some(CapturePolicyConfig {
                allowed_apps: vec!["Safari".to_owned()],
                browser: BrowserPolicy {
                    mode: BrowserMode::AllSites,
                    default_policy: PolicyAction::Allow,
                    on_url_unavailable: PolicyAction::Block,
                    block_auth: false,
                    block_payments: false,
                    allow_list: Vec::new(),
                    block_list: Vec::new(),
                },
                ide: IdePolicy {
                    block_env_files: false,
                    on_file_name_unavailable: PolicyAction::Allow,
                },
            }),
            ..FilterConfig::default()
        };
        filter.text_content.exclude_apps.clear();
        filter
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
