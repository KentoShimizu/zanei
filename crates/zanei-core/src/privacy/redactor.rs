use std::collections::HashSet;

use crate::config::RedactorKind;
use crate::schema::{Event, EventData, Redaction};

mod patterns;

fn rule_name(rule: RedactorKind) -> &'static str {
    match rule {
        RedactorKind::Email => "email",
        RedactorKind::CreditCard => "credit_card",
        RedactorKind::Token => "token",
    }
}

fn rule_marker(rule: RedactorKind) -> &'static str {
    match rule {
        RedactorKind::Email => "[REDACTED:email]",
        RedactorKind::CreditCard => "[REDACTED:credit_card]",
        RedactorKind::Token => "[REDACTED:token]",
    }
}

pub(crate) fn redact_event(mut event: Event, configured_rules: &[RedactorKind]) -> Event {
    let mut seen = HashSet::with_capacity(configured_rules.len());
    let mut fired = event
        .is_truncated()
        .then(|| Event::SIZE_LIMIT_RULE.to_owned())
        .into_iter()
        .collect::<Vec<_>>();

    for &rule in configured_rules {
        if seen.insert(rule) && apply_rule_to_event(&mut event, rule) {
            fired.push(rule_name(rule).to_owned());
        }
    }

    event.redaction = Redaction {
        applied: !fired.is_empty(),
        rules: fired,
    };
    event
}

fn apply_rule_to_event(event: &mut Event, rule: RedactorKind) -> bool {
    let mut changed = redact_required(&mut event.app.name, rule);
    if let Some(window) = &mut event.window {
        changed |= redact_optional(&mut window.title, rule);
    }
    if let Some(element) = &mut event.element {
        changed |= redact_optional(&mut element.title, rule);
        changed |= redact_optional(&mut element.value, rule);
    }

    changed | redact_payload(&mut event.data, rule)
}

fn redact_payload(data: &mut EventData, rule: RedactorKind) -> bool {
    match data {
        EventData::WindowTitle(value) => redact_optional(&mut value.prev_title, rule),
        EventData::UiValue(value) => redact_optional(&mut value.text, rule),
        EventData::InputKey(value) => redact_optional(&mut value.text, rule),
        EventData::ClipboardCopy(value) => redact_optional(&mut value.text, rule),
        EventData::ClipboardPaste(value) => redact_optional(&mut value.text, rule),
        EventData::BrowserNavigate(value) => {
            let url_changed = redact_optional(value.url.as_option_mut(), rule);
            url_changed | redact_optional(&mut value.tab_title, rule)
        }
        EventData::AppActivate(_)
        | EventData::AppLaunch(_)
        | EventData::AppTerminate(_)
        | EventData::WindowFocus(_)
        | EventData::UiFocus(_)
        | EventData::UiClick(_)
        | EventData::InputScroll(_) => false,
    }
}

fn redact_optional(value: &mut Option<String>, rule: RedactorKind) -> bool {
    value
        .as_mut()
        .is_some_and(|text| redact_required(text, rule))
}

fn redact_required(value: &mut String, rule: RedactorKind) -> bool {
    let redacted = redact_text(value, rule);
    if redacted == *value {
        false
    } else {
        *value = redacted;
        true
    }
}

pub(crate) fn redact_text(value: &str, rule: RedactorKind) -> String {
    patterns::redact(value, rule, rule_marker(rule))
}

#[cfg(test)]
mod tests {
    use crate::normalize::{URL_TITLE_FIELD_MAX_BYTES, enforce_size_limits};
    use crate::schema::{
        App, BrowserMode, BrowserNavigateData, BrowserTransition, Element, Event, EventData,
        UiValueData, Window, WindowTitleData,
    };

    use super::*;

    #[test]
    fn redacts_email_addresses_without_swallowing_punctuation() {
        assert_eq!(
            redact_text(
                "mail alice+tag@example.com, then bob@test.dev.",
                RedactorKind::Email
            ),
            "mail [REDACTED:email], then [REDACTED:email]."
        );
    }

    #[test]
    fn redacts_only_luhn_valid_card_candidates() {
        assert_eq!(
            redact_text(
                "valid 4111 1111 1111 1111 invalid 4111 1111 1111 1112",
                RedactorKind::CreditCard,
            ),
            "valid [REDACTED:credit_card] invalid 4111 1111 1111 1112"
        );
    }

    #[test]
    fn redacts_labeled_prefixed_bearer_and_jwt_tokens() {
        let value = "token=abcd1234 sk-testsecret Bearer xyz987 jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature";
        assert_eq!(
            redact_text(value, RedactorKind::Token),
            "token=[REDACTED:token] [REDACTED:token] Bearer [REDACTED:token] jwt [REDACTED:token]"
        );
    }

    #[test]
    fn event_redaction_tracks_unique_fired_rules_in_configuration_order() {
        let event = browser_event(
            "https://example.com/?token=abcd1234",
            Some("alice@example.com"),
            Some("card 4111 1111 1111 1111"),
        );
        let rules = [
            RedactorKind::Token,
            RedactorKind::Email,
            RedactorKind::Token,
            RedactorKind::CreditCard,
        ];

        let redacted = redact_event(event, &rules);

        assert_eq!(redacted.redaction.rules, ["token", "email", "credit_card"]);
        assert!(redacted.redaction.applied);
        let EventData::BrowserNavigate(data) = redacted.data else {
            panic!("expected browser data");
        };
        assert_eq!(data.url, "https://example.com/?token=[REDACTED:token]");
        assert_eq!(data.tab_title.as_deref(), Some("[REDACTED:email]"));
        assert_eq!(
            redacted
                .element
                .and_then(|element| element.value)
                .as_deref(),
            Some("card [REDACTED:credit_card]")
        );
    }

    #[test]
    fn redacts_email_in_previous_window_title() {
        let mut event = browser_event("https://example.com", None, None);
        event.source = "macos.ax".to_owned();
        event.event_type = "window.title".to_owned();
        event.element = None;
        event.data = EventData::WindowTitle(WindowTitleData {
            prev_title: Some("Inbox alice@example.com".to_owned()),
        });

        let redacted = redact_event(event, &[RedactorKind::Email]);

        assert!(redacted.redaction.applied);
        assert_eq!(redacted.redaction.rules, ["email"]);
        let EventData::WindowTitle(data) = redacted.data else {
            panic!("expected window title data");
        };
        assert_eq!(data.prev_title.as_deref(), Some("Inbox [REDACTED:email]"));
    }

    #[test]
    fn redacts_ui_value_input_delta() {
        let mut event = browser_event("https://example.com", None, None);
        event.source = "macos.ax".to_owned();
        event.event_type = "ui.value".to_owned();
        event.data = EventData::UiValue(UiValueData {
            field_kind: None,
            value_len: Some(17),
            text: Some("alice@example.com".to_owned()),
        });
        let redacted = redact_event(event, &[RedactorKind::Email]);
        let EventData::UiValue(data) = redacted.data else {
            panic!("expected ui.value data");
        };
        assert_eq!(data.text.as_deref(), Some("[REDACTED:email]"));
        assert_eq!(redacted.redaction.rules, ["email"]);
    }

    #[test]
    fn event_without_matches_has_consistent_empty_redaction() {
        let mut event = browser_event("https://example.com", Some("Public"), None);
        event.redaction = Redaction {
            applied: true,
            rules: vec!["stale".to_owned()],
        };
        let redacted = redact_event(event, &[RedactorKind::Email]);
        assert_eq!(
            redacted.redaction,
            Redaction {
                applied: false,
                rules: Vec::new(),
            }
        );
    }

    #[test]
    fn redaction_preserves_the_size_limit_transformation_rule() {
        let mut event = browser_event("https://example.com", Some("Public"), None);
        event.window.as_mut().expect("window").title =
            Some("x".repeat(URL_TITLE_FIELD_MAX_BYTES + 1));
        enforce_size_limits(&mut event);

        let redacted = redact_event(event, &[RedactorKind::Email]);

        assert!(redacted.is_truncated());
        assert_eq!(redacted.redaction.rules, ["size_limit"]);
        assert!(redacted.redaction.applied);
    }

    fn browser_event(url: &str, tab_title: Option<&str>, element_value: Option<&str>) -> Event {
        Event {
            version: 1,
            id: "evt_01J00000000000000000000000".to_owned(),
            ts: "2026-08-16T00:00:00.000Z".to_owned(),
            mono_ns: 1,
            source: "macos.applescript".to_owned(),
            event_type: "browser.navigate".to_owned(),
            app: App {
                name: "Chrome".to_owned(),
                bundle_id: Some("com.google.Chrome".to_owned()),
                pid: Some(1),
            },
            window: Some(Window {
                title: Some("Window".to_owned()),
                id: Some(2),
            }),
            element: Some(Element {
                role: Some("AXTextField".to_owned()),
                title: None,
                value: element_value.map(str::to_owned),
            }),
            data: EventData::BrowserNavigate(BrowserNavigateData {
                url: url.to_owned().into(),
                tab_title: tab_title.map(str::to_owned),
                mode: BrowserMode::Normal,
                transition: Some(BrowserTransition::Navigate),
            }),
            redaction: Redaction {
                applied: false,
                rules: Vec::new(),
            },
        }
    }
}
