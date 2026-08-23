//! Transition-driven Chrome observation worker and navigation state.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
    },
    time::{Duration, Instant},
};

use zanei_collector::RawEvent;
use zanei_core::{
    privacy::CHROME_BUNDLE_ID,
    schema::{App, BrowserMode, BrowserNavigateData, BrowserTransition, EventData, Window},
};

use super::{
    ChromeApi, ChromeEligibilityObservation, ChromeEligibilityPublisher, ChromeMetrics,
    ChromeObservation, ChromeSnapshot, ObservationTrigger,
};
use crate::{
    focus_context::{FocusTransition, FocusTransitionReceiver},
    workspace::{ApplicationInfo, WorkspaceEvent},
};

pub(super) const EVENT_SOURCE: &str = "macos.applescript";
pub(super) const EVENT_TYPE: &str = "browser.navigate";
const WORKER_WAKE_INTERVAL: Duration = Duration::from_millis(100);
const ON_DEMAND_DEBOUNCE: Duration = Duration::from_millis(200);

pub(super) struct ChromeWorkerReceivers<'a> {
    pub(super) workspace: &'a Receiver<WorkspaceEvent>,
    pub(super) focus: &'a FocusTransitionReceiver,
    pub(super) observations: &'a Receiver<ObservationTrigger>,
}

pub(super) fn run_worker<A: ChromeApi>(
    api: &mut A,
    receivers: &ChromeWorkerReceivers<'_>,
    sender: &SyncSender<RawEvent>,
    stop: &AtomicBool,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
    initial_focus: Option<FocusTransition>,
) {
    let mut state = ChromeWorkerState::default();
    if let Some(transition) = initial_focus
        && !handle_focus_transition(transition, api, sender, &mut state, metrics, eligibility)
    {
        eligibility.clear_all();
        return;
    }

    while !stop.load(Ordering::Acquire) {
        let wait = state
            .on_demand
            .map_or(WORKER_WAKE_INTERVAL, |(_, deadline)| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(WORKER_WAKE_INTERVAL)
            });
        match receivers.workspace.recv_timeout(wait) {
            Ok(event) => handle_workspace_event(event, &mut state, eligibility),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        loop {
            match receivers.focus.try_recv() {
                Ok(transition) => {
                    if !handle_focus_transition(
                        transition,
                        api,
                        sender,
                        &mut state,
                        metrics,
                        eligibility,
                    ) {
                        eligibility.clear_all();
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    eligibility.clear_all();
                    return;
                }
            }
        }
        for trigger in receivers.observations.try_iter() {
            if !handle_observation_trigger(
                trigger,
                Instant::now(),
                api,
                sender,
                &mut state,
                metrics,
                eligibility,
            ) {
                eligibility.clear_all();
                return;
            }
        }
        if !service_on_demand(
            Instant::now(),
            api,
            sender,
            &mut state,
            metrics,
            eligibility,
        ) {
            eligibility.clear_all();
            return;
        }
    }
    eligibility.clear_all();
}

pub(super) fn handle_observation_trigger<A: ChromeApi>(
    trigger: ObservationTrigger,
    now: Instant,
    api: &mut A,
    sender: &SyncSender<RawEvent>,
    state: &mut ChromeWorkerState,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> bool {
    match trigger {
        ObservationTrigger::OnDemand { pid } => {
            // Keep the first deadline so a quarantine that repeats its request
            // while waiting cannot postpone confirmation forever.
            state.on_demand = Some(match state.on_demand {
                Some((_, deadline)) => (pid, deadline),
                None => (pid, now + ON_DEMAND_DEBOUNCE),
            });
            true
        }
        ObservationTrigger::PageLoaded { pid } => {
            observe_frontmost(pid, api, sender, state, metrics, eligibility)
        }
    }
}

pub(super) fn service_on_demand<A: ChromeApi>(
    now: Instant,
    api: &mut A,
    sender: &SyncSender<RawEvent>,
    state: &mut ChromeWorkerState,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> bool {
    let Some((pid, deadline)) = state.on_demand else {
        return true;
    };
    if now < deadline {
        return true;
    }
    state.on_demand = None;
    observe_frontmost(pid, api, sender, state, metrics, eligibility)
}

#[derive(Default)]
pub(super) struct ChromeWorkerState {
    pub(super) navigation: NavigationTracker,
    pub(super) frontmost: Option<ApplicationInfo>,
    pub(super) on_demand: Option<(i64, Instant)>,
}

pub(super) fn handle_workspace_event(
    event: WorkspaceEvent,
    state: &mut ChromeWorkerState,
    eligibility: &ChromeEligibilityPublisher,
) {
    match event {
        WorkspaceEvent::Terminated(app) if is_chrome(&app) => {
            eligibility.observe(app.pid, ChromeEligibilityObservation::Unavailable);
            state.frontmost = None;
            state.on_demand = None;
            state.navigation.reset();
        }
        WorkspaceEvent::DidWake => eligibility.clear_all(),
        WorkspaceEvent::Activated(_)
        | WorkspaceEvent::Launched(_)
        | WorkspaceEvent::Terminated(_) => {}
    }
}

pub(super) fn handle_focus_transition<A: ChromeApi>(
    transition: FocusTransition,
    api: &mut A,
    sender: &SyncSender<RawEvent>,
    state: &mut ChromeWorkerState,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> bool {
    let Some(current) = transition.current else {
        clear_frontmost(state, eligibility);
        return true;
    };
    if !is_chrome(&current.app) {
        clear_frontmost(state, eligibility);
        return true;
    }
    let pid = current.app.pid;
    state.frontmost = Some(current.app);
    observe_frontmost(pid, api, sender, state, metrics, eligibility)
}

fn clear_frontmost(state: &mut ChromeWorkerState, eligibility: &ChromeEligibilityPublisher) {
    if let Some(app) = state.frontmost.take() {
        eligibility.observe(app.pid, ChromeEligibilityObservation::Unavailable);
    }
    state.on_demand = None;
    state.navigation.reset();
}

fn observe_frontmost<A: ChromeApi>(
    pid: i64,
    api: &mut A,
    sender: &SyncSender<RawEvent>,
    state: &mut ChromeWorkerState,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> bool {
    let Some(app) = state
        .frontmost
        .as_ref()
        .filter(|app| app.pid == pid)
        .cloned()
    else {
        return true;
    };
    match observe_once(
        api,
        &mut state.navigation,
        &app,
        sender,
        metrics,
        eligibility,
    ) {
        ObservationOutcome::Continue => true,
        ObservationOutcome::Inactive => {
            state.frontmost = None;
            true
        }
        ObservationOutcome::Stop => false,
    }
}

fn is_chrome(app: &ApplicationInfo) -> bool {
    app.bundle_id.as_deref() == Some(CHROME_BUNDLE_ID)
}

pub(super) enum ObservationOutcome {
    Continue,
    Inactive,
    Stop,
}

pub(super) fn observe_once<A: ChromeApi>(
    api: &mut A,
    tracker: &mut NavigationTracker,
    app: &ApplicationInfo,
    sender: &SyncSender<RawEvent>,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> ObservationOutcome {
    match api.query(app.pid) {
        Ok(ChromeObservation::Snapshot(snapshot)) => {
            eligibility.observe(
                app.pid,
                ChromeEligibilityObservation::Normal {
                    window_id: snapshot.window_id,
                    url: snapshot.url.clone(),
                },
            );
            let navigation = match tracker.observe(snapshot) {
                Ok(navigation) => navigation,
                Err(_) => {
                    eligibility.observe(app.pid, ChromeEligibilityObservation::Unavailable);
                    metrics.degraded.fetch_add(1, Ordering::Relaxed);
                    return ObservationOutcome::Stop;
                }
            };
            let Some(navigation) = navigation else {
                return ObservationOutcome::Continue;
            };
            match sender.try_send(raw_event(app, navigation)) {
                Ok(()) => ObservationOutcome::Continue,
                Err(TrySendError::Full(_)) => {
                    metrics.dropped.fetch_add(1, Ordering::Relaxed);
                    ObservationOutcome::Continue
                }
                Err(TrySendError::Disconnected(_)) => {
                    metrics.dropped.fetch_add(1, Ordering::Relaxed);
                    metrics.degraded.fetch_add(1, Ordering::Relaxed);
                    ObservationOutcome::Stop
                }
            }
        }
        Ok(ChromeObservation::Incognito { window_id }) => {
            eligibility.observe(
                app.pid,
                ChromeEligibilityObservation::Incognito { window_id },
            );
            tracker.reset();
            ObservationOutcome::Continue
        }
        Ok(ChromeObservation::NoWindow) => {
            eligibility.observe(app.pid, ChromeEligibilityObservation::Unavailable);
            tracker.reset();
            ObservationOutcome::Continue
        }
        Ok(ChromeObservation::NotRunning) => {
            eligibility.observe(app.pid, ChromeEligibilityObservation::Unavailable);
            tracker.reset();
            ObservationOutcome::Inactive
        }
        Ok(ChromeObservation::NotFrontmost) => {
            eligibility.observe(app.pid, ChromeEligibilityObservation::Unavailable);
            ObservationOutcome::Inactive
        }
        Err(_) => {
            eligibility.observe(app.pid, ChromeEligibilityObservation::Unavailable);
            metrics.degraded.fetch_add(1, Ordering::Relaxed);
            ObservationOutcome::Stop
        }
    }
}

fn raw_event(app: &ApplicationInfo, navigation: Navigation) -> RawEvent {
    let website_host = zanei_core::privacy::website_host(&navigation.snapshot.url);
    RawEvent {
        observed_at: None,
        source: EVENT_SOURCE.to_owned(),
        event_type: EVENT_TYPE.to_owned(),
        app: App {
            name: app.name.clone(),
            bundle_id: app.bundle_id.clone(),
            pid: Some(app.pid),
        },
        window: Some(Window {
            title: navigation.snapshot.window_title,
            // Chrome's AppleScript ID is a browser session ID, not CGWindowNumber.
            id: None,
        }),
        element: None,
        data: EventData::BrowserNavigate(BrowserNavigateData {
            url: navigation.snapshot.url.into(),
            tab_title: navigation.snapshot.tab_title,
            mode: BrowserMode::Normal,
            transition: navigation.transition,
        }),
        capture_context: zanei_core::schema::CaptureContext { website_host },
    }
}

#[derive(Default)]
pub(super) struct NavigationTracker {
    pub(super) previous: Option<ObservedPage>,
}

impl NavigationTracker {
    pub(super) fn observe(
        &mut self,
        snapshot: ChromeSnapshot,
    ) -> Result<Option<Navigation>, SnapshotError> {
        validate_snapshot(&snapshot)?;
        let current = ObservedPage {
            window_key: snapshot.window_key.clone(),
            tab_key: snapshot.tab_key.clone(),
            url: snapshot.url.clone(),
        };
        let transition = match self.previous.as_ref() {
            None => None,
            Some(previous)
                if previous.window_key != current.window_key
                    || previous.tab_key != current.tab_key =>
            {
                Some(BrowserTransition::TabSwitch)
            }
            Some(previous) if previous.url != current.url => Some(BrowserTransition::Navigate),
            Some(_) => {
                self.previous = Some(current);
                return Ok(None);
            }
        };
        self.previous = Some(current);
        Ok(Some(Navigation {
            snapshot,
            transition,
        }))
    }

    fn reset(&mut self) {
        self.previous = None;
    }
}

pub(super) struct ObservedPage {
    window_key: String,
    tab_key: String,
    url: String,
}

pub(super) struct Navigation {
    pub(super) transition: Option<BrowserTransition>,
    snapshot: ChromeSnapshot,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum SnapshotError {
    #[error("Chrome window identity is empty")]
    EmptyWindowIdentity,
    #[error("Chrome tab identity is empty")]
    EmptyTabIdentity,
    #[error("Chrome returned a non-absolute URL")]
    InvalidUrl,
}

fn validate_snapshot(snapshot: &ChromeSnapshot) -> Result<(), SnapshotError> {
    if snapshot.window_key.is_empty() {
        return Err(SnapshotError::EmptyWindowIdentity);
    }
    if snapshot.tab_key.is_empty() {
        return Err(SnapshotError::EmptyTabIdentity);
    }
    if !is_absolute_uri(&snapshot.url) {
        return Err(SnapshotError::InvalidUrl);
    }
    Ok(())
}

fn is_absolute_uri(value: &str) -> bool {
    let Some((scheme, remainder)) = value.split_once(':') else {
        return false;
    };
    !scheme.is_empty()
        && scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic()
                || (index > 0 && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')))
        })
        && !remainder.is_empty()
        && !value.chars().any(char::is_whitespace)
}
