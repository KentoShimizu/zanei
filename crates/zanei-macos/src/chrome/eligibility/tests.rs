use zanei_core::config::capture_policy::{
    BrowserMode, BrowserPolicy, BrowserUrlRule, CapturePolicyConfig, IdePolicy, PolicyAction,
};
use zanei_core::config::{FilterConfig, ScopedFilterConfig};

use super::*;

fn normal(window_id: i64, url: &str) -> ChromeEligibilityObservation {
    ChromeEligibilityObservation::Normal {
        window_id: Some(window_id),
        url: url.to_owned(),
    }
}

#[test]
fn unchanged_observation_preserves_version_but_window_identity_change_advances_it() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let first_observation = Instant::now();
    publisher.observe_with_window_id_at(
        7,
        normal(11, "https://example.com"),
        Some(AppleScriptWindowId::for_test("window-a")),
        first_observation,
    );
    let version = tracker.state_version(7, 11).expect("version");

    let confirmation = first_observation + std::time::Duration::from_millis(1);
    publisher.observe_at(7, normal(11, "https://example.com"), confirmation);

    assert_eq!(tracker.state_version(7, 11), Some(version));
    assert_eq!(tracker.observed_at(7, 11), Some(confirmation));

    publisher.observe_with_window_id_at(
        7,
        normal(11, "https://example.com"),
        Some(AppleScriptWindowId::for_test("window-b")),
        confirmation + std::time::Duration::from_millis(1),
    );

    assert!(
        tracker
            .state_version(7, 11)
            .is_some_and(|next| next > version)
    );
}

#[test]
fn ownership_unavailable_reobservation_preserves_version() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let initial = Instant::now();
    publisher.observe_at(7, normal(11, "https://example.com"), initial);
    let normal_version = tracker.state_version(7, 11).expect("normal version");
    publisher.observe_at(
        7,
        ChromeEligibilityObservation::Unavailable {
            window_id: Some(11),
        },
        initial + std::time::Duration::from_millis(1),
    );
    let repeated = initial + std::time::Duration::from_millis(2);
    publisher.observe_at(
        7,
        ChromeEligibilityObservation::Unavailable {
            window_id: Some(11),
        },
        repeated,
    );
    assert_eq!(tracker.observed_at(7, 11), Some(repeated));

    publisher.observe_at(
        7,
        normal(11, "https://example.com"),
        initial + std::time::Duration::from_millis(3),
    );

    assert_eq!(tracker.state_version(7, 11), Some(normal_version + 2));
}

#[test]
fn host_mode_change_and_disappearance_advance_version() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    publisher.observe(7, normal(11, "https://example.com"));
    let normal_version = tracker.state_version(7, 11).expect("normal version");

    publisher.observe(
        7,
        ChromeEligibilityObservation::Incognito {
            window_id: Some(11),
        },
    );
    let incognito_version = tracker.state_version(7, 11).expect("incognito version");
    assert!(incognito_version > normal_version);
    assert!(!tracker.allows_text(7, Some(11)));

    publisher.observe(
        7,
        ChromeEligibilityObservation::Unavailable { window_id: None },
    );
    assert_eq!(tracker.state_version(7, 11), None);
}

#[test]
fn observing_another_window_preserves_prior_window_until_targeted_unavailable() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    publisher.observe(7, normal(11, "https://first.example"));
    let first_version = tracker.state_version(7, 11);

    publisher.observe(7, normal(12, "https://second.example"));

    assert_eq!(tracker.state_version(7, 11), first_version);
    assert!(tracker.allows_snapshot(7, Some(11)));
    assert!(tracker.allows_snapshot(7, Some(12)));

    publisher.observe(
        7,
        ChromeEligibilityObservation::Unavailable {
            window_id: Some(11),
        },
    );

    assert_eq!(tracker.state_version(7, 11), None);
    assert!(tracker.allows_snapshot(7, Some(12)));
}

#[test]
fn filter_replacement_rechecks_preserved_host_without_changing_version() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    publisher.observe(7, normal(11, "https://example.com"));
    let version = tracker.state_version(7, 11);

    tracker.replace_filter(FilterConfig {
        text_content: ScopedFilterConfig {
            exclude_websites: vec!["example.com".to_owned()],
            ..ScopedFilterConfig::default()
        },
        ..FilterConfig::default()
    });

    assert_eq!(tracker.state_version(7, 11), version);
    assert!(!tracker.allows_text(7, Some(11)));
}

#[test]
fn unknown_incognito_and_hostless_windows_fail_closed() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    assert!(!tracker.allows_text(7, Some(11)));

    publisher.observe(
        7,
        ChromeEligibilityObservation::Incognito {
            window_id: Some(11),
        },
    );
    assert!(!tracker.allows_text(7, Some(11)));

    publisher.observe(7, normal(11, "about:blank"));
    assert!(!tracker.allows_text(7, Some(11)));
}

#[test]
fn long_urls_keep_one_shared_buffer_across_capture_decisions() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let url = format!("https://example.com/{}", "a".repeat(128 * 1024));
    publisher.observe(7, normal(11, &url));
    let first = tracker.decision(PrivacyScope::TextContent, 7, Some(11));
    let original = first.capture_context().url.unwrap();
    assert_eq!(&*original, url);
    publisher.observe(7, normal(11, &url));
    for _ in 0..100 {
        let next = tracker.decision(PrivacyScope::TextContent, 7, Some(11));
        assert!(next.is_allowed());
        assert!(Arc::ptr_eq(&original, &next.capture_context().url.unwrap()));
    }
}

#[test]
fn decision_returns_allow_context_and_version_from_one_record() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    publisher.observe(7, normal(11, "https://example.com/path"));

    let decision = tracker.decision(PrivacyScope::TextContent, 7, Some(11));

    assert!(decision.is_allowed());
    assert_eq!(
        decision.capture_context().url.as_deref(),
        Some("https://example.com/path")
    );
    assert!(decision.version().is_some());
}

#[test]
fn safari_keeps_optional_url_and_never_invents_tab_identity() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    publisher.observe_with_surface_at(
        7,
        ChromeEligibilityObservation::Safari {
            window_id: Some(11),
            url: Some("https://example.com/path".to_owned()),
        },
        Some(AppleScriptWindowId::for_test("safari-window")),
        Some("must-not-be-used"),
        Instant::now(),
    );

    let decision = tracker.decision(PrivacyScope::AllEvents, 7, Some(11));
    let context = decision.capture_context();
    assert_eq!(context.url.as_deref(), Some("https://example.com/path"));
    assert_eq!(
        context
            .surface
            .as_ref()
            .and_then(|surface| surface.tab_id.as_deref()),
        None
    );
    assert_eq!(
        context
            .surface
            .as_ref()
            .and_then(|surface| surface.applescript_window_id.as_deref()),
        Some("safari-window")
    );

    publisher.observe(
        7,
        ChromeEligibilityObservation::Safari {
            window_id: Some(12),
            url: None,
        },
    );
    let unknown = tracker.decision(PrivacyScope::AllEvents, 7, Some(12));
    assert!(!unknown.is_allowed());
    assert!(unknown.capture_context().url.is_none());
}

fn safari_policy(on_url_unavailable: PolicyAction) -> FilterConfig {
    FilterConfig {
        capture_policy: Some(CapturePolicyConfig {
            allowed_apps: vec!["Safari".to_owned()],
            browser: BrowserPolicy {
                mode: BrowserMode::Rules,
                default_policy: PolicyAction::Allow,
                on_url_unavailable,
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
    }
}

#[test]
fn safari_url_unknown_uses_app_policy_and_standalone_safari_is_denied() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    publisher.observe(
        7,
        ChromeEligibilityObservation::Safari {
            window_id: Some(11),
            url: Some("https://example.com".to_owned()),
        },
    );
    assert!(!tracker.allows_url_events(7, Some(11)));

    tracker.replace_filter(safari_policy(PolicyAction::Allow));
    publisher.observe(
        7,
        ChromeEligibilityObservation::Safari {
            window_id: Some(11),
            url: None,
        },
    );
    assert!(!tracker.allows_url_events(7, Some(11)));
    assert!(tracker.allows_text(7, Some(11)));
    assert!(tracker.allows_snapshot(7, Some(11)));

    tracker.replace_filter(safari_policy(PolicyAction::Block));
    publisher.observe(
        7,
        ChromeEligibilityObservation::Safari {
            window_id: Some(11),
            url: None,
        },
    );
    assert!(!tracker.allows_url_events(7, Some(11)));
    assert!(!tracker.allows_text(7, Some(11)));
    assert!(!tracker.allows_snapshot(7, Some(11)));
}

#[test]
fn app_owned_chrome_uses_block_list_default_and_allow_list_after_reobservation() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    publisher.observe(7, normal(11, "https://blocked.example/"));
    let mut filter = safari_policy(PolicyAction::Allow);
    let policy = filter.capture_policy.as_mut().expect("capture policy");
    policy.allowed_apps = vec!["Google Chrome".to_owned()];
    policy.browser.default_policy = PolicyAction::Block;
    policy.browser.allow_list = vec![BrowserUrlRule {
        host: "allowed.example".to_owned(),
        path_prefix: "/docs".to_owned(),
        match_subdomains: false,
    }];
    policy.browser.block_list = vec![BrowserUrlRule {
        host: "blocked.example".to_owned(),
        path_prefix: "/".to_owned(),
        match_subdomains: false,
    }];

    tracker.replace_filter(filter);
    assert!(tracker.state_version(7, 11).is_none());

    publisher.observe(7, normal(11, "https://blocked.example/"));
    assert!(!tracker.allows_url_events(7, Some(11)));
    assert!(!tracker.allows_text(7, Some(11)));
    assert!(!tracker.allows_snapshot(7, Some(11)));

    publisher.observe(7, normal(11, "https://other.example/"));
    assert!(!tracker.allows_url_events(7, Some(11)));

    publisher.observe(7, normal(11, "https://allowed.example/docs/start"));
    assert!(tracker.allows_url_events(7, Some(11)));
    assert!(tracker.allows_text(7, Some(11)));
    assert!(tracker.allows_snapshot(7, Some(11)));
}

#[test]
fn changing_app_owned_policy_invalidates_existing_chrome_observation() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    publisher.observe(7, normal(11, "https://example.com"));
    let version = tracker.state_version(7, 11).expect("version");

    tracker.replace_filter(safari_policy(PolicyAction::Allow));

    assert!(tracker.state_version(7, 11).is_none());
    assert!(!tracker.allows_url_events(7, Some(11)));
    assert!(version > 0);
}
