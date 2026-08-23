//! Transition-driven Chrome observation worker and navigation state.

use std::{
    collections::HashMap,
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
    ChromeObservation, ChromeQuery, ChromeSnapshot, ObservationTrigger,
};
use crate::{
    focus_context::{FocusSnapshot, FocusTransition, FocusTransitionReceiver},
    workspace::ApplicationInfo,
};

pub(super) const EVENT_SOURCE: &str = "macos.applescript";
pub(super) const EVENT_TYPE: &str = "browser.navigate";
const WORKER_WAKE_INTERVAL: Duration = Duration::from_millis(100);
const ON_DEMAND_DEBOUNCE: Duration = Duration::from_millis(200);

pub(super) struct ChromeWorkerReceivers<'a> {
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
        && !handle_focus_transition(
            transition,
            Instant::now(),
            api,
            sender,
            &mut state,
            metrics,
            eligibility,
        )
    {
        eligibility.clear_all();
        return;
    }

    while !stop.load(Ordering::Acquire) {
        let wait = state
            .on_demand
            .values()
            .min()
            .map_or(WORKER_WAKE_INTERVAL, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(WORKER_WAKE_INTERVAL)
            });
        match receivers.focus.recv_timeout(wait) {
            Ok(transition) => {
                if !handle_focus_transition(
                    transition,
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
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        loop {
            match receivers.focus.try_recv() {
                Ok(transition) => {
                    if !handle_focus_transition(
                        transition,
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
        ObservationTrigger::OnDemand { pid, window_id } => {
            // Keep the first deadline so a quarantine that repeats its request
            // while waiting cannot postpone confirmation forever.
            state
                .on_demand
                .entry((pid, window_id))
                .or_insert(now + ON_DEMAND_DEBOUNCE);
            true
        }
        ObservationTrigger::PageLoaded { pid } => {
            observe_frontmost(pid, now, api, sender, state, metrics, eligibility)
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
    let Some((&key, &deadline)) = state.on_demand.iter().min_by_key(|(_, deadline)| *deadline)
    else {
        return true;
    };
    if now < deadline {
        return true;
    }
    state.on_demand.remove(&key);
    observe_confirmation(key, now, api, sender, state, metrics, eligibility)
}

#[derive(Default)]
pub(super) struct ChromeWorkerState {
    pub(super) navigation: NavigationTracker,
    pub(super) frontmost: Option<FocusSnapshot>,
    pub(super) apps: HashMap<i64, ApplicationInfo>,
    pub(super) on_demand: HashMap<(i64, i64), Instant>,
}

pub(super) fn handle_focus_transition<A: ChromeApi>(
    transition: FocusTransition,
    observed_at: Instant,
    api: &mut A,
    sender: &SyncSender<RawEvent>,
    state: &mut ChromeWorkerState,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> bool {
    // A wake resync is the single ordering boundary for Chrome state: invalidate
    // stale eligibility, then immediately rebuild it from the re-read focus.
    if transition.resynced {
        eligibility.clear_all();
        state.navigation.clear();
    }
    let Some(current) = transition.current else {
        if let Some(previous) = transition.previous.filter(|focus| is_chrome(&focus.app)) {
            terminate_chrome(previous.app.pid, state, eligibility);
        } else {
            leave_chrome_focus(state);
        }
        return true;
    };
    if !is_chrome(&current.app) {
        leave_chrome_focus(state);
        return true;
    }
    let pid = current.app.pid;
    state.apps.insert(pid, current.app.clone());
    state.frontmost = Some(current);
    observe_frontmost(pid, observed_at, api, sender, state, metrics, eligibility)
}

fn leave_chrome_focus(state: &mut ChromeWorkerState) {
    state.frontmost = None;
    state.navigation.reset_page();
}

fn terminate_chrome(
    pid: i64,
    state: &mut ChromeWorkerState,
    eligibility: &ChromeEligibilityPublisher,
) {
    eligibility.observe(
        pid,
        ChromeEligibilityObservation::Unavailable { window_id: None },
    );
    state.frontmost = None;
    state.apps.remove(&pid);
    state
        .on_demand
        .retain(|(candidate_pid, _), _| *candidate_pid != pid);
    state.navigation.terminate_pid(pid);
}

fn observe_frontmost<A: ChromeApi>(
    pid: i64,
    observed_at: Instant,
    api: &mut A,
    sender: &SyncSender<RawEvent>,
    state: &mut ChromeWorkerState,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> bool {
    let Some(focus) = state
        .frontmost
        .as_ref()
        .filter(|focus| focus.app.pid == pid)
        .cloned()
    else {
        return true;
    };
    let query = ChromeQuery::FrontWindow {
        pid,
        window_id: focus.window.as_ref().and_then(|window| window.id),
    };
    let context = ObservationContext {
        sender,
        metrics,
        eligibility,
    };
    match observe_query_once(
        api,
        &mut state.navigation,
        Some(&focus.app),
        query,
        true,
        observed_at,
        &context,
    ) {
        ObservationOutcome::Continue => true,
        ObservationOutcome::Inactive => {
            terminate_chrome(pid, state, eligibility);
            true
        }
        ObservationOutcome::Stop => false,
    }
}

fn observe_confirmation<A: ChromeApi>(
    (pid, window_id): (i64, i64),
    observed_at: Instant,
    api: &mut A,
    sender: &SyncSender<RawEvent>,
    state: &mut ChromeWorkerState,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> bool {
    let query = state
        .navigation
        .applescript_window_id(pid, window_id)
        .map_or(
            ChromeQuery::FrontWindow {
                pid,
                window_id: Some(window_id),
            },
            |applescript_window_id| ChromeQuery::Window {
                pid,
                window_id,
                applescript_window_id,
            },
        );
    let app = state.apps.get(&pid);
    let context = ObservationContext {
        sender,
        metrics,
        eligibility,
    };
    match observe_query_once(
        api,
        &mut state.navigation,
        app,
        query,
        false,
        observed_at,
        &context,
    ) {
        ObservationOutcome::Continue => true,
        ObservationOutcome::Inactive => {
            terminate_chrome(pid, state, eligibility);
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

pub(super) struct ObservationContext<'a> {
    pub(super) sender: &'a SyncSender<RawEvent>,
    pub(super) metrics: &'a ChromeMetrics,
    pub(super) eligibility: &'a ChromeEligibilityPublisher,
}

pub(super) fn observe_query_once<A: ChromeApi>(
    api: &mut A,
    tracker: &mut NavigationTracker,
    app: Option<&ApplicationInfo>,
    query: ChromeQuery,
    emit_navigation: bool,
    observed_at: Instant,
    context: &ObservationContext<'_>,
) -> ObservationOutcome {
    let ObservationContext {
        sender,
        metrics,
        eligibility,
    } = context;
    let pid = query.pid();
    match api.query(query) {
        Ok(ChromeObservation::Snapshot(snapshot)) => {
            if validate_snapshot(&snapshot).is_err() {
                eligibility.observe_at(
                    pid,
                    ChromeEligibilityObservation::Unavailable { window_id: None },
                    observed_at,
                );
                metrics.degraded.fetch_add(1, Ordering::Relaxed);
                return ObservationOutcome::Stop;
            }
            eligibility.observe_at(
                pid,
                ChromeEligibilityObservation::Normal {
                    window_id: snapshot.window_id,
                    url: snapshot.url.clone(),
                },
                observed_at,
            );
            tracker.remember_window(pid, &snapshot);
            if !emit_navigation {
                return ObservationOutcome::Continue;
            }
            let navigation = match tracker.observe(snapshot) {
                Ok(navigation) => navigation,
                Err(_) => {
                    eligibility.observe_at(
                        pid,
                        ChromeEligibilityObservation::Unavailable { window_id: None },
                        observed_at,
                    );
                    metrics.degraded.fetch_add(1, Ordering::Relaxed);
                    return ObservationOutcome::Stop;
                }
            };
            let Some(navigation) = navigation else {
                return ObservationOutcome::Continue;
            };
            let Some(app) = app else {
                eligibility.observe_at(
                    pid,
                    ChromeEligibilityObservation::Unavailable { window_id: None },
                    observed_at,
                );
                metrics.degraded.fetch_add(1, Ordering::Relaxed);
                return ObservationOutcome::Stop;
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
            eligibility.observe_at(
                pid,
                ChromeEligibilityObservation::Incognito { window_id },
                observed_at,
            );
            if emit_navigation {
                tracker.reset_page();
            }
            ObservationOutcome::Continue
        }
        Ok(ChromeObservation::NoWindow) => {
            eligibility.observe_at(
                pid,
                ChromeEligibilityObservation::Unavailable {
                    window_id: if query.is_targeted() {
                        query.window_id()
                    } else {
                        None
                    },
                },
                observed_at,
            );
            if query.is_targeted() {
                if let Some(window_id) = query.window_id() {
                    tracker.forget_window(pid, window_id);
                }
            } else {
                tracker.terminate_pid(pid);
            }
            ObservationOutcome::Continue
        }
        Ok(ChromeObservation::NotRunning) => {
            eligibility.observe_at(
                pid,
                ChromeEligibilityObservation::Unavailable { window_id: None },
                observed_at,
            );
            tracker.terminate_pid(pid);
            ObservationOutcome::Inactive
        }
        Err(_) => {
            eligibility.observe_at(
                pid,
                ChromeEligibilityObservation::Unavailable { window_id: None },
                observed_at,
            );
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
    window_ids: HashMap<(i64, i64), i64>,
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

    fn remember_window(&mut self, pid: i64, snapshot: &ChromeSnapshot) {
        if let Some(window_id) = snapshot.window_id {
            self.window_ids
                .insert((pid, window_id), snapshot.applescript_window_id);
        }
    }

    fn applescript_window_id(&self, pid: i64, window_id: i64) -> Option<i64> {
        self.window_ids.get(&(pid, window_id)).copied()
    }

    fn forget_window(&mut self, pid: i64, window_id: i64) {
        self.window_ids.remove(&(pid, window_id));
    }

    fn terminate_pid(&mut self, pid: i64) {
        self.window_ids
            .retain(|(candidate_pid, _), _| *candidate_pid != pid);
        self.reset_page();
    }

    fn reset_page(&mut self) {
        self.previous = None;
    }

    fn clear(&mut self) {
        self.previous = None;
        self.window_ids.clear();
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

mod validation;

pub(super) use validation::SnapshotError;
use validation::validate_snapshot;
