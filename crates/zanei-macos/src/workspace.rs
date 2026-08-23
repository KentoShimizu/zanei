//! `NSWorkspace` application lifecycle collector.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use zanei_collector::{Collector, CollectorError, Permission, RawEvent};
use zanei_core::schema::{App, EmptyData, EventData, Window};

use crate::ffi::workspace::{
    NativeApplication, NativeApplicationActivationPolicy, NativeWorkspaceEvent,
    NativeWorkspaceEvents, NativeWorkspaceObserver,
};

const WORKSPACE_CHANNEL_CAPACITY: usize = 256;
const RUN_LOOP_SLICE: Duration = Duration::from_millis(100);
const NO_PERMISSIONS: [Permission; 0] = [];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(isize)]
pub enum ApplicationActivationPolicy {
    Regular = 0,
    Accessory = 1,
    Prohibited = 2,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationInfo {
    pub name: String,
    pub bundle_id: Option<String>,
    pub pid: i64,
    pub activation_policy: ApplicationActivationPolicy,
}

impl ApplicationInfo {
    pub(crate) fn raw_app(&self) -> App {
        App {
            name: self.name.clone(),
            bundle_id: self.bundle_id.clone(),
            pid: Some(self.pid),
        }
    }
}

impl From<NativeApplication> for ApplicationInfo {
    fn from(app: NativeApplication) -> Self {
        Self {
            name: app.name,
            bundle_id: app.bundle_id,
            pid: i64::from(app.pid),
            activation_policy: match app.activation_policy {
                NativeApplicationActivationPolicy::Regular => ApplicationActivationPolicy::Regular,
                NativeApplicationActivationPolicy::Accessory => {
                    ApplicationActivationPolicy::Accessory
                }
                NativeApplicationActivationPolicy::Prohibited => {
                    ApplicationActivationPolicy::Prohibited
                }
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceEvent {
    Activated(ApplicationInfo),
    Launched(ApplicationInfo),
    Terminated(ApplicationInfo),
    DidWake,
}

#[must_use]
pub fn notification_channel() -> (SyncSender<WorkspaceEvent>, Receiver<WorkspaceEvent>) {
    sync_channel(WORKSPACE_CHANNEL_CAPACITY)
}

pub struct WorkspaceCollector {
    subscribers: Vec<SyncSender<WorkspaceEvent>>,
    events: Option<NativeWorkspaceEvents>,
    worker: Option<Worker>,
    dropped_events: Arc<AtomicU64>,
}

pub struct WorkspaceObserver {
    _native: NativeWorkspaceObserver,
}

impl WorkspaceCollector {
    #[must_use]
    pub fn new(subscribers: Vec<SyncSender<WorkspaceEvent>>) -> Self {
        Self {
            subscribers,
            events: None,
            worker: None,
            dropped_events: Arc::new(AtomicU64::new(0)),
        }
    }

    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    pub fn prepare_main_thread(&mut self) -> Result<WorkspaceObserver, CollectorError> {
        if self.events.is_some() || self.worker.is_some() {
            return Err(CollectorError::AlreadyRunning {
                collector: self.name().to_owned(),
            });
        }
        let (observer, events) =
            NativeWorkspaceObserver::new().map_err(|error| CollectorError::Start {
                collector: self.name().to_owned(),
                message: error.to_string(),
            })?;
        self.events = Some(events);
        Ok(WorkspaceObserver { _native: observer })
    }
}

impl Default for WorkspaceCollector {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Collector for WorkspaceCollector {
    fn name(&self) -> &str {
        "workspace"
    }

    fn required_permissions(&self) -> &[Permission] {
        &NO_PERMISSIONS
    }

    fn start(&mut self, sender: SyncSender<RawEvent>) -> Result<(), CollectorError> {
        if self.worker.is_some() {
            return Err(CollectorError::AlreadyRunning {
                collector: self.name().to_owned(),
            });
        }

        let mut events = self.events.take().ok_or_else(|| CollectorError::Start {
            collector: self.name().to_owned(),
            message: "workspace observer was not prepared on the main thread".to_owned(),
        })?;
        events.enable();
        let enabled = events.enabled_flag();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let subscribers = self.subscribers.clone();
        let dropped_events = Arc::clone(&self.dropped_events);
        let handle = thread::Builder::new()
            .name("zanei-workspace".to_owned())
            .spawn(move || {
                run_workspace(
                    &mut events,
                    &thread_stop,
                    &sender,
                    &subscribers,
                    &dropped_events,
                );
                events.disable();
                events
            })
            .map_err(|error| CollectorError::Start {
                collector: self.name().to_owned(),
                message: error.to_string(),
            })?;
        self.worker = Some(Worker {
            stop,
            enabled,
            handle,
        });
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.enabled.store(false, Ordering::Release);
            worker.stop.store(true, Ordering::Release);
            if let Ok(events) = worker.handle.join() {
                self.dropped_events
                    .fetch_add(events.take_dropped_events(), Ordering::Relaxed);
                self.events = Some(events);
            }
        }
    }
}

impl Drop for WorkspaceCollector {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Worker {
    stop: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    handle: JoinHandle<NativeWorkspaceEvents>,
}

trait WorkspaceApi {
    fn poll(&mut self, timeout: Duration) -> Vec<NativeWorkspaceEvent>;
    fn frontmost_application(&self) -> Option<NativeApplication>;
    fn front_window(&self, pid: i64) -> Option<Window>;
    fn take_dropped_events(&self) -> u64;
}

impl WorkspaceApi for NativeWorkspaceEvents {
    fn poll(&mut self, timeout: Duration) -> Vec<NativeWorkspaceEvent> {
        NativeWorkspaceEvents::poll(self, timeout)
    }

    fn frontmost_application(&self) -> Option<NativeApplication> {
        NativeWorkspaceEvents::frontmost_application(self)
    }

    fn front_window(&self, pid: i64) -> Option<Window> {
        crate::ffi::window_list::front_window(pid).map(|window| Window {
            title: window.title,
            id: window.id,
        })
    }

    fn take_dropped_events(&self) -> u64 {
        NativeWorkspaceEvents::take_dropped_events(self)
    }
}

fn run_workspace(
    runtime: &mut impl WorkspaceApi,
    stop: &AtomicBool,
    sender: &SyncSender<RawEvent>,
    subscribers: &[SyncSender<WorkspaceEvent>],
    dropped_events: &AtomicU64,
) {
    let mut tracker = ActivationTracker::default();
    let startup_events = runtime.poll(Duration::ZERO);
    let has_queued_activation = startup_events
        .iter()
        .any(|event| matches!(event, NativeWorkspaceEvent::Activated(_)));
    for event in startup_events {
        process_workspace_event(
            runtime,
            event,
            &mut tracker,
            sender,
            subscribers,
            dropped_events,
        );
    }
    if !has_queued_activation && let Some(app) = runtime.frontmost_application() {
        process_workspace_event(
            runtime,
            NativeWorkspaceEvent::Activated(app),
            &mut tracker,
            sender,
            subscribers,
            dropped_events,
        );
    }

    while !stop.load(Ordering::Acquire) {
        let events = runtime.poll(RUN_LOOP_SLICE);
        if stop.load(Ordering::Acquire) {
            break;
        }
        for event in events {
            process_workspace_event(
                runtime,
                event,
                &mut tracker,
                sender,
                subscribers,
                dropped_events,
            );
        }
        dropped_events.fetch_add(runtime.take_dropped_events(), Ordering::Relaxed);
    }
}

#[derive(Default)]
struct ActivationTracker {
    current_pid: Option<i64>,
    current_bundle_id: Option<String>,
}

impl ActivationTracker {
    fn event(&mut self, app: &ApplicationInfo, window: Option<Window>) -> Option<RawEvent> {
        if self.current_pid == Some(app.pid) && self.current_bundle_id == app.bundle_id {
            return None;
        }
        let event = RawEvent {
            observed_at: None,
            source: "macos.workspace".to_owned(),
            event_type: "app.activate".to_owned(),
            app: app.raw_app(),
            window,
            element: None,
            data: EventData::AppActivate(EmptyData {}),
            capture_context: Default::default(),
        };
        self.current_pid = Some(app.pid);
        self.current_bundle_id.clone_from(&app.bundle_id);
        Some(event)
    }

    fn terminated(&mut self, app: &ApplicationInfo) {
        if self.current_pid == Some(app.pid) {
            self.current_pid = None;
            self.current_bundle_id = None;
        }
    }
}

fn process_workspace_event(
    runtime: &impl WorkspaceApi,
    event: NativeWorkspaceEvent,
    tracker: &mut ActivationTracker,
    sender: &SyncSender<RawEvent>,
    subscribers: &[SyncSender<WorkspaceEvent>],
    dropped_events: &AtomicU64,
) {
    let (notification, raw_event) = match event {
        NativeWorkspaceEvent::Activated(app) => {
            let app = ApplicationInfo::from(app);
            let raw_event = tracker.event(&app, runtime.front_window(app.pid));
            if raw_event.is_none() {
                return;
            }
            (WorkspaceEvent::Activated(app), raw_event)
        }
        NativeWorkspaceEvent::Launched(app) => {
            let app = ApplicationInfo::from(app);
            let raw_event = lifecycle_raw_event("app.launch", &app, EventData::AppLaunch);
            (WorkspaceEvent::Launched(app), Some(raw_event))
        }
        NativeWorkspaceEvent::Terminated(app) => {
            let app = ApplicationInfo::from(app);
            tracker.terminated(&app);
            let raw_event = lifecycle_raw_event("app.terminate", &app, EventData::AppTerminate);
            (WorkspaceEvent::Terminated(app), Some(raw_event))
        }
        NativeWorkspaceEvent::DidWake => (WorkspaceEvent::DidWake, None),
    };

    for subscriber in subscribers {
        if subscriber.try_send(notification.clone()).is_err() {
            dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }
    if raw_event.is_some_and(|event| sender.try_send(event).is_err()) {
        dropped_events.fetch_add(1, Ordering::Relaxed);
    }
}

fn lifecycle_raw_event(
    event_type: &str,
    app: &ApplicationInfo,
    data: fn(EmptyData) -> EventData,
) -> RawEvent {
    RawEvent {
        observed_at: None,
        source: "macos.workspace".to_owned(),
        event_type: event_type.to_owned(),
        app: app.raw_app(),
        window: None,
        element: None,
        data: data(EmptyData::default()),
        capture_context: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, AtomicU64},
            mpsc::sync_channel,
        },
        time::Duration,
    };

    use zanei_core::schema::Window;

    use super::{
        ApplicationActivationPolicy, ApplicationInfo, EmptyData, EventData, NativeApplication,
        NativeApplicationActivationPolicy, NativeWorkspaceEvent, WorkspaceApi, lifecycle_raw_event,
        run_workspace,
    };

    #[test]
    fn lifecycle_events_use_the_workspace_source() {
        let app = ApplicationInfo {
            name: "Example".to_owned(),
            bundle_id: Some("dev.example.App".to_owned()),
            pid: 42,
            activation_policy: ApplicationActivationPolicy::Regular,
        };
        let event = lifecycle_raw_event("app.launch", &app, EventData::AppLaunch);

        assert_eq!(event.source, "macos.workspace");
        assert_eq!(event.event_type, "app.launch");
        assert_eq!(event.app.pid, Some(42));
        assert!(matches!(event.data, EventData::AppLaunch(_)));
    }

    #[test]
    fn startup_emits_the_initial_activation_with_no_previous_app() {
        struct InitialWorkspace;

        impl WorkspaceApi for InitialWorkspace {
            fn poll(&mut self, _timeout: Duration) -> Vec<NativeWorkspaceEvent> {
                Vec::new()
            }

            fn frontmost_application(&self) -> Option<NativeApplication> {
                Some(NativeApplication {
                    name: "Example".to_owned(),
                    bundle_id: Some("dev.example.App".to_owned()),
                    pid: 42,
                    activation_policy: NativeApplicationActivationPolicy::Regular,
                })
            }

            fn front_window(&self, _pid: i64) -> Option<Window> {
                Some(Window {
                    title: Some("Initial".to_owned()),
                    id: Some(7),
                })
            }

            fn take_dropped_events(&self) -> u64 {
                0
            }
        }

        let (sender, receiver) = sync_channel(1);
        let stopped = AtomicBool::new(true);
        run_workspace(
            &mut InitialWorkspace,
            &stopped,
            &sender,
            &[],
            &AtomicU64::new(0),
        );

        let event = receiver.try_recv().expect("initial activation");
        assert_eq!(event.event_type, "app.activate");
        assert_eq!(event.window.and_then(|window| window.id), Some(7));
        let EventData::AppActivate(data) = event.data else {
            panic!("expected app.activate payload");
        };
        assert_eq!(data, EmptyData {});
    }

    #[test]
    fn queued_activation_wins_over_snapshot_and_duplicates_are_suppressed() {
        struct QueuedWorkspace {
            events: Vec<NativeWorkspaceEvent>,
        }

        impl WorkspaceApi for QueuedWorkspace {
            fn poll(&mut self, _timeout: Duration) -> Vec<NativeWorkspaceEvent> {
                std::mem::take(&mut self.events)
            }

            fn frontmost_application(&self) -> Option<NativeApplication> {
                panic!("snapshot must not run when activation is already queued")
            }

            fn front_window(&self, _pid: i64) -> Option<Window> {
                Some(Window {
                    title: Some("Queued".to_owned()),
                    id: Some(8),
                })
            }

            fn take_dropped_events(&self) -> u64 {
                0
            }
        }

        let app = NativeApplication {
            name: "Queued".to_owned(),
            bundle_id: Some("dev.example.Queued".to_owned()),
            pid: 43,
            activation_policy: NativeApplicationActivationPolicy::Regular,
        };
        let mut runtime = QueuedWorkspace {
            events: vec![
                NativeWorkspaceEvent::Activated(app.clone()),
                NativeWorkspaceEvent::Activated(app),
            ],
        };
        let (sender, receiver) = sync_channel(2);
        run_workspace(
            &mut runtime,
            &AtomicBool::new(true),
            &sender,
            &[],
            &AtomicU64::new(0),
        );

        assert_eq!(
            receiver.try_recv().expect("queued activation").app.pid,
            Some(43)
        );
        assert!(receiver.try_recv().is_err());
    }
}
