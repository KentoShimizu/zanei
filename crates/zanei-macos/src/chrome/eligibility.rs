//! Versioned Chrome window eligibility shared by text and snapshot capture.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};

use zanei_core::{
    config::FilterConfig,
    privacy::{PrivacyScope, host_is_allowed_for, website_host},
    schema::CaptureContext,
};

pub(crate) const CHROME_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChromeWindowState {
    Normal { host: Option<String> },
    Incognito,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChromeEligibilityObservation {
    Normal { window_id: Option<i64>, url: String },
    Incognito { window_id: Option<i64> },
    Unavailable,
}

#[derive(Clone, Debug)]
struct WindowRecord {
    state: Option<ChromeWindowState>,
    version: u64,
}

struct EligibilityState {
    filter: FilterConfig,
    windows: HashMap<(i32, i64), WindowRecord>,
    next_version: u64,
}

#[derive(Clone)]
pub struct ChromeEligibilityPublisher {
    state: Arc<RwLock<EligibilityState>>,
}

impl ChromeEligibilityPublisher {
    pub fn observe(&self, pid: i64, observation: ChromeEligibilityObservation) {
        let Ok(pid) = i32::try_from(pid) else {
            return;
        };
        let (active_key, next_state) = match observation {
            ChromeEligibilityObservation::Normal { window_id, url } => (
                window_id.map(|window_id| (pid, window_id)),
                Some(ChromeWindowState::Normal {
                    host: website_host(&url),
                }),
            ),
            ChromeEligibilityObservation::Incognito { window_id } => (
                window_id.map(|window_id| (pid, window_id)),
                Some(ChromeWindowState::Incognito),
            ),
            ChromeEligibilityObservation::Unavailable => (None, None),
        };
        let Ok(mut state) = self.state.write() else {
            crate::trace::trace!(
                "component=chrome phase=eligibility action=observe result=poisoned"
            );
            return;
        };
        mark_disappeared_windows(&mut state, pid, active_key);
        let Some(key) = active_key else {
            return;
        };
        if state
            .windows
            .get(&key)
            .and_then(|record| record.state.as_ref())
            == next_state.as_ref()
        {
            return;
        }
        let version = next_version(&mut state);
        state.windows.insert(
            key,
            WindowRecord {
                state: next_state,
                version,
            },
        );
    }

    pub(crate) fn clear_all(&self) {
        let Ok(mut state) = self.state.write() else {
            return;
        };
        let active: Vec<_> = state
            .windows
            .iter()
            .filter_map(|(key, record)| record.state.as_ref().map(|_| *key))
            .collect();
        for key in active {
            let version = next_version(&mut state);
            if let Some(record) = state.windows.get_mut(&key) {
                record.state = None;
                record.version = version;
            }
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

    #[must_use]
    pub fn state_version(&self, pid: i64, window_id: i64) -> Option<u64> {
        let pid = i32::try_from(pid).ok()?;
        self.state.read().ok().and_then(|state| {
            state
                .windows
                .get(&(pid, window_id))
                .filter(|record| record.state.is_some())
                .map(|record| record.version)
        })
    }

    #[must_use]
    pub const fn poll_interval(&self) -> Duration {
        CHROME_POLL_INTERVAL
    }

    fn allows(&self, scope: PrivacyScope, pid: i64, window_id: Option<i64>) -> bool {
        let (Ok(pid), Some(window_id)) = (i32::try_from(pid), window_id) else {
            return false;
        };
        self.state.read().is_ok_and(|state| {
            let Some(ChromeWindowState::Normal { host }) = state
                .windows
                .get(&(pid, window_id))
                .and_then(|record| record.state.as_ref())
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
                    .and_then(|record| record.state.as_ref())
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
        next_version: 0,
    }));
    (
        ChromeEligibilityPublisher {
            state: Arc::clone(&state),
        },
        ChromeEligibilityTracker { state },
    )
}

fn mark_disappeared_windows(
    state: &mut EligibilityState,
    pid: i32,
    active_key: Option<(i32, i64)>,
) {
    let disappeared: Vec<_> = state
        .windows
        .iter()
        .filter_map(|(key, record)| {
            (key.0 == pid && Some(*key) != active_key && record.state.is_some()).then_some(*key)
        })
        .collect();
    for key in disappeared {
        let version = next_version(state);
        if let Some(record) = state.windows.get_mut(&key) {
            record.state = None;
            record.version = version;
        }
    }
}

fn next_version(state: &mut EligibilityState) -> u64 {
    state.next_version = state.next_version.saturating_add(1);
    state.next_version
}

#[cfg(test)]
mod tests {
    use zanei_core::config::{FilterConfig, ScopedFilterConfig};

    use super::*;

    fn normal(window_id: i64, url: &str) -> ChromeEligibilityObservation {
        ChromeEligibilityObservation::Normal {
            window_id: Some(window_id),
            url: url.to_owned(),
        }
    }

    #[test]
    fn unchanged_observation_preserves_version() {
        let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
        publisher.observe(7, normal(11, "https://example.com"));
        let version = tracker.state_version(7, 11).expect("version");

        publisher.observe(7, normal(11, "https://example.com"));

        assert_eq!(tracker.state_version(7, 11), Some(version));
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

        publisher.observe(7, ChromeEligibilityObservation::Unavailable);
        assert_eq!(tracker.state_version(7, 11), None);
    }

    #[test]
    fn filter_replacement_rechecks_preserved_host_without_changing_version() {
        let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
        publisher.observe(7, normal(11, "https://example.com"));
        let version = tracker.state_version(7, 11);

        publisher.replace_filter(FilterConfig {
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
}
