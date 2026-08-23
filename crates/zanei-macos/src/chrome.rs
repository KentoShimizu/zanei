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
    workspace::WorkspaceEvent,
};

use zanei_core::privacy::CHROME_BUNDLE_ID;
const COLLECTOR_NAME: &str = "chrome";
use observer::ObservationTrigger;
use worker::{ChromeWorkerReceivers, run_worker};

#[cfg(test)]
use worker::{
    ChromeWorkerState, EVENT_SOURCE, EVENT_TYPE, NavigationTracker, ObservationOutcome,
    SnapshotError, handle_focus_transition, handle_observation_trigger, handle_workspace_event,
    observe_once, service_on_demand,
};
pub struct ChromeCollector {
    workspace_events: Option<Receiver<WorkspaceEvent>>,
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
        workspace_events: Receiver<WorkspaceEvent>,
        eligibility: ChromeEligibilityPublisher,
        focus_context: FocusContext,
        observer: ChromeObserver,
    ) -> Self {
        let focus_transitions = focus_context.subscribe();
        let observation_triggers = observer.subscribe();
        Self {
            workspace_events: Some(workspace_events),
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
        self.eligibility.clear_all();
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        runtime.stop.store(true, Ordering::Release);
        if let Ok((workspace_events, focus_transitions, observation_triggers)) =
            runtime.handle.join()
        {
            self.workspace_events = Some(workspace_events);
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
        let workspace_events =
            self.workspace_events
                .take()
                .ok_or_else(|| CollectorError::Start {
                    collector: COLLECTOR_NAME.to_owned(),
                    message: "workspace event receiver is unavailable".to_owned(),
                })?;
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
                        SystemChromeApi {
                            client,
                            focus_context,
                        }
                    }
                    Err(error) => {
                        metrics.degraded.fetch_add(1, Ordering::Relaxed);
                        let _ = startup_sender.send(Err(error.to_string()));
                        return (workspace_events, focus_transitions, observation_triggers);
                    }
                };
                let receivers = ChromeWorkerReceivers {
                    workspace: &workspace_events,
                    focus: &focus_transitions,
                    observations: &observation_triggers,
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
                    }),
                );
                (workspace_events, focus_transitions, observation_triggers)
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
                if let Ok((workspace, focus, triggers)) = handle.join() {
                    self.workspace_events = Some(workspace);
                    self.focus_transitions = Some(focus);
                    self.observation_triggers = Some(triggers);
                }
                Err(CollectorError::Start {
                    collector: COLLECTOR_NAME.to_owned(),
                    message,
                })
            }
            Err(error) => {
                if let Ok((workspace, focus, triggers)) = handle.join() {
                    self.workspace_events = Some(workspace);
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
    handle: JoinHandle<(
        Receiver<WorkspaceEvent>,
        FocusTransitionReceiver,
        Receiver<ObservationTrigger>,
    )>,
}

#[derive(Clone, Default)]
struct ChromeMetrics {
    dropped: Arc<AtomicU64>,
    degraded: Arc<AtomicU64>,
}

trait ChromeApi {
    type Error: Display;

    fn query(&mut self, pid: i64) -> Result<ChromeObservation, Self::Error>;
}

struct SystemChromeApi {
    client: AppleScriptClient,
    focus_context: FocusContext,
}

impl ChromeApi for SystemChromeApi {
    type Error = AppleScriptError;

    fn query(&mut self, pid: i64) -> Result<ChromeObservation, Self::Error> {
        let observation = self.client.query()?;
        let window_id = self
            .focus_context
            .current()
            .filter(|focus| focus.app.pid == pid)
            .and_then(|focus| focus.window)
            .and_then(|window| window.id);
        Ok(match observation {
            NativeObservation::Snapshot(snapshot) => {
                ChromeObservation::Snapshot(ChromeSnapshot::from_native(snapshot, window_id))
            }
            NativeObservation::Incognito => ChromeObservation::Incognito { window_id },
            NativeObservation::NotFrontmost => ChromeObservation::NotFrontmost,
            NativeObservation::NoWindow => ChromeObservation::NoWindow,
            NativeObservation::NotRunning => ChromeObservation::NotRunning,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChromeObservation {
    Snapshot(ChromeSnapshot),
    Incognito { window_id: Option<i64> },
    NotFrontmost,
    NoWindow,
    NotRunning,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChromeSnapshot {
    window_id: Option<i64>,
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
            window_key: value.window_key,
            window_title: value.window_title,
            tab_key: value.tab_key,
            url: value.url,
            tab_title: value.tab_title,
        }
    }
}

#[cfg(test)]
#[path = "chrome/tests.rs"]
mod tests;
