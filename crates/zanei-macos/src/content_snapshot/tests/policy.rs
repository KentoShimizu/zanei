use std::{thread, time::Duration};

use zanei_core::config::{FilterConfig, ScopedFilterConfig};

use crate::{
    chrome::{ChromeEligibilityObservation, chrome_eligibility_channel},
    content_snapshot::{
        ActivityError,
        policy::{ActivityProbe, SnapshotPolicy},
    },
    secure_input::{SecureInputProbe, secure_input_test_channel},
};

use super::support::app;

#[derive(Clone, Copy)]
struct FakeActivity(Result<f64, ActivityError>);

impl ActivityProbe for FakeActivity {
    fn seconds_since_last_input(&self) -> Result<f64, ActivityError> {
        self.0
    }
}

fn disconnected_probe() -> SecureInputProbe {
    let (probe, responder) = secure_input_test_channel();
    drop(responder);
    probe
}

fn secure_input_decision(enabled: bool) -> bool {
    let (_publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let (probe, responder) = secure_input_test_channel();
    let worker = thread::spawn(move || responder.respond_next(enabled));
    let policy = SnapshotPolicy::with_activity(
        FilterConfig::default(),
        tracker,
        probe,
        FakeActivity(Ok(0.0)),
    );
    let allowed = policy.secure_input_allows();
    worker.join().expect("Secure Input responder");
    allowed
}

#[test]
fn global_and_snapshot_app_scopes_are_both_required_and_reload_immediately() {
    let (_publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let target = app(7, "dev.example.App");
    let mut policy = SnapshotPolicy::with_activity(
        FilterConfig::default(),
        tracker,
        disconnected_probe(),
        FakeActivity(Ok(0.0)),
    );
    assert!(policy.app_allows(&target));

    policy.replace_filter(FilterConfig {
        exclude_apps: vec!["dev.example.App".to_owned()],
        ..FilterConfig::default()
    });
    assert!(!policy.app_allows(&target));

    policy.replace_filter(FilterConfig {
        content_snapshot: ScopedFilterConfig {
            include_only_apps: vec!["dev.other.App".to_owned()],
            ..ScopedFilterConfig::default()
        },
        ..FilterConfig::default()
    });
    assert!(!policy.app_allows(&target));
}

#[test]
fn chrome_unknown_incognito_global_site_and_snapshot_site_fail_closed() {
    let config = FilterConfig {
        exclude_websites: vec!["global.example".to_owned()],
        content_snapshot: ScopedFilterConfig {
            exclude_websites: vec!["snapshot.example".to_owned()],
            ..ScopedFilterConfig::default()
        },
        ..FilterConfig::default()
    };
    let (publisher, tracker) = chrome_eligibility_channel(config.clone());
    let chrome = app(7, "com.google.Chrome");
    let policy =
        SnapshotPolicy::with_activity(config, tracker, disconnected_probe(), FakeActivity(Ok(0.0)));

    assert!(!policy.chrome_allows(&chrome, 11));
    publisher.observe(
        7,
        ChromeEligibilityObservation::Incognito {
            window_id: Some(11),
        },
    );
    assert!(!policy.chrome_allows(&chrome, 11));
    publisher.observe(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(11),
            url: "https://global.example/page".to_owned(),
        },
    );
    assert!(!policy.chrome_allows(&chrome, 11));
    publisher.observe(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(11),
            url: "https://snapshot.example/page".to_owned(),
        },
    );
    assert!(!policy.chrome_allows(&chrome, 11));
    publisher.observe(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(11),
            url: "https://public.example/page".to_owned(),
        },
    );
    assert!(policy.chrome_allows(&chrome, 11));
    assert_eq!(
        policy.capture_context(&chrome, 11).website_host.as_deref(),
        Some("public.example")
    );
}

#[test]
fn secure_input_enabled_timeout_and_disconnect_all_fail_closed() {
    assert!(!secure_input_decision(true));
    assert!(secure_input_decision(false));

    let (_publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let policy = SnapshotPolicy::with_activity(
        FilterConfig::default(),
        tracker,
        disconnected_probe(),
        FakeActivity(Ok(0.0)),
    );
    assert!(!policy.secure_input_allows());

    let (_publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let (probe, responder) = secure_input_test_channel();
    let policy = SnapshotPolicy::with_activity(
        FilterConfig::default(),
        tracker,
        probe,
        FakeActivity(Ok(0.0)),
    );
    assert!(!policy.secure_input_allows(), "unanswered probe times out");
    drop(responder);
}

#[test]
fn refresh_requires_input_within_its_own_interval_and_rejects_probe_errors() {
    for (activity, expected) in [(29.0, true), (30.0, true), (30.1, false)] {
        let (_publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
        let policy = SnapshotPolicy::with_activity(
            FilterConfig::default(),
            tracker,
            disconnected_probe(),
            FakeActivity(Ok(activity)),
        );
        assert_eq!(
            policy.refresh_activity_allows(Some(Duration::from_secs(30))),
            expected
        );
    }

    let (_publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let policy = SnapshotPolicy::with_activity(
        FilterConfig::default(),
        tracker,
        disconnected_probe(),
        FakeActivity(Err(ActivityError::Negative { seconds: -1.0 })),
    );
    assert!(!policy.refresh_activity_allows(Some(Duration::from_secs(30))));
    assert!(policy.refresh_activity_allows(None));
}
