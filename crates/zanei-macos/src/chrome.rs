//! Chrome URL collection and OS-independent navigation change detection.

mod eligibility;
mod failure;
mod observer;
mod worker;

pub use eligibility::{
    ChromeEligibilityObservation, ChromeEligibilityPublisher, ChromeEligibilityTracker,
    chrome_eligibility_channel,
};
pub use failure::{
    ChromeFailure, ChromeFailurePhase, ChromeFailureState, ChromeParseFailure, ChromeQueryFailure,
    ChromeValidationFailure,
};
pub use observer::ChromeObserver;

use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
};

use zanei_collector::{Collector, CollectorError, Permission, RawEvent};
#[cfg(test)]
use zanei_core::schema::BrowserTransition;

use crate::{
    ffi::applescript::{
        AppleScriptClient, AppleScriptError, AppleScriptResponseError,
        Observation as NativeObservation, Snapshot as NativeSnapshot,
    },
    focus_context::{FocusContext, FocusTransition, FocusTransitionReceiver},
};

use zanei_core::privacy::CHROME_BUNDLE_ID;
const COLLECTOR_NAME: &str = "chrome";
use failure::ChromeFailurePublisher;
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
    channels: Arc<Mutex<ChromeWorkerChannels>>,
    eligibility: ChromeEligibilityPublisher,
    focus_context: FocusContext,
    runtime: Option<ChromeRuntime>,
    permissions: [Permission; 1],
    metrics: ChromeMetrics,
    #[cfg(test)]
    panic_next_worker: Arc<AtomicBool>,
}

struct ChromeWorkerChannels {
    focus_transitions: FocusTransitionReceiver,
    observation_triggers: Receiver<ObservationTrigger>,
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
            channels: Arc::new(Mutex::new(ChromeWorkerChannels {
                focus_transitions,
                observation_triggers,
            })),
            eligibility,
            focus_context,
            runtime: None,
            permissions: [Permission::Automation {
                bundle_id: CHROME_BUNDLE_ID.to_owned(),
            }],
            metrics: ChromeMetrics::default(),
            #[cfg(test)]
            panic_next_worker: Arc::new(AtomicBool::new(false)),
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

    #[must_use]
    pub fn failure_state(&self) -> ChromeFailureState {
        self.metrics.failure.state()
    }

    fn stop_worker(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        runtime.stop.store(true, Ordering::Release);
        let _ = runtime.handle.join();
    }

    #[cfg(test)]
    fn panic_next_worker_for_test(&self) {
        self.panic_next_worker.store(true, Ordering::Release);
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
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let metrics = self.metrics.clone();
        let eligibility = self.eligibility.clone();
        let focus_context = self.focus_context.clone();
        let initial_focus = focus_context.current();
        let channels = Arc::clone(&self.channels);
        #[cfg(test)]
        let panic_next_worker = Arc::clone(&self.panic_next_worker);
        let handle = thread::Builder::new()
            .name("zanei-chrome".to_owned())
            .spawn(move || {
                let channels = lock_worker_channels(&channels);
                let _clear_eligibility = EligibilityClearGuard::new(&eligibility);
                #[cfg(test)]
                if panic_next_worker.swap(false, Ordering::AcqRel) {
                    panic!("injected Chrome worker panic");
                }
                let mut api = SystemChromeApi { client: None };
                let receivers = ChromeWorkerReceivers {
                    focus: &channels.focus_transitions,
                    observations: &channels.observation_triggers,
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
            })
            .map_err(|error| CollectorError::Start {
                collector: COLLECTOR_NAME.to_owned(),
                message: error.to_string(),
            })?;
        self.runtime = Some(ChromeRuntime { stop, handle });
        Ok(())
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
    handle: JoinHandle<()>,
}

struct EligibilityClearGuard<'a> {
    eligibility: &'a ChromeEligibilityPublisher,
}

impl<'a> EligibilityClearGuard<'a> {
    const fn new(eligibility: &'a ChromeEligibilityPublisher) -> Self {
        Self { eligibility }
    }
}

impl Drop for EligibilityClearGuard<'_> {
    fn drop(&mut self) {
        self.eligibility.clear_all();
    }
}

fn lock_worker_channels(
    channels: &Mutex<ChromeWorkerChannels>,
) -> MutexGuard<'_, ChromeWorkerChannels> {
    channels
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Default)]
struct ChromeMetrics {
    dropped: Arc<AtomicU64>,
    degraded: Arc<AtomicU64>,
    failure: ChromeFailurePublisher,
}

trait ChromeApi {
    fn query(&mut self, query: &ChromeQuery) -> Result<ChromeObservation, ChromeFailure>;
}

struct SystemChromeApi<C = AppleScriptClient> {
    client: Option<C>,
}

impl ChromeApi for SystemChromeApi {
    fn query(&mut self, query: &ChromeQuery) -> Result<ChromeObservation, ChromeFailure> {
        let client = self.client()?;
        let observation = match query {
            ChromeQuery::FrontWindow { .. } => client.query()?,
            ChromeQuery::Window {
                applescript_window_id,
                ..
            } => client.query_window(applescript_window_id)?,
        };
        let window_id = query.window_id();
        Ok(match observation {
            NativeObservation::Snapshot(snapshot) => {
                ChromeObservation::Snapshot(ChromeSnapshot::from_native(snapshot, window_id))
            }
            NativeObservation::Incognito => ChromeObservation::Incognito { window_id },
            NativeObservation::NoWindow => ChromeObservation::NoWindow,
            NativeObservation::NotRunning => ChromeObservation::NotRunning,
        })
    }
}

impl SystemChromeApi {
    fn client(&mut self) -> Result<&mut AppleScriptClient, ChromeFailure> {
        self.get_or_initialize_client(AppleScriptClient::new)
    }
}

impl<C> SystemChromeApi<C> {
    fn get_or_initialize_client(
        &mut self,
        initialize: impl FnOnce() -> Result<C, AppleScriptError>,
    ) -> Result<&mut C, ChromeFailure> {
        if self.client.is_none() {
            self.client = Some(initialize().map_err(ChromeFailure::from)?);
        }
        self.client
            .as_mut()
            .ok_or(ChromeFailure::Query(ChromeQueryFailure::RuntimeUnavailable))
    }

    #[cfg(test)]
    fn client_for_test(
        &mut self,
        result: Result<C, AppleScriptError>,
    ) -> Result<&mut C, ChromeFailure> {
        self.get_or_initialize_client(|| result)
    }
}

#[cfg(test)]
#[test]
fn system_chrome_api_retries_failed_client_initialization() {
    let mut api = SystemChromeApi::<()> { client: None };

    assert!(matches!(
        api.client_for_test(Err(AppleScriptError::ChromeUnavailable)),
        Err(ChromeFailure::Query(ChromeQueryFailure::RuntimeUnavailable))
    ));
    assert!(api.client_for_test(Ok(())).is_ok());
    let cached = api.client_for_test(Err(AppleScriptError::ChromeUnavailable));
    assert!(cached.is_ok());
}

impl From<AppleScriptError> for ChromeFailure {
    fn from(error: AppleScriptError) -> Self {
        match error {
            AppleScriptError::Compile { code } | AppleScriptError::Execute { code } => {
                Self::Query(code.map_or(
                    ChromeQueryFailure::AppleEventCodeUnavailable,
                    ChromeQueryFailure::AppleEvent,
                ))
            }
            AppleScriptError::ClassUnavailable(_)
            | AppleScriptError::Allocation(_)
            | AppleScriptError::ChromeUnavailable => {
                Self::Query(ChromeQueryFailure::RuntimeUnavailable)
            }
            AppleScriptError::InvalidResponse(error) => Self::Parse(match error {
                AppleScriptResponseError::EmptyDescriptorList => ChromeParseFailure::EmptyResponse,
                AppleScriptResponseError::UnsupportedModeLength
                | AppleScriptResponseError::StatusLength
                | AppleScriptResponseError::SnapshotLength => {
                    ChromeParseFailure::InvalidResponseShape
                }
                AppleScriptResponseError::UnknownStatus => ChromeParseFailure::UnknownStatus,
                AppleScriptResponseError::RequiredItemNotText => ChromeParseFailure::MissingText,
                AppleScriptResponseError::StringContainsNul => ChromeParseFailure::InvalidString,
            }),
            AppleScriptError::UnsupportedMode => {
                Self::Parse(ChromeParseFailure::UnsupportedWindowMode)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChromeQuery {
    FrontWindow {
        pid: i64,
        window_id: Option<i64>,
    },
    Window {
        pid: i64,
        window_id: i64,
        applescript_window_id: String,
    },
}

impl ChromeQuery {
    const fn pid(&self) -> i64 {
        match self {
            Self::FrontWindow { pid, .. } | Self::Window { pid, .. } => *pid,
        }
    }

    const fn window_id(&self) -> Option<i64> {
        match self {
            Self::FrontWindow { window_id, .. } => *window_id,
            Self::Window { window_id, .. } => Some(*window_id),
        }
    }

    fn applescript_window_id(&self) -> Option<&str> {
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
    applescript_window_id: String,
    window_key: String,
    window_title: Option<String>,
    tab_key: String,
    url: String,
    tab_title: Option<String>,
}

impl ChromeSnapshot {
    fn from_native(value: NativeSnapshot, window_id: Option<i64>) -> Self {
        Self {
            window_id,
            applescript_window_id: value.window_key.clone(),
            window_key: value.window_key,
            window_title: value.window_title,
            tab_key: value.tab_key,
            url: value.url,
            tab_title: value.tab_title,
        }
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
