//! Versioned Chrome window eligibility shared by text and snapshot capture.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Instant,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChromeEligibilityObservation {
    Normal { window_id: Option<i64>, url: String },
    Incognito { window_id: Option<i64> },
    Unavailable { window_id: Option<i64> },
}

#[derive(Clone, Debug)]
struct WindowRecord {
    state: Option<ChromeWindowState>,
    version: u64,
    observed_at: Instant,
    applescript_window_id: Option<String>,
}

struct EligibilityState {
    filter: FilterConfig,
    windows: HashMap<(i32, i64), WindowRecord>,
    next_version: u64,
}

#[derive(Default)]
pub(crate) struct ChromeEligibilityDecision {
    allowed: bool,
    capture_context: CaptureContext,
    version: Option<u64>,
}

impl ChromeEligibilityDecision {
    pub(crate) const fn is_allowed(&self) -> bool {
        self.allowed
    }

    pub(crate) fn capture_context(&self) -> CaptureContext {
        self.capture_context.clone()
    }

    pub(crate) const fn version(&self) -> Option<u64> {
        self.version
    }
}

#[derive(Clone)]
pub struct ChromeEligibilityPublisher {
    state: Arc<RwLock<EligibilityState>>,
}

impl ChromeEligibilityPublisher {
    pub fn observe(&self, pid: i64, observation: ChromeEligibilityObservation) {
        self.observe_at(pid, observation, Instant::now());
    }

    pub(crate) fn observe_at(
        &self,
        pid: i64,
        observation: ChromeEligibilityObservation,
        observed_at: Instant,
    ) {
        self.observe_with_window_id_at(pid, observation, None, observed_at);
    }

    pub(crate) fn observe_with_window_id_at(
        &self,
        pid: i64,
        observation: ChromeEligibilityObservation,
        applescript_window_id: Option<String>,
        observed_at: Instant,
    ) {
        let Ok(pid) = i32::try_from(pid) else {
            return;
        };
        let (key, next_state) = match observation {
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
            ChromeEligibilityObservation::Unavailable { window_id } => {
                let Ok(mut state) = self.state.write() else {
                    crate::trace::trace!(
                        "component=chrome phase=eligibility action=observe result=poisoned"
                    );
                    return;
                };
                mark_unavailable(&mut state, pid, window_id, observed_at);
                return;
            }
        };
        let Ok(mut state) = self.state.write() else {
            crate::trace::trace!(
                "component=chrome phase=eligibility action=observe result=poisoned"
            );
            return;
        };
        let Some(key) = key else {
            return;
        };
        if let Some(record) = state.windows.get_mut(&key)
            && record.state.as_ref() == next_state.as_ref()
            && applescript_window_id
                .as_ref()
                .is_none_or(|window_id| record.applescript_window_id.as_ref() == Some(window_id))
        {
            record.observed_at = observed_at;
            if let Some(applescript_window_id) = applescript_window_id {
                record.applescript_window_id = Some(applescript_window_id);
            }
            return;
        }
        let remembered_window_id = applescript_window_id.or_else(|| {
            state
                .windows
                .get(&key)
                .and_then(|record| record.applescript_window_id.clone())
        });
        let version = next_version(&mut state);
        state.windows.insert(
            key,
            WindowRecord {
                state: next_state,
                version,
                observed_at,
                applescript_window_id: remembered_window_id,
            },
        );
    }

    pub(crate) fn clear_all(&self) {
        let Ok(mut state) = self.state.write() else {
            return;
        };
        let observed_at = Instant::now();
        let keys: Vec<_> = state.windows.keys().copied().collect();
        for key in keys {
            mark_record_unavailable(&mut state, key, observed_at);
        }
    }

    pub(crate) fn applescript_window_id(&self, pid: i64, window_id: i64) -> Option<String> {
        let pid = i32::try_from(pid).ok()?;
        self.state
            .read()
            .ok()?
            .windows
            .get(&(pid, window_id))
            .and_then(|record| record.applescript_window_id.clone())
    }
}

#[derive(Clone)]
pub struct ChromeEligibilityTracker {
    state: Arc<RwLock<EligibilityState>>,
}

impl ChromeEligibilityTracker {
    pub fn allows_url_events(&self, pid: i64, window_id: Option<i64>) -> bool {
        self.decision(PrivacyScope::AllEvents, pid, window_id)
            .is_allowed()
    }

    pub fn allows_text(&self, pid: i64, window_id: Option<i64>) -> bool {
        self.decision(PrivacyScope::TextContent, pid, window_id)
            .is_allowed()
    }

    pub fn allows_snapshot(&self, pid: i64, window_id: Option<i64>) -> bool {
        self.decision(PrivacyScope::ContentSnapshot, pid, window_id)
            .is_allowed()
    }

    pub(crate) fn replace_filter(&self, filter: FilterConfig) {
        match self.state.write() {
            Ok(mut state) => state.filter = filter,
            Err(_) => crate::trace::trace!(
                "component=chrome phase=eligibility action=replace_filter result=poisoned"
            ),
        }
    }

    #[must_use]
    #[cfg(test)]
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
    pub fn observed_at(&self, pid: i64, window_id: i64) -> Option<Instant> {
        let pid = i32::try_from(pid).ok()?;
        self.state
            .read()
            .ok()?
            .windows
            .get(&(pid, window_id))
            .map(|record| record.observed_at)
    }

    pub(crate) fn decision(
        &self,
        scope: PrivacyScope,
        pid: i64,
        window_id: Option<i64>,
    ) -> ChromeEligibilityDecision {
        let (Ok(pid), Some(window_id)) = (i32::try_from(pid), window_id) else {
            return ChromeEligibilityDecision::default();
        };
        let Ok(state) = self.state.read() else {
            return ChromeEligibilityDecision::default();
        };
        let Some(record) = state.windows.get(&(pid, window_id)) else {
            return ChromeEligibilityDecision::default();
        };
        let capture_context = record
            .state
            .as_ref()
            .map(|window| CaptureContext {
                website_host: match window {
                    ChromeWindowState::Normal { host } => host.clone(),
                    ChromeWindowState::Incognito => None,
                },
            })
            .unwrap_or_default();
        let allowed = match record.state.as_ref() {
            Some(ChromeWindowState::Normal { host }) => {
                host_is_allowed_for(scope, host.as_deref(), &state.filter)
            }
            Some(ChromeWindowState::Incognito) | None => false,
        };
        let version = record.state.as_ref().map(|_| record.version);
        debug_assert!(!allowed || version.is_some());
        ChromeEligibilityDecision {
            allowed,
            capture_context,
            version,
        }
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

fn mark_unavailable(
    state: &mut EligibilityState,
    pid: i32,
    window_id: Option<i64>,
    observed_at: Instant,
) {
    if let Some(window_id) = window_id {
        let key = (pid, window_id);
        if state.windows.contains_key(&key) {
            mark_record_unavailable(state, key, observed_at);
        } else {
            let version = next_version(state);
            state.windows.insert(
                key,
                WindowRecord {
                    state: None,
                    version,
                    observed_at,
                    applescript_window_id: None,
                },
            );
        }
        return;
    }
    let keys: Vec<_> = state
        .windows
        .keys()
        .filter(|key| key.0 == pid)
        .copied()
        .collect();
    for key in keys {
        mark_record_unavailable(state, key, observed_at);
    }
}

fn mark_record_unavailable(state: &mut EligibilityState, key: (i32, i64), observed_at: Instant) {
    let state_changed = state
        .windows
        .get(&key)
        .is_some_and(|record| record.state.is_some());
    let version = state_changed.then(|| next_version(state));
    let record = state
        .windows
        .get_mut(&key)
        .expect("Chrome window key remains present");
    record.state = None;
    if let Some(version) = version {
        record.version = version;
    }
    record.observed_at = observed_at;
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
    fn unchanged_observation_preserves_version_but_window_identity_change_advances_it() {
        let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
        let first_observation = Instant::now();
        publisher.observe_with_window_id_at(
            7,
            normal(11, "https://example.com"),
            Some("window-a".to_owned()),
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
            Some("window-b".to_owned()),
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
    fn decision_returns_allow_context_and_version_from_one_record() {
        let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
        publisher.observe(7, normal(11, "https://example.com/path"));

        let decision = tracker.decision(PrivacyScope::TextContent, 7, Some(11));

        assert!(decision.is_allowed());
        assert_eq!(
            decision.capture_context().website_host.as_deref(),
            Some("example.com")
        );
        assert!(decision.version().is_some());
    }
}
