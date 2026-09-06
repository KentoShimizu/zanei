use std::{thread, time::Duration};

use zanei_core::{
    config::{
        CapturePolicyConfig, FilterConfig, ScopedFilterConfig,
        capture_policy::{BrowserMode, BrowserPolicy, IdePolicy, PolicyAction},
    },
    privacy::PrivacyScope,
    schema::App,
};

use crate::{
    capture_policy::{ActivityProbe, CapturePolicy},
    chrome::{ChromeEligibilityObservation, chrome_eligibility_channel},
    content_snapshot::ActivityError,
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

fn title_policy(on_file_name_unavailable: PolicyAction) -> FilterConfig {
    title_policy_with(true, on_file_name_unavailable)
}

fn title_policy_with(
    block_env_files: bool,
    on_file_name_unavailable: PolicyAction,
) -> FilterConfig {
    FilterConfig {
        capture_policy: Some(CapturePolicyConfig {
            allowed_apps: vec![
                "Cursor".to_owned(),
                "Notes".to_owned(),
                "Google Chrome".to_owned(),
            ],
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
                block_env_files,
                on_file_name_unavailable,
            },
        }),
        ..FilterConfig::default()
    }
}

fn app_named(name: &str) -> App {
    App {
        name: name.to_owned(),
        bundle_id: None,
        pid: Some(7),
    }
}

fn title_policy_capture(on_file_name_unavailable: PolicyAction) -> CapturePolicy {
    let filter = title_policy(on_file_name_unavailable);
    let (_, tracker) = chrome_eligibility_channel(filter.clone());
    CapturePolicy::new(tracker, filter, None)
}

fn secure_input_decision(enabled: bool) -> bool {
    let (_publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let (probe, responder) = secure_input_test_channel();
    let worker = thread::spawn(move || responder.respond_next(enabled));
    let policy = CapturePolicy::with_activity(
        tracker,
        FilterConfig::default(),
        Some(probe),
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
    let policy = CapturePolicy::with_activity(
        tracker,
        FilterConfig::default(),
        Some(disconnected_probe()),
        FakeActivity(Ok(0.0)),
    );
    assert!(
        policy
            .decision(
                PrivacyScope::ContentSnapshot,
                &target.raw_app(),
                Some(11),
                None,
            )
            .is_allowed()
    );

    policy.replace_filter(FilterConfig {
        exclude_apps: vec!["dev.example.App".to_owned()],
        ..FilterConfig::default()
    });
    assert!(
        !policy
            .decision(
                PrivacyScope::ContentSnapshot,
                &target.raw_app(),
                Some(11),
                None,
            )
            .is_allowed()
    );

    policy.replace_filter(FilterConfig {
        content_snapshot: ScopedFilterConfig {
            include_only_apps: vec!["dev.other.App".to_owned()],
            ..ScopedFilterConfig::default()
        },
        ..FilterConfig::default()
    });
    assert!(
        !policy
            .decision(
                PrivacyScope::ContentSnapshot,
                &target.raw_app(),
                Some(11),
                None,
            )
            .is_allowed()
    );
}

#[test]
fn title_policy_applies_to_general_apps_and_ide_titles() {
    let policy = title_policy_capture(PolicyAction::Block);
    let cursor = app_named("Cursor");
    let notes = app_named("Notes");
    let other = app_named("Other");

    assert!(
        policy
            .decision(
                PrivacyScope::TextContent,
                &cursor,
                Some(11),
                Some("main.rs")
            )
            .is_allowed()
    );
    assert!(
        !policy
            .decision(PrivacyScope::TextContent, &cursor, Some(11), Some(".env"))
            .is_allowed()
    );
    assert!(
        policy
            .decision(
                PrivacyScope::TextContent,
                &cursor,
                Some(11),
                Some(".env.example")
            )
            .is_allowed()
    );
    assert!(
        !policy
            .decision(PrivacyScope::TextContent, &cursor, Some(11), None)
            .is_allowed()
    );
    let allow_missing_title = title_policy_capture(PolicyAction::Allow);
    assert!(
        allow_missing_title
            .decision(PrivacyScope::TextContent, &cursor, Some(11), None)
            .is_allowed()
    );
    assert!(
        policy
            .decision(PrivacyScope::TextContent, &notes, Some(11), None)
            .is_allowed()
    );
    assert!(
        !policy
            .decision(PrivacyScope::TextContent, &other, Some(11), None)
            .is_allowed()
    );
}

#[test]
fn browser_display_names_with_unrecognized_bundles_still_require_policy_identity() {
    let mut filter = title_policy(PolicyAction::Block);
    let capture_policy = filter.capture_policy.as_mut().expect("capture policy");
    capture_policy.allowed_apps = vec!["Cursor".to_owned()];
    capture_policy.browser.on_url_unavailable = PolicyAction::Allow;
    let (_, tracker) = chrome_eligibility_channel(filter.clone());
    let policy = CapturePolicy::new(tracker, filter, None);
    for (name, bundle_id) in [
        ("Google Chrome", Some("com.example.Chrome")),
        ("Google Chrome", None),
        ("Safari", Some("com.example.Safari")),
        ("Safari", None),
    ] {
        let app = App {
            name: name.to_owned(),
            bundle_id: bundle_id.map(str::to_owned),
            pid: Some(7),
        };
        assert!(
            !policy
                .decision(PrivacyScope::TextContent, &app, Some(11), None)
                .is_allowed(),
            "unrecognized {name} bundle must not bypass app-owned policy"
        );
    }
}

#[test]
fn title_policy_reload_and_read_deny_cannot_become_send_allow() {
    let policy = title_policy_capture(PolicyAction::Block);
    let cursor = app_named("Cursor");
    let earlier = policy.decision(PrivacyScope::TextContent, &cursor, Some(11), Some(".env"));
    assert!(!earlier.is_allowed());

    policy.replace_filter(title_policy_with(false, PolicyAction::Allow));
    assert!(
        policy
            .decision(PrivacyScope::TextContent, &cursor, Some(11), Some(".env"))
            .is_allowed()
    );
    assert!(
        !policy
            .decision_at_send(
                PrivacyScope::TextContent,
                &cursor,
                Some(11),
                Some(".env"),
                Some(&earlier),
            )
            .is_allowed()
    );
}

#[test]
fn chrome_tracker_and_unknown_field_rules_remain_independent_of_title_policy() {
    let filter = title_policy(PolicyAction::Block);
    let (publisher, tracker) = chrome_eligibility_channel(filter.clone());
    let policy = CapturePolicy::new(tracker, filter, None);
    let chrome = App {
        name: "Google Chrome".to_owned(),
        bundle_id: Some("com.google.Chrome".to_owned()),
        pid: Some(7),
    };
    publisher.observe(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(11),
            url: "https://example.com".to_owned(),
        },
    );
    assert!(
        policy
            .decision(PrivacyScope::TextContent, &chrome, Some(11), Some(".env"))
            .is_allowed()
    );

    let cursor = app_named("Cursor");
    assert!(
        !policy
            .input_decision(&cursor, Some(11), Some("main.rs"), None,)
            .is_allowed()
    );
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
    let policy = CapturePolicy::with_activity(
        tracker,
        config,
        Some(disconnected_probe()),
        FakeActivity(Ok(0.0)),
    );

    let allows = || {
        policy
            .decision(
                PrivacyScope::ContentSnapshot,
                &chrome.raw_app(),
                Some(11),
                None,
            )
            .is_allowed()
    };
    assert!(!allows());
    publisher.observe(
        7,
        ChromeEligibilityObservation::Incognito {
            window_id: Some(11),
        },
    );
    assert!(!allows());
    publisher.observe(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(11),
            url: "https://global.example/page".to_owned(),
        },
    );
    assert!(!allows());
    publisher.observe(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(11),
            url: "https://snapshot.example/page".to_owned(),
        },
    );
    assert!(!allows());
    publisher.observe(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(11),
            url: "https://public.example/page".to_owned(),
        },
    );
    assert!(allows());
    assert_eq!(
        policy
            .decision(
                PrivacyScope::ContentSnapshot,
                &chrome.raw_app(),
                Some(11),
                None,
            )
            .capture_context()
            .url
            .as_deref(),
        Some("https://public.example/page")
    );
}

#[test]
fn secure_input_enabled_timeout_and_disconnect_all_fail_closed() {
    assert!(!secure_input_decision(true));
    assert!(secure_input_decision(false));

    let (_publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let policy = CapturePolicy::with_activity(
        tracker,
        FilterConfig::default(),
        Some(disconnected_probe()),
        FakeActivity(Ok(0.0)),
    );
    assert!(!policy.secure_input_allows());

    let (_publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let (probe, responder) = secure_input_test_channel();
    let policy = CapturePolicy::with_activity(
        tracker,
        FilterConfig::default(),
        Some(probe),
        FakeActivity(Ok(0.0)),
    );
    assert!(!policy.secure_input_allows(), "unanswered probe times out");
    drop(responder);
}

#[test]
fn refresh_requires_input_within_its_own_interval_and_rejects_probe_errors() {
    for (activity, expected) in [(29.0, true), (30.0, true), (30.1, false)] {
        let (_publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
        let policy = CapturePolicy::with_activity(
            tracker,
            FilterConfig::default(),
            Some(disconnected_probe()),
            FakeActivity(Ok(activity)),
        );
        assert_eq!(
            policy.refresh_activity_allows(Some(Duration::from_secs(30))),
            expected
        );
    }

    let (_publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let policy = CapturePolicy::with_activity(
        tracker,
        FilterConfig::default(),
        Some(disconnected_probe()),
        FakeActivity(Err(ActivityError::Negative { seconds: -1.0 })),
    );
    assert!(!policy.refresh_activity_allows(Some(Duration::from_secs(30))));
    assert!(policy.refresh_activity_allows(None));
}
