//! Shared Chrome window eligibility used to authorize text capture.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use zanei_core::{config::FilterConfig, privacy::website_is_allowed};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChromeWindowEligibility {
    Normal,
    Incognito,
    ExcludedSite,
}

struct EligibilityState {
    filter: FilterConfig,
    windows: HashMap<(i32, i64), ChromeWindowEligibility>,
}

#[derive(Clone)]
pub struct ChromeEligibilityPublisher {
    state: Arc<RwLock<EligibilityState>>,
}

impl ChromeEligibilityPublisher {
    pub(crate) fn publish_normal(&self, pid: i64, window_id: Option<i64>, url: &str) {
        let (Ok(pid), Some(window_id)) = (i32::try_from(pid), window_id) else {
            self.clear_pid(pid);
            return;
        };
        if let Ok(mut state) = self.state.write() {
            state
                .windows
                .retain(|(window_pid, _), _| *window_pid != pid);
            let eligibility = if website_is_allowed(url, &state.filter) {
                ChromeWindowEligibility::Normal
            } else {
                ChromeWindowEligibility::ExcludedSite
            };
            state.windows.insert((pid, window_id), eligibility);
        }
    }

    pub(crate) fn publish_incognito(&self, pid: i64, window_id: Option<i64>) {
        let (Ok(pid), Some(window_id)) = (i32::try_from(pid), window_id) else {
            self.clear_pid(pid);
            return;
        };
        if let Ok(mut state) = self.state.write() {
            state
                .windows
                .retain(|(window_pid, _), _| *window_pid != pid);
            state
                .windows
                .insert((pid, window_id), ChromeWindowEligibility::Incognito);
        }
    }

    pub(crate) fn clear_pid(&self, pid: i64) {
        let Ok(pid) = i32::try_from(pid) else {
            return;
        };
        if let Ok(mut state) = self.state.write() {
            state
                .windows
                .retain(|(window_pid, _), _| *window_pid != pid);
        }
    }

    pub(crate) fn clear_all(&self) {
        if let Ok(mut state) = self.state.write() {
            state.windows.clear();
        }
    }

    pub fn replace_filter(&self, filter: FilterConfig) {
        if let Ok(mut state) = self.state.write() {
            state.filter = filter;
            state.windows.clear();
        }
    }
}

#[derive(Clone)]
pub struct ChromeEligibilityTracker {
    state: Arc<RwLock<EligibilityState>>,
}

impl ChromeEligibilityTracker {
    pub(crate) fn allows_text(&self, pid: i64, window_id: Option<i64>) -> bool {
        let (Ok(pid), Some(window_id)) = (i32::try_from(pid), window_id) else {
            return false;
        };
        self.state.read().is_ok_and(|state| {
            state.windows.get(&(pid, window_id)) == Some(&ChromeWindowEligibility::Normal)
        })
    }

    #[cfg(test)]
    fn eligibility(&self, pid: i64, window_id: i64) -> Option<ChromeWindowEligibility> {
        self.state
            .read()
            .ok()?
            .windows
            .get(&(i32::try_from(pid).ok()?, window_id))
            .copied()
    }
}

#[must_use]
pub fn chrome_eligibility_channel(
    filter: FilterConfig,
) -> (ChromeEligibilityPublisher, ChromeEligibilityTracker) {
    let state = Arc::new(RwLock::new(EligibilityState {
        filter,
        windows: HashMap::new(),
    }));
    (
        ChromeEligibilityPublisher {
            state: Arc::clone(&state),
        },
        ChromeEligibilityTracker { state },
    )
}

#[cfg(test)]
mod tests {
    use zanei_core::config::FilterConfig;

    use super::*;

    #[test]
    fn normal_incognito_and_excluded_sites_are_pid_and_window_scoped() {
        let (publisher, tracker) = chrome_eligibility_channel(FilterConfig {
            exclude_websites: vec!["private.example".to_owned()],
            ..FilterConfig::default()
        });

        publisher.publish_normal(7, Some(11), "https://example.com");
        assert!(tracker.allows_text(7, Some(11)));
        assert!(!tracker.allows_text(7, Some(12)));
        assert!(!tracker.allows_text(8, Some(11)));

        publisher.publish_incognito(7, Some(11));
        assert_eq!(
            tracker.eligibility(7, 11),
            Some(ChromeWindowEligibility::Incognito)
        );
        assert!(!tracker.allows_text(7, Some(11)));

        publisher.publish_normal(7, Some(11), "https://private.example/path");
        assert_eq!(
            tracker.eligibility(7, 11),
            Some(ChromeWindowEligibility::ExcludedSite)
        );
        assert!(!tracker.allows_text(7, Some(11)));
    }

    #[test]
    fn filter_replacement_invalidates_existing_decisions() {
        let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
        publisher.publish_normal(7, Some(11), "https://example.com");
        assert!(tracker.allows_text(7, Some(11)));

        publisher.replace_filter(FilterConfig {
            exclude_websites: vec!["example.com".to_owned()],
            ..FilterConfig::default()
        });

        assert!(!tracker.allows_text(7, Some(11)));
    }
}
