//! Versioned browser window eligibility shared by text and snapshot capture.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock, RwLockWriteGuard},
    time::Instant,
};

use zanei_core::{
    config::FilterConfig,
    privacy::{
        CapturePolicyDecision, PrivacyScope, evaluate_capture_policy, host_is_allowed_for,
        website_host,
    },
    schema::{App, CaptureContext, CaptureSurface},
};

use crate::{browser_context::BrowserTarget, ffi::applescript::AppleScriptWindowId};

#[derive(Clone, Debug, Eq, PartialEq)]
enum BrowserWindowState {
    Normal {
        url: Arc<str>,
        tab_id: Option<String>,
    },
    Safari {
        url: Option<Arc<str>>,
    },
    Incognito,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChromeEligibilityObservation {
    Normal {
        window_id: Option<i64>,
        url: String,
    },
    Safari {
        window_id: Option<i64>,
        url: Option<String>,
    },
    Incognito {
        window_id: Option<i64>,
    },
    Unavailable {
        window_id: Option<i64>,
    },
}

#[derive(Clone, Debug)]
struct WindowRecord {
    state: Option<BrowserWindowState>,
    version: u64,
    observed_at: Instant,
    applescript_window_id: Option<AppleScriptWindowId>,
}

struct EligibilityState {
    filter: FilterConfig,
    filter_revision: u64,
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
        applescript_window_id: Option<AppleScriptWindowId>,
        observed_at: Instant,
    ) {
        self.observe_with_surface_at(pid, observation, applescript_window_id, None, observed_at);
    }

    pub(crate) fn observe_with_surface_at(
        &self,
        pid: i64,
        observation: ChromeEligibilityObservation,
        applescript_window_id: Option<AppleScriptWindowId>,
        tab_id: Option<&str>,
        observed_at: Instant,
    ) {
        let Ok(state) = self.state.write() else {
            crate::trace::trace!(
                "component=chrome phase=eligibility action=observe result=poisoned"
            );
            return;
        };
        EligibilityPublication { state }.observe(
            pid,
            observation,
            applescript_window_id,
            tab_id,
            observed_at,
        );
    }

    pub(crate) fn filter_revision(&self) -> Option<u64> {
        self.state.read().ok().map(|state| state.filter_revision)
    }

    pub(crate) fn publication(&self, revision: u64) -> Option<EligibilityPublication<'_>> {
        let state = self.state.write().ok()?;
        (state.filter_revision == revision).then_some(EligibilityPublication { state })
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

    pub(crate) fn applescript_window_id(
        &self,
        pid: i64,
        window_id: i64,
    ) -> Option<AppleScriptWindowId> {
        let pid = i32::try_from(pid).ok()?;
        self.state
            .read()
            .ok()?
            .windows
            .get(&(pid, window_id))
            .and_then(|record| record.applescript_window_id.clone())
    }
}

/// Serializes query results and nonblocking publication with filter replacement.
pub(crate) struct EligibilityPublication<'a> {
    state: RwLockWriteGuard<'a, EligibilityState>,
}

impl EligibilityPublication<'_> {
    pub(crate) fn allows_url(&self, target: BrowserTarget, url: &str) -> bool {
        browser_is_allowed(
            target,
            PrivacyScope::AllEvents,
            Some(url),
            &self.state.filter,
        )
    }

    pub(crate) fn observe(
        &mut self,
        pid: i64,
        observation: ChromeEligibilityObservation,
        applescript_window_id: Option<AppleScriptWindowId>,
        tab_id: Option<&str>,
        observed_at: Instant,
    ) {
        let applescript_window_id_for_record = applescript_window_id.clone();
        let Ok(pid) = i32::try_from(pid) else {
            return;
        };
        let (key, next_state) = match observation {
            ChromeEligibilityObservation::Normal { window_id, url } => {
                let key = window_id.map(|window_id| (pid, window_id));
                let tab_id = tab_id.map(str::to_owned);
                (
                    key,
                    Some(BrowserWindowState::Normal {
                        url: url.into(),
                        tab_id,
                    }),
                )
            }
            ChromeEligibilityObservation::Safari { window_id, url } => (
                window_id.map(|window_id| (pid, window_id)),
                Some(BrowserWindowState::Safari {
                    url: url.map(Into::into),
                }),
            ),
            ChromeEligibilityObservation::Incognito { window_id } => (
                window_id.map(|window_id| (pid, window_id)),
                Some(BrowserWindowState::Incognito),
            ),
            ChromeEligibilityObservation::Unavailable { window_id } => {
                mark_unavailable(&mut self.state, pid, window_id, observed_at);
                return;
            }
        };
        let Some(key) = key else {
            return;
        };
        if let Some(record) = self.state.windows.get_mut(&key)
            && record.state.as_ref() == next_state.as_ref()
            && applescript_window_id
                .as_ref()
                .is_none_or(|window_id| record.applescript_window_id.as_ref() == Some(window_id))
        {
            record.observed_at = observed_at;
            if let Some(applescript_window_id) = applescript_window_id.as_ref() {
                record.applescript_window_id = Some(applescript_window_id.clone());
            }
            return;
        }
        let remembered_window_id = applescript_window_id_for_record.or_else(|| {
            self.state
                .windows
                .get(&key)
                .and_then(|record| record.applescript_window_id.clone())
        });
        let version = next_version(&mut self.state);
        self.state.windows.insert(
            key,
            WindowRecord {
                state: next_state,
                version,
                observed_at,
                applescript_window_id: remembered_window_id,
            },
        );
    }
}

#[derive(Clone)]
pub struct ChromeEligibilityTracker {
    state: Arc<RwLock<EligibilityState>>,
}

impl ChromeEligibilityTracker {
    pub fn allows_url_events(&self, pid: i64, window_id: Option<i64>) -> bool {
        let decision = self.decision(PrivacyScope::AllEvents, pid, window_id);
        decision.is_allowed() && decision.capture_context().url.is_some()
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
            Ok(mut state) => {
                let app_owned_policy_changed = state.filter.capture_policy != filter.capture_policy;
                if state.filter != filter {
                    state.filter_revision += 1;
                }
                state.filter = filter;
                if app_owned_policy_changed {
                    let observed_at = Instant::now();
                    let keys: Vec<_> = state.windows.keys().copied().collect();
                    for key in keys {
                        mark_record_unavailable(&mut state, key, observed_at);
                    }
                }
            }
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
            .map(|window| match window {
                BrowserWindowState::Normal { url, tab_id } => CaptureContext {
                    url: Some(url.clone()),
                    surface: Some(Box::new(CaptureSurface {
                        cg_window_id: Some(window_id),
                        applescript_window_id: record
                            .applescript_window_id
                            .as_ref()
                            .map(|id| id.as_str().to_owned()),
                        tab_id: tab_id.clone(),
                    })),
                },
                BrowserWindowState::Safari { url } => CaptureContext {
                    url: url.clone(),
                    surface: Some(Box::new(CaptureSurface {
                        cg_window_id: Some(window_id),
                        applescript_window_id: record
                            .applescript_window_id
                            .as_ref()
                            .map(|id| id.as_str().to_owned()),
                        tab_id: None,
                    })),
                },
                BrowserWindowState::Incognito => CaptureContext::default(),
            })
            .unwrap_or_default();
        let allowed = match record.state.as_ref() {
            Some(BrowserWindowState::Normal { url, .. }) => {
                browser_is_allowed(BrowserTarget::Chrome, scope, Some(url), &state.filter)
            }
            Some(BrowserWindowState::Safari { url }) => {
                browser_is_allowed(BrowserTarget::Safari, scope, url.as_deref(), &state.filter)
            }
            Some(BrowserWindowState::Incognito) | None => false,
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
        filter_revision: 0,
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

fn browser_is_allowed(
    target: BrowserTarget,
    scope: PrivacyScope,
    url: Option<&str>,
    filter: &FilterConfig,
) -> bool {
    match filter.capture_policy.as_ref() {
        Some(policy) => matches!(
            evaluate_capture_policy(
                policy,
                &App {
                    name: target.display_name().to_owned(),
                    bundle_id: Some(target.bundle_id().to_owned()),
                    pid: None,
                },
                None,
                url,
            ),
            CapturePolicyDecision::Allow
        ),
        None if target == BrowserTarget::Chrome => {
            host_is_allowed_for(scope, url.and_then(website_host).as_deref(), filter)
        }
        None => false,
    }
}

#[cfg(test)]
#[path = "eligibility/tests.rs"]
mod tests;
