//! Chrome URL collection and OS-independent navigation change detection.

mod eligibility;

pub use eligibility::{
    ChromeEligibilityPublisher, ChromeEligibilityTracker, chrome_eligibility_channel,
};

use std::{
    fmt::Display,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use zanei_collector::{Collector, CollectorError, Permission, RawEvent};
use zanei_core::schema::{
    App, BrowserMode, BrowserNavigateData, BrowserTransition, EventData, Window,
};

use crate::{
    ffi::applescript::{
        AppleScriptClient, AppleScriptError, Observation as NativeObservation,
        Snapshot as NativeSnapshot,
    },
    ffi::eventtap::current_context,
    workspace::{ApplicationInfo, WorkspaceEvent},
};

use zanei_core::privacy::CHROME_BUNDLE_ID;
const COLLECTOR_NAME: &str = "chrome";
const EVENT_SOURCE: &str = "macos.applescript";
const EVENT_TYPE: &str = "browser.navigate";
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(100);
pub struct ChromeCollector {
    workspace_events: Option<Receiver<WorkspaceEvent>>,
    eligibility: ChromeEligibilityPublisher,
    runtime: Option<ChromeRuntime>,
    permissions: [Permission; 1],
    metrics: ChromeMetrics,
}
impl ChromeCollector {
    #[must_use]
    pub fn new(
        workspace_events: Receiver<WorkspaceEvent>,
        eligibility: ChromeEligibilityPublisher,
    ) -> Self {
        Self {
            workspace_events: Some(workspace_events),
            eligibility,
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
        if let Ok(workspace_events) = runtime.handle.join() {
            self.workspace_events = Some(workspace_events);
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
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let metrics = self.metrics.clone();
        let eligibility = self.eligibility.clone();
        let (startup_sender, startup_receiver) = sync_channel(1);
        let handle = thread::Builder::new()
            .name("zanei-chrome".to_owned())
            .spawn(move || {
                let mut api = match AppleScriptClient::new() {
                    Ok(api) => {
                        let _ = startup_sender.send(Ok(()));
                        api
                    }
                    Err(error) => {
                        metrics.degraded.fetch_add(1, Ordering::Relaxed);
                        let _ = startup_sender.send(Err(error.to_string()));
                        return workspace_events;
                    }
                };
                run_worker(
                    &mut api,
                    &workspace_events,
                    &sender,
                    &worker_stop,
                    &metrics,
                    &eligibility,
                );
                workspace_events
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
                self.workspace_events = handle.join().ok();
                Err(CollectorError::Start {
                    collector: COLLECTOR_NAME.to_owned(),
                    message,
                })
            }
            Err(error) => {
                self.workspace_events = handle.join().ok();
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
    handle: JoinHandle<Receiver<WorkspaceEvent>>,
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

impl ChromeApi for AppleScriptClient {
    type Error = AppleScriptError;

    fn query(&mut self, pid: i64) -> Result<ChromeObservation, Self::Error> {
        let observation = AppleScriptClient::query(self)?;
        let window_id = current_context()
            .filter(|context| context.app.pid == pid)
            .and_then(|context| context.window)
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

fn run_worker<A: ChromeApi>(
    api: &mut A,
    workspace_events: &Receiver<WorkspaceEvent>,
    sender: &SyncSender<RawEvent>,
    stop: &AtomicBool,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) {
    let mut state = ChromeWorkerState::default();

    while !stop.load(Ordering::Acquire) {
        let wait = state
            .next_poll
            .map_or(STOP_CHECK_INTERVAL, |deadline: Instant| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(STOP_CHECK_INTERVAL)
            });
        match workspace_events.recv_timeout(wait) {
            Ok(event) => {
                if !handle_workspace_event(event, api, sender, &mut state, metrics, eligibility) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let Some(deadline) = state.next_poll else {
                    continue;
                };
                if Instant::now() < deadline {
                    continue;
                }
                let Some(app) = state.frontmost.as_ref() else {
                    state.next_poll = None;
                    continue;
                };
                match poll_once(
                    api,
                    &mut state.navigation,
                    app,
                    sender,
                    metrics,
                    eligibility,
                ) {
                    PollOutcome::Continue => state.next_poll = Some(Instant::now() + POLL_INTERVAL),
                    PollOutcome::Inactive => {
                        state.frontmost = None;
                        state.next_poll = None;
                    }
                    PollOutcome::Stop => break,
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    eligibility.clear_all();
}

#[derive(Default)]
struct ChromeWorkerState {
    navigation: NavigationTracker,
    frontmost: Option<ApplicationInfo>,
    next_poll: Option<Instant>,
}

fn handle_workspace_event<A: ChromeApi>(
    event: WorkspaceEvent,
    api: &mut A,
    sender: &SyncSender<RawEvent>,
    state: &mut ChromeWorkerState,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> bool {
    match event {
        WorkspaceEvent::Activated(app) if is_chrome(&app) => {
            eligibility.clear_pid(app.pid);
            let outcome = poll_once(
                api,
                &mut state.navigation,
                &app,
                sender,
                metrics,
                eligibility,
            );
            match outcome {
                PollOutcome::Continue => {
                    state.frontmost = Some(app);
                    state.next_poll = Some(Instant::now() + POLL_INTERVAL);
                }
                PollOutcome::Inactive => {
                    state.frontmost = None;
                    state.next_poll = None;
                }
                PollOutcome::Stop => return false,
            }
        }
        WorkspaceEvent::Activated(_) => {
            if let Some(app) = state.frontmost.as_ref() {
                eligibility.clear_pid(app.pid);
            }
            state.frontmost = None;
            state.next_poll = None;
        }
        WorkspaceEvent::Terminated(app) if is_chrome(&app) => {
            eligibility.clear_pid(app.pid);
            state.frontmost = None;
            state.next_poll = None;
            state.navigation.reset();
        }
        WorkspaceEvent::DidWake => eligibility.clear_all(),
        WorkspaceEvent::Launched(_) | WorkspaceEvent::Terminated(_) => {}
    }
    true
}

fn is_chrome(app: &ApplicationInfo) -> bool {
    app.bundle_id.as_deref() == Some(CHROME_BUNDLE_ID)
}

enum PollOutcome {
    Continue,
    Inactive,
    Stop,
}

fn poll_once<A: ChromeApi>(
    api: &mut A,
    tracker: &mut NavigationTracker,
    app: &ApplicationInfo,
    sender: &SyncSender<RawEvent>,
    metrics: &ChromeMetrics,
    eligibility: &ChromeEligibilityPublisher,
) -> PollOutcome {
    eligibility.clear_pid(app.pid);
    match api.query(app.pid) {
        Ok(ChromeObservation::Snapshot(snapshot)) => {
            eligibility.publish_normal(app.pid, snapshot.window_id, &snapshot.url);
            let navigation = match tracker.observe(snapshot) {
                Ok(navigation) => navigation,
                Err(_) => {
                    eligibility.clear_pid(app.pid);
                    metrics.degraded.fetch_add(1, Ordering::Relaxed);
                    return PollOutcome::Stop;
                }
            };
            let Some(navigation) = navigation else {
                return PollOutcome::Continue;
            };
            match sender.try_send(raw_event(app, navigation)) {
                Ok(()) => PollOutcome::Continue,
                Err(TrySendError::Full(_)) => {
                    metrics.dropped.fetch_add(1, Ordering::Relaxed);
                    PollOutcome::Continue
                }
                Err(TrySendError::Disconnected(_)) => {
                    metrics.dropped.fetch_add(1, Ordering::Relaxed);
                    metrics.degraded.fetch_add(1, Ordering::Relaxed);
                    PollOutcome::Stop
                }
            }
        }
        Ok(ChromeObservation::Incognito { window_id }) => {
            eligibility.publish_incognito(app.pid, window_id);
            tracker.reset();
            PollOutcome::Continue
        }
        Ok(ChromeObservation::NoWindow) => {
            tracker.reset();
            PollOutcome::Continue
        }
        Ok(ChromeObservation::NotRunning) => {
            tracker.reset();
            PollOutcome::Inactive
        }
        Ok(ChromeObservation::NotFrontmost) => PollOutcome::Inactive,
        Err(_) => {
            metrics.degraded.fetch_add(1, Ordering::Relaxed);
            PollOutcome::Stop
        }
    }
}

fn raw_event(app: &ApplicationInfo, navigation: Navigation) -> RawEvent {
    let website_host = zanei_core::privacy::website_host(&navigation.snapshot.url);
    RawEvent {
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
struct NavigationTracker {
    previous: Option<ObservedPage>,
}

impl NavigationTracker {
    fn observe(&mut self, snapshot: ChromeSnapshot) -> Result<Option<Navigation>, SnapshotError> {
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

struct ObservedPage {
    window_key: String,
    tab_key: String,
    url: String,
}

struct Navigation {
    snapshot: ChromeSnapshot,
    transition: Option<BrowserTransition>,
}

#[derive(Debug, thiserror::Error)]
enum SnapshotError {
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

#[cfg(test)]
#[path = "chrome/tests.rs"]
mod tests;
