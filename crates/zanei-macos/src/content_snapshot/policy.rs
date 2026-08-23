//! Capture gates that never construct or read Accessibility objects.

use std::time::Duration;

use zanei_core::{
    config::FilterConfig,
    privacy::{CHROME_BUNDLE_ID, PrivacyScope, app_is_allowed_for},
    schema::{App, CaptureContext},
};

use crate::{
    SecureInputProbe,
    chrome::ChromeEligibilityTracker,
    ffi::activity::{ActivityError, seconds_since_last_input},
    workspace::ApplicationInfo,
};

pub(crate) trait ActivityProbe: Send + Sync + 'static {
    fn seconds_since_last_input(&self) -> Result<f64, ActivityError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemActivityProbe;

impl ActivityProbe for SystemActivityProbe {
    fn seconds_since_last_input(&self) -> Result<f64, ActivityError> {
        seconds_since_last_input()
    }
}

pub(crate) struct SnapshotPolicy<A = SystemActivityProbe> {
    filter: FilterConfig,
    chrome: ChromeEligibilityTracker,
    secure_input: SecureInputProbe,
    activity: A,
}

impl SnapshotPolicy<SystemActivityProbe> {
    pub(crate) fn new(
        filter: FilterConfig,
        chrome: ChromeEligibilityTracker,
        secure_input: SecureInputProbe,
    ) -> Self {
        Self {
            filter,
            chrome,
            secure_input,
            activity: SystemActivityProbe,
        }
    }
}

impl<A: ActivityProbe> SnapshotPolicy<A> {
    #[cfg(test)]
    pub(crate) fn with_activity(
        filter: FilterConfig,
        chrome: ChromeEligibilityTracker,
        secure_input: SecureInputProbe,
        activity: A,
    ) -> Self {
        Self {
            filter,
            chrome,
            secure_input,
            activity,
        }
    }

    pub(crate) fn app_allows(&self, app: &ApplicationInfo) -> bool {
        app_is_allowed_for(PrivacyScope::ContentSnapshot, &raw_app(app), &self.filter)
    }

    pub(crate) fn chrome_allows(&self, app: &ApplicationInfo, window_id: i64) -> bool {
        app.bundle_id.as_deref() != Some(CHROME_BUNDLE_ID)
            || self.chrome.allows_snapshot(app.pid, Some(window_id))
    }

    pub(crate) fn secure_input_allows(&self) -> bool {
        matches!(self.secure_input.enabled(), Ok(false))
    }

    pub(crate) fn refresh_activity_allows(&self, interval: Option<Duration>) -> bool {
        let Some(interval) = interval else {
            return true;
        };
        self.activity
            .seconds_since_last_input()
            .is_ok_and(|seconds| seconds <= interval.as_secs_f64())
    }

    pub(crate) fn capture_context(&self, app: &ApplicationInfo, window_id: i64) -> CaptureContext {
        if app.bundle_id.as_deref() == Some(CHROME_BUNDLE_ID) {
            self.chrome.capture_context(app.pid, Some(window_id))
        } else {
            CaptureContext::default()
        }
    }

    pub(crate) fn replace_filter(&mut self, filter: FilterConfig) {
        self.filter = filter;
    }
}

fn raw_app(app: &ApplicationInfo) -> App {
    App {
        name: app.name.clone(),
        bundle_id: app.bundle_id.clone(),
        pid: Some(app.pid),
    }
}
