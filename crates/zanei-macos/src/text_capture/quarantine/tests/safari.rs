use zanei_core::config::{
    CapturePolicyConfig, FilterConfig,
    capture_policy::{BrowserMode, BrowserPolicy, IdePolicy, PolicyAction},
};

use super::*;
use crate::{
    chrome::{
        ChromeEligibilityObservation, ChromeEligibilityPublisher, chrome_eligibility_channel,
    },
    permission::SAFARI_BUNDLE_ID,
    text_capture::routing::{TextBodyRoute, route_text_body},
};

fn setup() -> (ChromeEligibilityPublisher, CapturePolicy, TextQuarantine) {
    let mut filter = FilterConfig {
        capture_policy: Some(CapturePolicyConfig {
            allowed_apps: Some(vec!["Safari".to_owned()]),
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
    let (publisher, tracker) = chrome_eligibility_channel(filter.clone());
    (
        publisher,
        CapturePolicy::new(tracker, filter, None),
        TextQuarantine::new(ChromeObserver::new()),
    )
}

fn safari_event() -> RawEvent {
    let mut event = event(OffsetDateTime::UNIX_EPOCH);
    event.app.name = "Safari".to_owned();
    event.app.bundle_id = Some(SAFARI_BUNDLE_ID.to_owned());
    event
}

#[test]
fn confirmation_releases_only_the_same_version() {
    let (publisher, policy, mut quarantine) = setup();
    let mut verify = |initial_url: &str, confirmation: ChromeEligibilityObservation| {
        publisher.observe(
            7,
            ChromeEligibilityObservation::Safari {
                window_id: Some(11),
                url: Some(initial_url.to_owned()),
            },
        );
        let TextBodyRoute::Quarantine {
            event,
            key,
            version,
            ..
        } = route_text_body(safari_event(), &policy, None)
        else {
            panic!("app-owned Safari body awaits confirmation")
        };
        let held_at = Instant::now();
        quarantine.hold_at(event, key, version, HeldBodyKind::Text, held_at);
        publisher.observe_at(7, confirmation, held_at + Duration::from_millis(1));
        quarantine
            .release(held_at + Duration::from_millis(2), &policy)
            .pop()
            .expect("confirmed metadata")
            .into_parts()
            .0
    };

    let same = verify(
        "https://same.example/path",
        ChromeEligibilityObservation::Safari {
            window_id: Some(11),
            url: Some("https://same.example/path".to_owned()),
        },
    );
    assert_eq!(input_text(&same), Some("private"));
    assert_eq!(
        same.capture_context.url.as_deref(),
        Some("https://same.example/path")
    );

    let changed = verify(
        "https://old.example/path",
        ChromeEligibilityObservation::Safari {
            window_id: Some(11),
            url: Some("https://new.example/path".to_owned()),
        },
    );
    assert_eq!(input_text(&changed), None);
    assert_eq!(
        changed.capture_context.url.as_deref(),
        Some("https://old.example/path")
    );

    let unavailable = verify(
        "https://available.example/path",
        ChromeEligibilityObservation::Unavailable {
            window_id: Some(11),
        },
    );
    assert_eq!(input_text(&unavailable), None);

    publisher.observe(
        7,
        ChromeEligibilityObservation::Safari {
            window_id: Some(11),
            url: Some("https://timeout.example/path".to_owned()),
        },
    );
    let TextBodyRoute::Quarantine {
        event,
        key,
        version,
        ..
    } = route_text_body(safari_event(), &policy, None)
    else {
        panic!("app-owned Safari body awaits confirmation")
    };
    let held_at = Instant::now();
    quarantine.hold_at(event, key, version, HeldBodyKind::Text, held_at);
    let timeout = quarantine
        .release(held_at + CONFIRMATION_SAFETY_CAP, &policy)
        .pop()
        .expect("timeout releases metadata")
        .into_parts()
        .0;
    assert_eq!(input_text(&timeout), None);
}

fn input_text(event: &RawEvent) -> Option<&str> {
    let EventData::InputKey(data) = &event.data else {
        panic!("input.key")
    };
    data.text.as_deref()
}
