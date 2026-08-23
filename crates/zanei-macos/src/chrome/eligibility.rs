//! Shared Chrome window eligibility used to authorize text capture.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use zanei_core::{
    config::FilterConfig,
    privacy::{PrivacyScope, host_is_allowed_for, website_host},
    schema::CaptureContext,
};

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChromeWindowState {
    Normal { host: Option<String> },
    Incognito,
}

struct EligibilityState {
    filter: FilterConfig,
    windows: HashMap<(i32, i64), ChromeWindowState>,
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
            state.windows.insert(
                (pid, window_id),
                ChromeWindowState::Normal {
                    host: website_host(url),
                },
            );
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
                .insert((pid, window_id), ChromeWindowState::Incognito);
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
        match self.state.write() {
            Ok(mut state) => state.filter = filter,
            Err(_) => crate::trace::trace!(
                "component=chrome phase=eligibility action=replace_filter result=poisoned"
            ),
        }
    }
}

#[derive(Clone)]
pub struct ChromeEligibilityTracker {
    state: Arc<RwLock<EligibilityState>>,
}

impl ChromeEligibilityTracker {
    pub fn allows_url_events(&self, pid: i64, window_id: Option<i64>) -> bool {
        self.allows(PrivacyScope::AllEvents, pid, window_id)
    }

    pub fn allows_text(&self, pid: i64, window_id: Option<i64>) -> bool {
        self.allows(PrivacyScope::TextContent, pid, window_id)
    }

    pub fn allows_snapshot(&self, pid: i64, window_id: Option<i64>) -> bool {
        self.allows(PrivacyScope::ContentSnapshot, pid, window_id)
    }

    fn allows(&self, scope: PrivacyScope, pid: i64, window_id: Option<i64>) -> bool {
        let (Ok(pid), Some(window_id)) = (i32::try_from(pid), window_id) else {
            return false;
        };
        self.state.read().is_ok_and(|state| {
            let Some(ChromeWindowState::Normal { host }) = state.windows.get(&(pid, window_id))
            else {
                return false;
            };
            host_is_allowed_for(scope, host.as_deref(), &state.filter)
        })
    }

    pub(crate) fn capture_context(&self, pid: i64, window_id: Option<i64>) -> CaptureContext {
        let (Ok(pid), Some(window_id)) = (i32::try_from(pid), window_id) else {
            return CaptureContext::default();
        };
        self.state
            .read()
            .ok()
            .and_then(|state| {
                state
                    .windows
                    .get(&(pid, window_id))
                    .map(|window| CaptureContext {
                        website_host: match window {
                            ChromeWindowState::Normal { host } => host.clone(),
                            ChromeWindowState::Incognito => None,
                        },
                    })
            })
            .unwrap_or_default()
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
    use std::thread;

    use zanei_core::config::{FilterConfig, ScopedFilterConfig};

    use super::*;

    #[test]
    fn three_scopes_are_evaluated_from_the_current_filter() {
        let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());

        publisher.publish_normal(7, Some(11), "https://example.com");
        assert!(tracker.allows_url_events(7, Some(11)));
        assert!(tracker.allows_text(7, Some(11)));
        assert!(tracker.allows_snapshot(7, Some(11)));
        assert!(!tracker.allows_text(7, Some(12)));
        assert!(!tracker.allows_text(8, Some(11)));

        publisher.replace_filter(FilterConfig {
            exclude_websites: vec!["global.example".to_owned()],
            text_content: ScopedFilterConfig {
                exclude_websites: vec!["text.example".to_owned()],
                ..ScopedFilterConfig::default()
            },
            content_snapshot: ScopedFilterConfig {
                exclude_websites: vec!["snapshot.example".to_owned()],
                ..ScopedFilterConfig::default()
            },
            ..FilterConfig::default()
        });

        publisher.publish_normal(7, Some(11), "https://global.example");
        assert!(!tracker.allows_url_events(7, Some(11)));
        assert!(!tracker.allows_text(7, Some(11)));
        assert!(!tracker.allows_snapshot(7, Some(11)));

        publisher.publish_normal(7, Some(11), "https://text.example");
        assert!(tracker.allows_url_events(7, Some(11)));
        assert!(!tracker.allows_text(7, Some(11)));
        assert!(tracker.allows_snapshot(7, Some(11)));

        publisher.publish_normal(7, Some(11), "https://snapshot.example");
        assert!(tracker.allows_url_events(7, Some(11)));
        assert!(tracker.allows_text(7, Some(11)));
        assert!(!tracker.allows_snapshot(7, Some(11)));
    }

    #[test]
    fn unknown_incognito_and_hostless_windows_fail_closed() {
        let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());

        assert!(!tracker.allows_url_events(7, Some(11)));
        assert!(!tracker.allows_text(7, Some(11)));
        assert!(!tracker.allows_snapshot(7, Some(11)));

        publisher.publish_incognito(7, Some(11));
        assert!(!tracker.allows_url_events(7, Some(11)));
        assert!(!tracker.allows_text(7, Some(11)));
        assert!(!tracker.allows_snapshot(7, Some(11)));

        publisher.publish_normal(7, Some(11), "about:blank");
        assert!(!tracker.allows_url_events(7, Some(11)));
        assert!(!tracker.allows_text(7, Some(11)));
        assert!(!tracker.allows_snapshot(7, Some(11)));
        assert_eq!(tracker.capture_context(7, Some(11)).website_host, None);
    }

    #[test]
    fn filter_replacement_rechecks_the_preserved_host() {
        let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
        publisher.publish_normal(7, Some(11), "https://example.com");
        assert!(tracker.allows_text(7, Some(11)));

        publisher.replace_filter(FilterConfig {
            text_content: ScopedFilterConfig {
                exclude_websites: vec!["example.com".to_owned()],
                ..ScopedFilterConfig::default()
            },
            ..FilterConfig::default()
        });

        assert!(tracker.allows_url_events(7, Some(11)));
        assert!(!tracker.allows_text(7, Some(11)));
        assert!(tracker.allows_snapshot(7, Some(11)));
        assert_eq!(
            tracker.capture_context(7, Some(11)).website_host.as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn poisoned_state_fails_closed_for_all_decisions() {
        let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
        publisher.publish_normal(7, Some(11), "https://example.com");
        let state = Arc::clone(&tracker.state);
        let _ = thread::spawn(move || {
            let _guard = state.write().expect("eligibility write lock");
            panic!("poison eligibility state");
        })
        .join();

        assert!(!tracker.allows_url_events(7, Some(11)));
        assert!(!tracker.allows_text(7, Some(11)));
        assert!(!tracker.allows_snapshot(7, Some(11)));
    }
}
