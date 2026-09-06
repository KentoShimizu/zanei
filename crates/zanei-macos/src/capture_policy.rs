//! One capture-time policy for text and content-snapshot bodies.

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use zanei_core::{
    config::FilterConfig,
    privacy::{
        CHROME_BUNDLE_ID, CapturePolicyDecision, PrivacyScope, app_is_allowed_for,
        evaluate_capture_policy,
    },
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
        let is_chrome = app.bundle_id.as_deref() == Some(CHROME_BUNDLE_ID);
        let is_known_browser = BrowserTarget::from_bundle_id(app.bundle_id.as_deref()).is_some();
        let (chrome_allowed, capture_context, chrome_version) = if is_chrome {
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
                    is_known_browser
                        || matches!(
                            evaluate_capture_policy(policy, app, window_title, None),
                            CapturePolicyDecision::Allow
                        )
                })
        });
        CaptureDecision {
            allowed: app_allowed && chrome_allowed,
            capture_context,
            chrome_version,
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
    use crate::chrome::{ChromeEligibilityObservation, chrome_eligibility_channel};
    use std::{
        sync::{
            Barrier,
            atomic::{AtomicBool, Ordering},
        },
        thread,
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
}
