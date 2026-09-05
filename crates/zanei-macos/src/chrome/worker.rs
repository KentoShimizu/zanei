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

use super::{
    BrowserPage, ChromeApi, ChromeEligibilityObservation, ChromeEligibilityPublisher,
    ChromeFailure, ChromeMetrics, ChromeObservation, ChromeQuery, ChromeSnapshot,
    ChromeValidationFailure, ObservationTrigger, eligibility::EligibilityPublication,
};
use crate::{
    browser_context::BrowserTarget,
    focus_context::{FocusContext, FocusSnapshot, FocusTransition, FocusTransitionReceiver},
    workspace::ApplicationInfo,
};

mod attribution;
mod events;
mod navigation;

use attribution::FrontWindowAttribution;
use events::raw_event;
pub(super) use navigation::{Navigation, NavigationTracker};

pub(super) const EVENT_SOURCE: &str = "macos.applescript";
pub(super) const EVENT_TYPE: &str = "browser.navigate";
const WORKER_WAKE_INTERVAL: Duration = Duration::from_millis(100);
const ON_DEMAND_DEBOUNCE: Duration = Duration::from_millis(200);

pub(super) struct ChromeWorkerReceivers<'a> {
    pub(super) focus: &'a FocusTransitionReceiver,
    pub(super) observations: &'a Receiver<ObservationTrigger>,
    pub(super) focus_context: &'a FocusContext,
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
    let context = ObservationContext {
        sender,
        stop,
        focus_context: receivers.focus_context,
        metrics,
        eligibility,
    };
    'worker: {
        if let Some(transition) = initial_focus
            && !handle_focus_transition(transition, Instant::now(), api, &mut state, &context)
        {
            break 'worker;
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
                        &mut state,
                        &context,
                    ) {
                        break 'worker;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break 'worker,
            }
            loop {
                match receivers.focus.try_recv() {
                    Ok(transition) => {
                        if !handle_focus_transition(
                            transition,
                            Instant::now(),
                            api,
                            &mut state,
                            &context,
                        ) {
                            break 'worker;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break 'worker,
                }
            }
            for trigger in receivers.observations.try_iter() {
                if !handle_observation_trigger(trigger, Instant::now(), api, &mut state, &context) {
                    break 'worker;
                }
            }
            if !service_on_demand(Instant::now(), api, &mut state, &context) {
                break 'worker;
            }
        }
    }
}

pub(super) fn handle_observation_trigger<A: ChromeApi>(
    trigger: ObservationTrigger,
    now: Instant,
    api: &mut A,
    state: &mut ChromeWorkerState,
    context: &ObservationContext<'_>,
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
        ObservationTrigger::PageLoaded { pid } => observe_frontmost(pid, now, api, state, context),
    }
}

pub(super) fn service_on_demand<A: ChromeApi>(
    now: Instant,
    api: &mut A,
    state: &mut ChromeWorkerState,
    context: &ObservationContext<'_>,
) -> bool {
    let Some((&key, &deadline)) = state.on_demand.iter().min_by_key(|(_, deadline)| *deadline)
    else {
        return true;
    };
    if now < deadline {
        return true;
    }
    state.on_demand.remove(&key);
    observe_confirmation(key, now, api, state, context)
}

#[derive(Default)]
pub(super) struct ChromeWorkerState {
    pub(super) navigation: NavigationTracker,
    pub(super) frontmost: Option<FocusSnapshot>,
    pub(super) apps: HashMap<i64, ApplicationInfo>,
    pub(super) on_demand: HashMap<(i64, i64), Instant>,
    pub(super) last_focus_generation: Option<u64>,
}

pub(super) fn handle_focus_transition<A: ChromeApi>(
    transition: FocusTransition,
    observed_at: Instant,
    api: &mut A,
    state: &mut ChromeWorkerState,
    context: &ObservationContext<'_>,
) -> bool {
    if let Some(generation) = transition
        .current
        .as_ref()
        .map(|current| current.generation)
    {
        if state
            .last_focus_generation
            .is_some_and(|seen| generation <= seen)
        {
            return true;
        }
        state.last_focus_generation = Some(generation);
    }
    // A wake resync is the single ordering boundary for Chrome state: invalidate
    // stale eligibility, then immediately rebuild it from the re-read focus.
    if transition.resynced {
        context.eligibility.clear_all();
        state.navigation.clear();
    }
    let Some(current) = transition.current else {
        if let Some(previous) = transition
            .previous
            .filter(|focus| browser_target(&focus.app).is_some())
        {
            context.eligibility.observe(
                previous.app.pid,
                ChromeEligibilityObservation::Unavailable { window_id: None },
            );
            terminate_chrome(previous.app.pid, state);
        } else {
            leave_chrome_focus(state);
        }
        return true;
    };
    if browser_target(&current.app)
        .is_none_or(|target| context.eligibility.query_revision(target).is_none())
    {
        leave_chrome_focus(state);
        return true;
    }
    let pid = current.app.pid;
    state.apps.insert(pid, current.app.clone());
    state.frontmost = Some(current);
    observe_frontmost(pid, observed_at, api, state, context)
}

fn leave_chrome_focus(state: &mut ChromeWorkerState) {
    state.frontmost = None;
    state.navigation.reset_page();
}

fn terminate_chrome(pid: i64, state: &mut ChromeWorkerState) {
    state.frontmost = None;
    state.apps.remove(&pid);
    state
        .on_demand
        .retain(|(candidate_pid, _), _| *candidate_pid != pid);
    state.navigation.reset_page();
}

fn observe_frontmost<A: ChromeApi>(
    pid: i64,
    observed_at: Instant,
    api: &mut A,
    state: &mut ChromeWorkerState,
    context: &ObservationContext<'_>,
) -> bool {
    let Some(focus) = state
        .frontmost
        .as_ref()
        .filter(|focus| focus.app.pid == pid)
        .cloned()
    else {
        return true;
    };
    let Some(target) = browser_target(&focus.app) else {
        return true;
    };
    let query = ChromeQuery::FrontWindow {
        target,
        pid,
        window_id: focus.window.as_ref().and_then(|window| window.id),
    };
    match observe_query_once(
        api,
        &mut state.navigation,
        Some(&focus.app),
        query,
        true,
        observed_at,
        context,
    ) {
        ObservationOutcome::Continue => true,
        ObservationOutcome::Inactive => {
            terminate_chrome(pid, state);
            true
        }
        ObservationOutcome::Stop => false,
    }
}

fn observe_confirmation<A: ChromeApi>(
    (pid, window_id): (i64, i64),
    observed_at: Instant,
    api: &mut A,
    state: &mut ChromeWorkerState,
    context: &ObservationContext<'_>,
) -> bool {
    let Some(app) = state.apps.get(&pid) else {
        return true;
    };
    let Some(target) = browser_target(app) else {
        return true;
    };
    let query = context
        .eligibility
        .applescript_window_id(pid, window_id)
        .map_or(
            ChromeQuery::FrontWindow {
                target,
                pid,
                window_id: Some(window_id),
            },
            |applescript_window_id| ChromeQuery::Window {
                target,
                pid,
                window_id,
                applescript_window_id,
            },
        );
    match observe_query_once(
        api,
        &mut state.navigation,
        Some(app),
        query,
        false,
        observed_at,
        context,
    ) {
        ObservationOutcome::Continue => true,
        ObservationOutcome::Inactive => {
            terminate_chrome(pid, state);
            true
        }
        ObservationOutcome::Stop => false,
    }
}

fn browser_target(app: &ApplicationInfo) -> Option<BrowserTarget> {
    BrowserTarget::from_bundle_id(app.bundle_id.as_deref())
}

pub(super) enum ObservationOutcome {
    Continue,
    Inactive,
    Stop,
}

pub(super) struct ObservationContext<'a> {
    pub(super) sender: &'a SyncSender<RawEvent>,
    pub(super) stop: &'a AtomicBool,
    pub(super) focus_context: &'a FocusContext,
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
        stop,
        focus_context,
        metrics,
        eligibility,
    } = context;
    let pid = query.pid();
    let target = query.target();
    let attribution = FrontWindowAttribution::capture(query.clone(), focus_context);
    if stop.load(Ordering::Acquire) {
        return ObservationOutcome::Stop;
    }
    if !attribution.allows(focus_context) {
        return ObservationOutcome::Continue;
    }
    let Some(revision) = eligibility.query_revision(target) else {
        return ObservationOutcome::Continue;
    };
    let observation = api.query(&query);
    if stop.load(Ordering::Acquire) {
        return ObservationOutcome::Stop;
    }
    // Native I/O is finished. Keep this gate through try_send so a reload
    // cannot interleave eligibility registration with URL publication.
    let Some(mut publication) = eligibility.publication(revision) else {
        return ObservationOutcome::Continue;
    };
    if stop.load(Ordering::Acquire) {
        return ObservationOutcome::Stop;
    }
    if !attribution.allows(focus_context) {
        publication.observe(
            pid,
            ChromeEligibilityObservation::Unavailable {
                window_id: query.window_id(),
            },
            None,
            None,
            observed_at,
        );
        return ObservationOutcome::Continue;
    }
    match observation {
        Ok(ChromeObservation::Snapshot(snapshot)) => {
            if let Err(error) = validate_query_snapshot(&query, &snapshot) {
                return record_failure(
                    tracker,
                    &query,
                    ChromeFailure::Validation(error.into()),
                    observed_at,
                    context,
                    &mut publication,
                );
            }
            if emit_navigation && app.is_none() {
                return record_failure(
                    tracker,
                    &query,
                    ChromeFailure::Validation(ChromeValidationFailure::MissingApplication),
                    observed_at,
                    context,
                    &mut publication,
                );
            }
            let url_allowed = snapshot
                .page
                .url()
                .is_some_and(|url| publication.allows_url(target, url));
            if emit_navigation && !url_allowed {
                tracker.reset_page();
            }
            let navigation = if emit_navigation && url_allowed {
                match tracker.observe(snapshot.clone()) {
                    Ok(navigation) => navigation,
                    Err(error) => {
                        return record_failure(
                            tracker,
                            &query,
                            ChromeFailure::Validation(error.into()),
                            observed_at,
                            context,
                            &mut publication,
                        );
                    }
                }
            } else {
                None
            };
            metrics.failure.observe_success(target);
            publication.observe(
                pid,
                match &snapshot.page {
                    BrowserPage::Chrome { url, .. } => ChromeEligibilityObservation::Normal {
                        window_id: snapshot.window_id,
                        url: url.clone(),
                    },
                    BrowserPage::Safari { url } => ChromeEligibilityObservation::Safari {
                        window_id: snapshot.window_id,
                        url: url.clone(),
                    },
                },
                Some(snapshot.applescript_window_id.clone()),
                snapshot.page.tab_key(),
                observed_at,
            );
            let (Some(navigation), Some(app)) = (navigation, app) else {
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
            metrics.failure.observe_success(target);
            publication.observe(
                pid,
                ChromeEligibilityObservation::Incognito { window_id },
                query.applescript_window_id().cloned(),
                None,
                observed_at,
            );
            if emit_navigation {
                tracker.reset_page();
            }
            ObservationOutcome::Continue
        }
        Ok(ChromeObservation::NoWindow) => {
            metrics.failure.observe_success(target);
            publication.observe(
                pid,
                ChromeEligibilityObservation::Unavailable {
                    window_id: query.window_id(),
                },
                None,
                None,
                observed_at,
            );
            if emit_navigation {
                tracker.reset_page();
            }
            ObservationOutcome::Continue
        }
        Ok(ChromeObservation::NotRunning) => {
            metrics.failure.observe_success(target);
            publication.observe(
                pid,
                ChromeEligibilityObservation::Unavailable { window_id: None },
                None,
                None,
                observed_at,
            );
            tracker.reset_page();
            ObservationOutcome::Inactive
        }
        Err(error) => record_failure(
            tracker,
            &query,
            error,
            observed_at,
            context,
            &mut publication,
        ),
    }
}

fn record_failure(
    tracker: &mut NavigationTracker,
    query: &ChromeQuery,
    failure: ChromeFailure,
    observed_at: Instant,
    context: &ObservationContext<'_>,
    publication: &mut EligibilityPublication<'_>,
) -> ObservationOutcome {
    publication.observe(
        query.pid(),
        ChromeEligibilityObservation::Unavailable { window_id: None },
        None,
        None,
        observed_at,
    );
    tracker.reset_page();
    context.metrics.degraded.fetch_add(1, Ordering::Relaxed);
    context
        .metrics
        .failure
        .observe_failure(query.target(), failure);
    ObservationOutcome::Continue
}

mod validation;

pub(super) use validation::SnapshotError;
use validation::{validate_query_snapshot, validate_snapshot};

impl From<SnapshotError> for ChromeValidationFailure {
    fn from(error: SnapshotError) -> Self {
        match error {
            SnapshotError::EmptyWindowIdentity => Self::EmptyWindowIdentity,
            SnapshotError::EmptyTabIdentity => Self::EmptyTabIdentity,
            SnapshotError::WindowIdentityMismatch => Self::WindowIdentityMismatch,
            SnapshotError::InvalidUrl => Self::InvalidUrl,
        }
    }
}
