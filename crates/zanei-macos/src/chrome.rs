//! Chrome URL collection and OS-independent navigation change detection.

mod eligibility;
mod observer;
mod worker;

pub use eligibility::{
    ChromeEligibilityObservation, ChromeEligibilityPublisher, ChromeEligibilityTracker,
    chrome_eligibility_channel,
};
pub use observer::ChromeObserver;

use std::{
    fmt::Display,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
};

use zanei_collector::{Collector, CollectorError, Permission, RawEvent};
#[cfg(test)]
use zanei_core::schema::BrowserTransition;

use crate::{
    ffi::applescript::{
        AppleScriptClient, AppleScriptError, Observation as NativeObservation,
        Snapshot as NativeSnapshot,
    },
    focus_context::{FocusContext, FocusTransition, FocusTransitionReceiver},
};

use zanei_core::privacy::CHROME_BUNDLE_ID;
const COLLECTOR_NAME: &str = "chrome";
use observer::ObservationTrigger;
use worker::{ChromeWorkerReceivers, run_worker};

#[cfg(test)]
use worker::{
    ChromeWorkerState, EVENT_SOURCE, EVENT_TYPE, NavigationTracker, ObservationContext,
    ObservationOutcome, SnapshotError, handle_focus_transition as handle_focus_transition_impl,
    handle_observation_trigger as handle_observation_trigger_impl, observe_query_once,
    service_on_demand as service_on_demand_impl,
};
pub struct ChromeCollector {
    focus_transitions: Option<FocusTransitionReceiver>,
    observation_triggers: Option<Receiver<ObservationTrigger>>,
    eligibility: ChromeEligibilityPublisher,
    focus_context: FocusContext,
    runtime: Option<ChromeRuntime>,
    permissions: [Permission; 1],
    metrics: ChromeMetrics,
}
impl ChromeCollector {
    #[must_use]
    pub fn new(
        eligibility: ChromeEligibilityPublisher,
        focus_context: FocusContext,
        observer: ChromeObserver,
    ) -> Self {
        let focus_transitions = focus_context.subscribe();
        let observation_triggers = observer.subscribe();
        Self {
            focus_transitions: Some(focus_transitions),
            observation_triggers: Some(observation_triggers),
            eligibility,
            focus_context,
            runtime: None,
            permissions: [Permission::Automation {
                bundle_id: CHROME_BUNDLE_ID.to_owned(),
            }],
            metrics: ChromeMetrics::default(),
        }
    }

    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.metrics.dropped.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn degraded_operations(&self) -> u64 {
        self.metrics.degraded.load(Ordering::Relaxed)
    }

    fn stop_worker(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        runtime.stop.store(true, Ordering::Release);
        if let Ok((focus_transitions, observation_triggers)) = runtime.handle.join() {
            self.focus_transitions = Some(focus_transitions);
            self.observation_triggers = Some(observation_triggers);
        }
    }
}

impl Collector for ChromeCollector {
    fn name(&self) -> &str {
        COLLECTOR_NAME
    }

    fn required_permissions(&self) -> &[Permission] {
        &self.permissions
    }

    fn start(&mut self, sender: SyncSender<RawEvent>) -> Result<(), CollectorError> {
        if self.runtime.is_some() {
            return Err(CollectorError::AlreadyRunning {
                collector: COLLECTOR_NAME.to_owned(),
            });
        }
        let focus_transitions =
            self.focus_transitions
                .take()
                .ok_or_else(|| CollectorError::Start {
                    collector: COLLECTOR_NAME.to_owned(),
                    message: "focus transition receiver is unavailable".to_owned(),
                })?;
        let observation_triggers =
            self.observation_triggers
                .take()
                .ok_or_else(|| CollectorError::Start {
                    collector: COLLECTOR_NAME.to_owned(),
                    message: "Chrome observation trigger receiver is unavailable".to_owned(),
                })?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let metrics = self.metrics.clone();
        let eligibility = self.eligibility.clone();
        let focus_context = self.focus_context.clone();
        let initial_focus = focus_context.current();
        let (startup_sender, startup_receiver) = sync_channel(1);
        let handle = thread::Builder::new()
            .name("zanei-chrome".to_owned())
            .spawn(move || {
                let mut api = match AppleScriptClient::new() {
                    Ok(client) => {
                        let _ = startup_sender.send(Ok(()));
                        SystemChromeApi { client }
                    }
                    Err(error) => {
                        metrics.degraded.fetch_add(1, Ordering::Relaxed);
                        let _ = startup_sender.send(Err(error.to_string()));
                        eligibility.clear_all();
                        return (focus_transitions, observation_triggers);
                    }
                };
                let receivers = ChromeWorkerReceivers {
                    focus: &focus_transitions,
                    observations: &observation_triggers,
                    focus_context: &focus_context,
                };
                run_worker(
                    &mut api,
                    &receivers,
                    &sender,
                    &worker_stop,
                    &metrics,
                    &eligibility,
                    initial_focus.map(|focus| FocusTransition {
                        previous: None,
                        current: Some(focus),
                        resynced: false,
                    }),
                );
                (focus_transitions, observation_triggers)
            })
            .map_err(|error| CollectorError::Start {
                collector: COLLECTOR_NAME.to_owned(),
                message: error.to_string(),
            })?;

        match startup_receiver.recv() {
            Ok(Ok(())) => {
                self.runtime = Some(ChromeRuntime { stop, handle });
                Ok(())
            }
            Ok(Err(message)) => {
                if let Ok((focus, triggers)) = handle.join() {
                    self.focus_transitions = Some(focus);
                    self.observation_triggers = Some(triggers);
                }
                Err(CollectorError::Start {
                    collector: COLLECTOR_NAME.to_owned(),
                    message,
                })
            }
            Err(error) => {
                if let Ok((focus, triggers)) = handle.join() {
                    self.focus_transitions = Some(focus);
                    self.observation_triggers = Some(triggers);
                }
                Err(CollectorError::Start {
                    collector: COLLECTOR_NAME.to_owned(),
                    message: error.to_string(),
                })
            }
        }
    }

    fn stop(&mut self) {
        self.stop_worker();
    }
}

impl Drop for ChromeCollector {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

struct ChromeRuntime {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<(FocusTransitionReceiver, Receiver<ObservationTrigger>)>,
}

#[derive(Clone, Default)]
struct ChromeMetrics {
    dropped: Arc<AtomicU64>,
    degraded: Arc<AtomicU64>,
}

trait ChromeApi {
    type Error: Display;

    fn query(&mut self, query: ChromeQuery) -> Result<ChromeObservation, Self::Error>;
}

struct SystemChromeApi {
    client: AppleScriptClient,
}

impl ChromeApi for SystemChromeApi {
    type Error = AppleScriptError;

    fn query(&mut self, query: ChromeQuery) -> Result<ChromeObservation, Self::Error> {
        let observation = match query {
            ChromeQuery::FrontWindow { .. } => self.client.query()?,
            ChromeQuery::Window {
                applescript_window_id,
                ..
            } => self.client.query_window(applescript_window_id)?,
        };
        let window_id = query.window_id();
        Ok(match observation {
            NativeObservation::Snapshot(snapshot) => {
                ChromeObservation::Snapshot(ChromeSnapshot::from_native(snapshot, window_id)?)
            }
            NativeObservation::Incognito => ChromeObservation::Incognito { window_id },
            NativeObservation::NoWindow => ChromeObservation::NoWindow,
            NativeObservation::NotRunning => ChromeObservation::NotRunning,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChromeQuery {
    FrontWindow {
        pid: i64,
        window_id: Option<i64>,
    },
    Window {
        pid: i64,
        window_id: i64,
        applescript_window_id: i64,
    },
}

impl ChromeQuery {
    const fn pid(self) -> i64 {
        match self {
            Self::FrontWindow { pid, .. } | Self::Window { pid, .. } => pid,
        }
    }

    const fn window_id(self) -> Option<i64> {
        match self {
            Self::FrontWindow { window_id, .. } => window_id,
            Self::Window { window_id, .. } => Some(window_id),
        }
    }

    const fn applescript_window_id(self) -> Option<i64> {
        match self {
            Self::Window {
                applescript_window_id,
                ..
            } => Some(applescript_window_id),
            Self::FrontWindow { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChromeObservation {
    Snapshot(ChromeSnapshot),
    Incognito { window_id: Option<i64> },
    NoWindow,
    NotRunning,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChromeSnapshot {
    window_id: Option<i64>,
    applescript_window_id: i64,
    window_key: String,
    window_title: Option<String>,
    tab_key: String,
    url: String,
    tab_title: Option<String>,
}

impl ChromeSnapshot {
    fn from_native(
        value: NativeSnapshot,
        window_id: Option<i64>,
    ) -> Result<Self, AppleScriptError> {
        let applescript_window_id = value
            .window_key
            .parse()
            .map_err(|_| AppleScriptError::InvalidResponse("Chrome window id is not an integer"))?;
        Ok(Self {
            window_id,
            applescript_window_id,
            window_key: value.window_key,
            window_title: value.window_title,
            tab_key: value.tab_key,
            url: value.url,
            tab_title: value.tab_title,
        })
    }
}

#[cfg(test)]
fn handle_focus_transition<A: ChromeApi>(
    transition: FocusTransition,
    observed_at: std::time::Instant,
    api: &mut A,
    sender: &SyncSender<RawEvent>,
    state: &mut ChromeWorkerState,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> bool {
    let focus_context = FocusContext::new();
    if let Some(current) = transition.current.as_ref() {
        focus_context.activate(current.app.clone(), current.window.clone());
    }
    let stop = AtomicBool::new(false);
    let context = ObservationContext {
        sender,
        stop: &stop,
        focus_context: &focus_context,
        metrics,
        eligibility,
    };
    handle_focus_transition_impl(transition, observed_at, api, state, &context)
}

#[cfg(test)]
fn handle_observation_trigger<A: ChromeApi>(
    trigger: ObservationTrigger,
    now: std::time::Instant,
    api: &mut A,
    sender: &SyncSender<RawEvent>,
    state: &mut ChromeWorkerState,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> bool {
    let focus_context = test_focus_context(state.frontmost.as_ref());
    let stop = AtomicBool::new(false);
    let context = ObservationContext {
        sender,
        stop: &stop,
        focus_context: &focus_context,
        metrics,
        eligibility,
    };
    handle_observation_trigger_impl(trigger, now, api, state, &context)
}

#[cfg(test)]
fn service_on_demand<A: ChromeApi>(
    now: std::time::Instant,
    api: &mut A,
    sender: &SyncSender<RawEvent>,
    state: &mut ChromeWorkerState,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> bool {
    let focus_context = test_focus_context(state.frontmost.as_ref());
    let stop = AtomicBool::new(false);
    let context = ObservationContext {
        sender,
        stop: &stop,
        focus_context: &focus_context,
        metrics,
        eligibility,
    };
    service_on_demand_impl(now, api, state, &context)
}

#[cfg(test)]
fn test_focus_context(focus: Option<&crate::focus_context::FocusSnapshot>) -> FocusContext {
    let context = FocusContext::new();
    if let Some(focus) = focus {
        context.activate(focus.app.clone(), focus.window.clone());
    }
    context
}

#[cfg(test)]
#[path = "chrome/tests.rs"]
mod tests;
