use serde::Deserialize;
use zanei_core::config::capture_policy::{BrowserMode, BrowserUrlRule, PolicyAction};
use zanei_core::config::{CapturePolicyConfig, Config, FilterConfig};
use zanei_core::normalize::NormalizedEvent;
use zanei_core::privacy::{
    CaptureDeniedReason as Reason, CapturePolicyDecision as Decision, PrivacyFilter,
    evaluate_capture_policy,
};
use zanei_core::schema::{App, CaptureContext, EmptyData, Event, EventData, Redaction};

#[derive(Deserialize)]
struct Fixtures {
    policy: CapturePolicyConfig,
    preset_cases: Vec<(Option<String>, String)>,
    ide_cases: Vec<(Option<String>, String)>,
}

fn fixtures() -> Fixtures {
    serde_json::from_str(include_str!("privacy_parity_cases.json")).expect("shared parity cases")
}

fn app(name: &str) -> App {
    App {
        name: name.to_owned(),
        bundle_id: None,
        pid: Some(1),
    }
}

fn check(actual: Decision, expected: &str, input: &str) {
    let expected = match expected {
        "Allow" => Decision::Allow,
        "AuthenticationUrl" => Decision::Deny(Reason::AuthenticationUrl),
        "PaymentUrl" => Decision::Deny(Reason::PaymentUrl),
        "UrlUnavailable" => Decision::Deny(Reason::UrlUnavailable),
        "EnvFile" => Decision::Deny(Reason::EnvFile),
        "FileNameUnavailable" => Decision::Deny(Reason::FileNameUnavailable),
        other => panic!("unknown fixture outcome: {other}"),
    };
    assert_eq!(actual, expected, "{input}");
}

fn rule(host: &str, path: &str, subdomains: bool) -> BrowserUrlRule {
    BrowserUrlRule {
        host: host.to_owned(),
        path_prefix: path.to_owned(),
        match_subdomains: subdomains,
    }
}

fn evaluate(policy: &CapturePolicyConfig, url: Option<&str>) -> Decision {
    evaluate_capture_policy(policy, &app("Google Chrome"), None, url)
}

#[test]
fn representative_presets_and_known_limits_are_shared_for_chrome_and_safari() {
    let fixture = fixtures();
    for browser in ["Google Chrome", "Safari"] {
        for (url, expected) in &fixture.preset_cases {
            check(
                evaluate_capture_policy(&fixture.policy, &app(browser), None, url.as_deref()),
                expected,
                url.as_deref().unwrap_or("unknown"),
            );
        }
    }
}

#[test]
fn ide_title_rules_handle_decorations_and_example_exceptions() {
    let fixture = fixtures();
    for editor in ["Cursor", "Visual Studio Code", "Code"] {
        for (title, expected) in &fixture.ide_cases {
            check(
                evaluate_capture_policy(&fixture.policy, &app(editor), title.as_deref(), None),
                expected,
                title.as_deref().unwrap_or("unknown"),
            );
        }
    }
    let mut policy = fixture.policy;
    policy.ide.on_file_name_unavailable = PolicyAction::Allow;
    assert_eq!(
        evaluate_capture_policy(&policy, &app("Cursor"), None, None),
        Decision::Allow
    );
    policy.ide.block_env_files = false;
    policy.ide.on_file_name_unavailable = PolicyAction::Block;
    for title in [None, Some(".env")] {
        assert_eq!(
            evaluate_capture_policy(&policy, &app("Cursor"), title, None),
            Decision::Allow
        );
    }
    let long_title = format!("{}.py", "a".repeat(253));
    policy.ide.block_env_files = true;
    assert_eq!(
        evaluate_capture_policy(&policy, &app("Cursor"), Some(&long_title), None),
        Decision::Deny(Reason::FileNameUnavailable)
    );
}

#[test]
fn app_allow_only_does_not_become_allow_all_and_cannot_override_protected_apps() {
    let mut policy = fixtures().policy;
    assert_eq!(
        evaluate_capture_policy(&policy, &app(" Notes "), None, None),
        Decision::Allow
    );
    assert_eq!(
        evaluate_capture_policy(&policy, &app("Other"), None, None),
        Decision::Deny(Reason::AppNotAllowed)
    );
    assert_eq!(
        evaluate_capture_policy(&policy, &app("1Password"), None, None),
        Decision::Deny(Reason::ProtectedApp)
    );
    let mut protected = app("Notes");
    protected.bundle_id = Some("com.apple.keychainaccess".to_owned());
    assert_eq!(
        evaluate_capture_policy(&policy, &protected, None, None),
        Decision::Deny(Reason::ProtectedApp)
    );
    policy.allowed_apps = Some(Vec::new());
    assert_eq!(
        evaluate_capture_policy(&policy, &app("Notes"), None, None),
        Decision::Deny(Reason::AppNotAllowed)
    );
}

#[test]
fn an_absent_allow_list_leaves_app_selection_to_the_filter_lists() {
    let mut policy = fixtures().policy;
    policy.allowed_apps = None;
    for name in ["Notes", "Slack", "Other"] {
        assert_eq!(
            evaluate_capture_policy(&policy, &app(name), None, None),
            Decision::Allow,
            "{name}"
        );
    }
    assert_eq!(
        evaluate_capture_policy(&policy, &app("1Password"), None, None),
        Decision::Deny(Reason::ProtectedApp)
    );
    // The browser and IDE rules still apply to the apps the filter lists admit.
    assert_eq!(
        evaluate_capture_policy(&policy, &app("Cursor"), Some(".env"), None),
        Decision::Deny(Reason::EnvFile)
    );
    assert_eq!(
        evaluate_capture_policy(&policy, &app("Google Chrome"), None, None),
        Decision::Deny(Reason::UrlUnavailable)
    );
}

#[test]
fn without_an_allow_list_exclude_and_include_only_apps_decide_what_is_recorded() {
    let policy = || {
        let mut policy = fixtures().policy;
        policy.allowed_apps = None;
        Some(policy)
    };
    let filter = |exclude: &[&str], include_only: &[&str]| FilterConfig {
        capture_policy: policy(),
        exclude_apps: exclude.iter().map(|name| (*name).to_owned()).collect(),
        include_only_apps: include_only.iter().map(|name| (*name).to_owned()).collect(),
        ..FilterConfig::default()
    };

    for (exclude, include_only, expected) in [
        (&[][..], &[][..], &[("Notes", true), ("Slack", true)][..]),
        (
            &["Slack"][..],
            &[][..],
            &[("Notes", true), ("Slack", false)][..],
        ),
        (
            &[][..],
            &["Notes"][..],
            &[("Notes", true), ("Slack", false)][..],
        ),
    ] {
        let privacy = PrivacyFilter::new(filter(exclude, include_only));
        for (name, recorded) in expected {
            assert_eq!(
                privacy.process(activation(name)).is_some(),
                *recorded,
                "{name} exclude={exclude:?} include_only={include_only:?}"
            );
        }
    }
    // An explicit allow list still overrides both lists exactly as before.
    let mut pinned = filter(&[], &[]);
    pinned.capture_policy.as_mut().expect("policy").allowed_apps = Some(vec!["Notes".to_owned()]);
    let privacy = PrivacyFilter::new(pinned);
    assert!(privacy.process(activation("Notes")).is_some());
    assert!(privacy.process(activation("Slack")).is_none());
}

#[test]
fn a_policy_without_an_allow_list_parses_and_is_serialized_without_the_key() {
    let mut config = Config::default();
    let mut policy = fixtures().policy;
    policy.allowed_apps = None;
    config.filter.capture_policy = Some(policy);
    config.validate().expect("no allow list to validate");

    let encoded = toml::to_string(&config).expect("generated config");
    assert!(!encoded.contains("allowed_apps"));
    assert_eq!(
        Config::from_toml(&encoded).expect("validated round trip"),
        config
    );
}

#[test]
fn host_subdomain_and_literal_path_prefix_rules_keep_the_existing_granularity() {
    let mut policy = fixtures().policy;
    policy.browser.mode = BrowserMode::Rules;
    policy.browser.allow_list = vec![rule("example.com", "/foo", false)];
    for (url, allowed) in [
        ("https://example.com/foo", true),
        ("https://example.com/foobar", true),
        ("https://example.com/Foo", false),
        ("https://sub.example.com/foo", false),
        ("https://evil-example.com/foo", false),
        ("https://example.com.evil/foo", false),
        ("https://EXAMPLE.com:8443/foo?q=secret#fragment", true),
        ("https://example.com./foo", false),
    ] {
        assert_eq!(
            evaluate(&policy, Some(url)) == Decision::Allow,
            allowed,
            "{url}"
        );
    }
    policy.browser.allow_list[0].match_subdomains = true;
    assert_eq!(
        evaluate(&policy, Some("https://deep.sub.example.com/foo")),
        Decision::Allow
    );
    policy.browser.block_list = vec![rule("sub.example.com", "/foo/private", true)];
    assert_eq!(
        evaluate(&policy, Some("https://sub.example.com/foo/private-note")),
        Decision::Deny(Reason::BlockedUrl)
    );
    for (host, url) in [
        ("xn--r8jz45g.xn--zckzah", "https://例え.テスト/"),
        ("[2001:db8::1]", "https://[2001:db8::1]/"),
    ] {
        policy.browser.allow_list = vec![rule(host, "", false)];
        assert_eq!(evaluate(&policy, Some(url)), Decision::Allow, "{url}");
    }
}

#[test]
fn all_sites_preserves_explicit_blocks_and_unknown_policy_even_with_presets_off() {
    let mut policy = fixtures().policy;
    policy.browser.block_auth = false;
    policy.browser.block_payments = false;
    policy.browser.block_list = vec![rule("example.com", "/private", false)];
    assert_eq!(
        evaluate(&policy, Some("https://example.com/private")),
        Decision::Deny(Reason::BlockedUrl)
    );
    assert_eq!(
        evaluate(&policy, None),
        Decision::Deny(Reason::UrlUnavailable)
    );
    policy.browser.on_url_unavailable = PolicyAction::Allow;
    for url in [None, Some("chrome://settings"), Some("not a URL")] {
        assert_eq!(evaluate(&policy, url), Decision::Allow);
    }
    policy.browser.mode = BrowserMode::Off;
    assert_eq!(evaluate(&policy, None), Decision::Deny(Reason::BrowserOff));
}

#[test]
fn block_list_and_presets_override_allow_rules_and_preset_toggles_are_independent() {
    let mut policy = fixtures().policy;
    policy.browser.mode = BrowserMode::Rules;
    policy.browser.allow_list = vec![rule("example.com", "", true)];
    let login = Some("https://example.com/login");
    policy.browser.block_list = vec![rule("example.com", "/login", false)];
    assert_eq!(evaluate(&policy, login), Decision::Deny(Reason::BlockedUrl));
    policy.browser.block_list.clear();
    assert_eq!(
        evaluate(&policy, login),
        Decision::Deny(Reason::AuthenticationUrl)
    );
    policy.browser.block_auth = false;
    assert_eq!(evaluate(&policy, login), Decision::Allow);
    assert_eq!(
        evaluate(&policy, Some("https://example.com/billing")),
        Decision::Deny(Reason::PaymentUrl)
    );
    policy.browser.block_payments = false;
    policy.browser.block_auth = true;
    assert_eq!(
        evaluate(&policy, Some("https://example.com/billing")),
        Decision::Allow
    );
    assert_eq!(
        evaluate(&policy, login),
        Decision::Deny(Reason::AuthenticationUrl)
    );
    assert_eq!(
        evaluate(&policy, Some("https://elsewhere.test/")),
        Decision::Deny(Reason::DefaultPolicy)
    );
    policy.browser.default_policy = PolicyAction::Allow;
    assert_eq!(
        evaluate(&policy, Some("https://elsewhere.test/")),
        Decision::Allow
    );
}

#[test]
fn generated_policy_round_trips_without_changing_standalone_defaults_or_serialization() {
    let standalone =
        Config::from_toml("[filter]\ninclude_only_apps = []").expect("standalone config");
    assert_eq!(standalone, Config::default());
    assert!(
        !toml::to_string(&standalone)
            .expect("serialize")
            .contains("capture_policy")
    );
    let mut config = standalone;
    config.filter.capture_policy = Some(fixtures().policy);
    let encoded = toml::to_string(&config).expect("generated config");
    assert_eq!(
        Config::from_toml(&encoded).expect("validated round trip"),
        config
    );
    for host in ["xn--r8jz45g.xn--zckzah", "[2001:db8::1]", "example.com."] {
        config
            .filter
            .capture_policy
            .as_mut()
            .unwrap()
            .browser
            .allow_list = vec![rule(host, "/", false)];
        config.validate().expect("canonical host");
    }
}

#[test]
fn incomplete_unknown_or_noncanonical_policy_never_silently_becomes_broad_allow() {
    for input in [
        "[filter.capture_policy]",
        "[filter.capture_policy]\nallowed_apps=[]\nunexpected=true",
    ] {
        assert!(Config::from_toml(input).is_err(), "{input}");
    }
    let base = serde_json::to_value(fixtures().policy).expect("fixture policy");
    for field in [
        "mode",
        "default_policy",
        "on_url_unavailable",
        "block_auth",
        "block_payments",
        "allow_list",
        "block_list",
    ] {
        let mut value = base.clone();
        value["browser"].as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<CapturePolicyConfig>(value).is_err(),
            "{field}"
        );
    }
    for (field, value) in [
        ("mode", "future"),
        ("on_url_unavailable", "ignore"),
        ("unexpected", "value"),
    ] {
        let mut policy = base.clone();
        policy["browser"][field] = value.into();
        assert!(
            serde_json::from_value::<CapturePolicyConfig>(policy).is_err(),
            "{field}"
        );
    }
    let mut config = Config::default();
    for apps in [vec![""], vec!["Notes", "notes"], vec![" Notes "]] {
        let mut policy = fixtures().policy;
        policy.allowed_apps = Some(apps.into_iter().map(str::to_owned).collect());
        config.filter.capture_policy = Some(policy);
        assert!(config.validate().is_err());
    }
    for (host, path) in [
        ("", "/"),
        ("EXAMPLE.com", "/"),
        ("例え.テスト", "/"),
        ("example.com:443", "/"),
        ("example.com/private", "/"),
        ("example.com", " /private"),
    ] {
        let mut policy = fixtures().policy;
        policy.browser.block_list = vec![rule(host, path, false)];
        config.filter.capture_policy = Some(policy);
        assert!(config.validate().is_err(), "{host} {path}");
    }
}

fn activation(app_name: &str) -> NormalizedEvent {
    NormalizedEvent::new(
        Event {
            version: 1,
            id: "evt_01J00000000000000000000000".to_owned(),
            ts: "2026-09-07T00:00:00.000Z".to_owned(),
            mono_ns: 1,
            source: "macos.workspace".to_owned(),
            event_type: "app.activate".to_owned(),
            app: App {
                name: app_name.to_owned(),
                bundle_id: None,
                pid: Some(1),
            },
            window: None,
            element: None,
            data: EventData::AppActivate(EmptyData {}),
            redaction: Redaction {
                applied: false,
                rules: Vec::new(),
            },
        },
        CaptureContext::default(),
    )
}
