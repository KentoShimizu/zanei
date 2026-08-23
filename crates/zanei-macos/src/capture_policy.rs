//! One capture-time policy for text and content-snapshot bodies.

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use zanei_core::{
    config::FilterConfig,
    privacy::{CHROME_BUNDLE_ID, PrivacyScope, app_is_allowed_for},
    schema::{App, CaptureContext},
};

use crate::{
    SecureInputProbe,
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
        self.chrome.replace_filter(filter.clone());
        match self.filter.write() {
            Ok(mut current) => *current = filter,
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
    ) -> CaptureDecision {
        let is_chrome = app.bundle_id.as_deref() == Some(CHROME_BUNDLE_ID);
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
        let app_allowed = self
            .filter
            .read()
            .is_ok_and(|filter| app_is_allowed_for(scope, app, &filter));
        CaptureDecision {
            allowed: app_allowed && chrome_allowed,
            capture_context,
            chrome_version,
        }
    }

    #[must_use]
    pub(crate) fn input_decision(
        &self,
        app: &App,
        window_id: Option<i64>,
        focused_field: Option<FocusedField>,
    ) -> CaptureDecision {
        let mut decision = self.decision(PrivacyScope::TextContent, app, window_id);
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
