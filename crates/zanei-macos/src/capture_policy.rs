//! One capture-time policy for text and content-snapshot bodies.

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use zanei_core::{
    config::FilterConfig,
    privacy::{CapturePolicyDecision, PrivacyScope, app_is_allowed_for, evaluate_capture_policy},
    schema::{App, CaptureContext},
};

use crate::{
    SecureInputProbe,
    browser_context::BrowserTarget,
    chrome::ChromeEligibilityTracker,
    ffi::activity::{ActivityError, seconds_since_last_input},
    focused_field::FocusedField,
};

/// A capture-time decision made before an optional body crosses the collector boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureDecision {
    allowed: bool,
    capture_context: CaptureContext,
    chrome_version: Option<u64>,
}

impl CaptureDecision {
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        self.allowed
    }

    #[must_use]
    pub fn capture_context(&self) -> CaptureContext {
        self.capture_context.clone()
    }

    #[must_use]
    pub const fn chrome_version(&self) -> Option<u64> {
        self.chrome_version
    }
}

pub(crate) trait ActivityProbe: Send + Sync + 'static {
    fn seconds_since_last_input(&self) -> Result<f64, ActivityError>;
}

#[derive(Clone, Copy, Debug, Default)]
struct SystemActivityProbe;

impl ActivityProbe for SystemActivityProbe {
    fn seconds_since_last_input(&self) -> Result<f64, ActivityError> {
        seconds_since_last_input()
    }
}

/// Shared, hot-reloadable policy for all optional captured bodies.
#[derive(Clone)]
pub struct CapturePolicy {
    chrome: ChromeEligibilityTracker,
    filter: Arc<RwLock<FilterConfig>>,
    secure_input: Option<SecureInputProbe>,
    activity: Arc<dyn ActivityProbe>,
}

impl CapturePolicy {
    #[must_use]
    pub fn new(
        chrome: ChromeEligibilityTracker,
        filter: FilterConfig,
        secure_input: Option<SecureInputProbe>,
    ) -> Self {
        Self {
            chrome,
            filter: Arc::new(RwLock::new(filter)),
            secure_input,
            activity: Arc::new(SystemActivityProbe),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_activity(
        chrome: ChromeEligibilityTracker,
        filter: FilterConfig,
        secure_input: Option<SecureInputProbe>,
        activity: impl ActivityProbe,
    ) -> Self {
        Self {
            chrome,
            filter: Arc::new(RwLock::new(filter)),
            secure_input,
            activity: Arc::new(activity),
        }
    }

    pub fn replace_filter(&self, filter: FilterConfig) {
        match self.filter.write() {
            Ok(mut current) => {
                self.chrome.replace_filter(filter.clone());
                *current = filter;
            }
            Err(_) => crate::trace::trace!(
                "component=capture_policy action=replace_filter result=poisoned"
            ),
        }
    }

    #[must_use]
    pub fn decision(
        &self,
        scope: PrivacyScope,
        app: &App,
        window_id: Option<i64>,
        window_title: Option<&str>,
    ) -> CaptureDecision {
        let filter = self.filter.read().ok();
        let browser_target = BrowserTarget::from_bundle_id(app.bundle_id.as_deref());
        let uses_browser_tracker = browser_target.is_some_and(|target| {
            target == BrowserTarget::Chrome
                || filter
                    .as_deref()
                    .is_some_and(|filter| filter.capture_policy.is_some())
        });
        let (browser_allowed, capture_context, browser_version) = if uses_browser_tracker {
            app.pid.map_or_else(
                || (false, CaptureContext::default(), None),
                |pid| {
                    let decision = self.chrome.decision(scope, pid, window_id);
                    (
                        decision.is_allowed(),
                        decision.capture_context(),
                        decision.version(),
                    )
                },
            )
        } else {
            (true, CaptureContext::default(), None)
        };
        let app_allowed = filter.as_deref().is_some_and(|filter| {
            app_is_allowed_for(scope, app, filter)
                && filter.capture_policy.as_ref().is_none_or(|policy| {
                    browser_target.is_some()
                        || matches!(
                            evaluate_capture_policy(policy, app, window_title, None),
                            CapturePolicyDecision::Allow
                        )
                })
        });
        CaptureDecision {
            allowed: app_allowed && browser_allowed,
            capture_context,
            chrome_version: browser_version,
        }
    }

    /// Re-evaluates the current policy before delivery. An earlier decision may only
    /// tighten access and owns the original context/version bound to confirmation.
    #[must_use]
    pub(crate) fn decision_at_send(
        &self,
        scope: PrivacyScope,
        app: &App,
        window_id: Option<i64>,
        window_title: Option<&str>,
        earlier: Option<&CaptureDecision>,
    ) -> CaptureDecision {
        let mut current = self.decision(scope, app, window_id, window_title);
        if let Some(earlier) = earlier {
            current.allowed &= earlier.allowed;
            current.allowed &=
                !(current.chrome_version.is_some() && earlier.chrome_version.is_none());
            current.chrome_version = earlier.chrome_version;
            current.capture_context = earlier.capture_context();
        }
        current
    }

    #[must_use]
    pub(crate) fn input_decision(
        &self,
        app: &App,
        window_id: Option<i64>,
        window_title: Option<&str>,
        focused_field: Option<FocusedField>,
    ) -> CaptureDecision {
        let mut decision = self.decision(PrivacyScope::TextContent, app, window_id, window_title);
        decision.allowed &= focused_field.is_some_and(|field| field.class.is_known_text());
        decision
    }

    #[must_use]
    pub fn secure_input_allows(&self) -> bool {
        self.secure_input
            .as_ref()
            .is_some_and(|probe| matches!(probe.enabled(), Ok(false)))
    }

    #[must_use]
    pub fn refresh_activity_allows(&self, interval: Option<Duration>) -> bool {
        let Some(interval) = interval else {
            return true;
        };
        self.activity
            .seconds_since_last_input()
            .is_ok_and(|seconds| seconds <= interval.as_secs_f64())
    }

    #[must_use]
    pub(crate) fn chrome_tracker(&self) -> ChromeEligibilityTracker {
        self.chrome.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chrome::{ChromeEligibilityObservation, chrome_eligibility_channel},
        permission::SAFARI_BUNDLE_ID,
    };
    use std::{
        sync::{
            Barrier,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };
    use zanei_core::{
        config::{
            CapturePolicyConfig,
            capture_policy::{BrowserMode, BrowserPolicy, BrowserUrlRule, IdePolicy, PolicyAction},
        },
        privacy::CHROME_BUNDLE_ID,
    };

    #[test]
    fn concurrent_reload_never_combines_old_app_allow_with_new_host_allow() {
        let app = App {
            name: "Google Chrome".to_owned(),
            bundle_id: Some(CHROME_BUNDLE_ID.to_owned()),
            pid: Some(7),
        };
        let old = FilterConfig {
            exclude_websites: vec!["example.com".to_owned()],
            ..Default::default()
        };
        let new = FilterConfig {
            exclude_apps: vec![CHROME_BUNDLE_ID.to_owned()],
            ..Default::default()
        };
        let (publisher, chrome) = chrome_eligibility_channel(old.clone());
        publisher.observe(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: "https://example.com".to_owned(),
            },
        );
        let policy = CapturePolicy::new(chrome, old.clone(), None);
        assert!(
            !policy
                .decision(PrivacyScope::TextContent, &app, Some(11), None)
                .is_allowed()
        );
        policy.replace_filter(new.clone());
        assert!(
            !policy
                .decision(PrivacyScope::TextContent, &app, Some(11), None)
                .is_allowed()
        );
        let barrier = Arc::new(Barrier::new(2));
        let done = Arc::new(AtomicBool::new(false));
        let writer_policy = policy.clone();
        let writer_barrier = Arc::clone(&barrier);
        let writer_done = Arc::clone(&done);
        let writer = thread::spawn(move || {
            writer_barrier.wait();
            for _ in 0..2_000 {
                writer_policy.replace_filter(old.clone());
                writer_policy.replace_filter(new.clone());
            }
            writer_done.store(true, Ordering::Release);
        });
        barrier.wait();
        let mut mixed_allow = false;
        while !done.load(Ordering::Acquire) {
            mixed_allow |= policy
                .decision(PrivacyScope::TextContent, &app, Some(11), None)
                .is_allowed();
        }
        writer.join().expect("reload writer");
        assert!(
            !mixed_allow,
            "both complete policy revisions deny this body"
        );
    }

    #[test]
    fn app_owned_safari_uses_tracker_for_both_body_scopes() {
        for scope in [PrivacyScope::TextContent, PrivacyScope::ContentSnapshot] {
            let filter = safari_filter(BrowserMode::AllSites, PolicyAction::Block);
            let (publisher, tracker) = chrome_eligibility_channel(filter.clone());
            let policy = CapturePolicy::new(tracker, filter, None);
            let decide = || policy.decision(scope, &safari_app(), Some(11), None);

            assert!(!decide().is_allowed(), "{scope:?}: unobserved");
            publisher.observe(
                7,
                ChromeEligibilityObservation::Safari {
                    window_id: Some(11),
                    url: Some("https://allowed.example/path".to_owned()),
                },
            );
            let allowed = decide();
            assert!(allowed.is_allowed(), "{scope:?}: fresh URL");
            assert!(allowed.chrome_version().is_some());
            assert_eq!(
                allowed.capture_context().url.as_deref(),
                Some("https://allowed.example/path")
            );

            publisher.observe(
                7,
                ChromeEligibilityObservation::Safari {
                    window_id: Some(11),
                    url: Some("https://denied.example/path".to_owned()),
                },
            );
            assert!(!decide().is_allowed(), "{scope:?}: blocked URL");
            publisher.observe(
                7,
                ChromeEligibilityObservation::Safari {
                    window_id: Some(11),
                    url: None,
                },
            );
            assert!(!decide().is_allowed(), "{scope:?}: URL unavailable");
            publisher.observe(
                7,
                ChromeEligibilityObservation::Unavailable {
                    window_id: Some(11),
                },
            );
            assert!(!decide().is_allowed(), "{scope:?}: unavailable observation");
        }
    }

    #[test]
    fn app_owned_safari_respects_url_unavailable_off_and_app_scopes() {
        for scope in [PrivacyScope::TextContent, PrivacyScope::ContentSnapshot] {
            let filter = safari_filter(BrowserMode::AllSites, PolicyAction::Allow);
            let (publisher, tracker) = chrome_eligibility_channel(filter.clone());
            let policy = CapturePolicy::new(tracker, filter, None);
            publisher.observe(
                7,
                ChromeEligibilityObservation::Safari {
                    window_id: Some(11),
                    url: None,
                },
            );
            assert!(
                policy
                    .decision(scope, &safari_app(), Some(11), None)
                    .is_allowed(),
                "{scope:?}: configured URL-unavailable allow"
            );

            let off = safari_filter(BrowserMode::Off, PolicyAction::Allow);
            let mut global_exclude = safari_filter(BrowserMode::AllSites, PolicyAction::Allow);
            global_exclude
                .exclude_apps
                .push(SAFARI_BUNDLE_ID.to_owned());
            let mut global_include = safari_filter(BrowserMode::AllSites, PolicyAction::Allow);
            global_include
                .include_only_apps
                .push("dev.example.Other".to_owned());
            let mut scoped_exclude = safari_filter(BrowserMode::AllSites, PolicyAction::Allow);
            let mut scoped_include = safari_filter(BrowserMode::AllSites, PolicyAction::Allow);
            match scope {
                PrivacyScope::TextContent => {
                    scoped_exclude
                        .text_content
                        .exclude_apps
                        .push(SAFARI_BUNDLE_ID.to_owned());
                    scoped_include
                        .text_content
                        .include_only_apps
                        .push("dev.example.Other".to_owned());
                }
                PrivacyScope::ContentSnapshot => {
                    scoped_exclude
                        .content_snapshot
                        .exclude_apps
                        .push(SAFARI_BUNDLE_ID.to_owned());
                    scoped_include
                        .content_snapshot
                        .include_only_apps
                        .push("dev.example.Other".to_owned());
                }
                PrivacyScope::AllEvents => unreachable!(),
            }
            for (case, denied) in [
                ("off", off),
                ("global exclude", global_exclude),
                ("global include", global_include),
                ("scoped exclude", scoped_exclude),
                ("scoped include", scoped_include),
            ] {
                policy.replace_filter(denied);
                publisher.observe(
                    7,
                    ChromeEligibilityObservation::Safari {
                        window_id: Some(11),
                        url: Some("https://allowed.example/path".to_owned()),
                    },
                );
                assert!(
                    !policy
                        .decision(scope, &safari_app(), Some(11), None)
                        .is_allowed(),
                    "{scope:?}: {case}"
                );
            }
        }
    }

    #[test]
    fn safari_send_revalidation_does_not_cross_confirmation_modes() {
        let mut standalone = FilterConfig::default();
        standalone.text_content.exclude_apps.clear();
        let (publisher, tracker) = chrome_eligibility_channel(standalone.clone());
        let policy = CapturePolicy::new(tracker, standalone, None);
        let generic = policy.decision(PrivacyScope::TextContent, &safari_app(), Some(11), None);
        assert!(generic.is_allowed());
        assert_eq!(generic.chrome_version(), None);

        policy.replace_filter(safari_filter(BrowserMode::AllSites, PolicyAction::Allow));
        publisher.observe(
            7,
            ChromeEligibilityObservation::Safari {
                window_id: Some(11),
                url: Some("https://allowed.example/path".to_owned()),
            },
        );
        let current = policy.decision(PrivacyScope::TextContent, &safari_app(), Some(11), None);
        assert!(current.is_allowed());
        assert!(current.chrome_version().is_some());
        let at_send = policy.decision_at_send(
            PrivacyScope::TextContent,
            &safari_app(),
            Some(11),
            None,
            Some(&generic),
        );
        assert!(!at_send.is_allowed());
        assert_eq!(at_send.chrome_version(), None);

        let app_owned = current;
        let mut standalone = FilterConfig::default();
        standalone.text_content.exclude_apps.clear();
        policy.replace_filter(standalone);
        let at_send = policy.decision_at_send(
            PrivacyScope::TextContent,
            &safari_app(),
            Some(11),
            None,
            Some(&app_owned),
        );
        assert!(at_send.is_allowed());
        assert_eq!(at_send.chrome_version(), app_owned.chrome_version());
        assert_eq!(at_send.capture_context(), app_owned.capture_context());
    }

    fn safari_app() -> App {
        App {
            name: "Safari".to_owned(),
            bundle_id: Some(SAFARI_BUNDLE_ID.to_owned()),
            pid: Some(7),
        }
    }

    fn safari_filter(mode: BrowserMode, on_url_unavailable: PolicyAction) -> FilterConfig {
        let mut filter = FilterConfig {
            capture_policy: Some(CapturePolicyConfig {
                allowed_apps: vec!["Safari".to_owned()],
                browser: BrowserPolicy {
                    mode,
                    default_policy: PolicyAction::Allow,
                    on_url_unavailable,
                    block_auth: false,
                    block_payments: false,
                    allow_list: Vec::new(),
                    block_list: vec![BrowserUrlRule {
                        host: "denied.example".to_owned(),
                        path_prefix: "/".to_owned(),
                        match_subdomains: true,
                    }],
                },
                ide: IdePolicy {
                    block_env_files: false,
                    on_file_name_unavailable: PolicyAction::Allow,
                },
            }),
            exclude_apps: Vec::new(),
            ..FilterConfig::default()
        };
        filter.text_content.exclude_apps.clear();
        filter.content_snapshot.exclude_apps.clear();
        filter
    }
}
