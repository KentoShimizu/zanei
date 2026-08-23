//! Per-application Accessibility observers and AX event construction.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use zanei_collector::{Collector, CollectorError, Permission, RawEvent};
use zanei_core::config::FilterConfig;
use zanei_core::schema::{App, ClickButton};

use crate::{
    InputAuthorizations, SecureInputProbe,
    content_snapshot::SnapshotTriggerPublisher,
    ffi::{
        ax::{ManualAccessibilityPolicy, NativeAx, NativeAxError, NativeAxEvent, NativeHitTest},
        workspace::running_applications,
    },
    focused_field::{FocusedField, FocusedFieldPublisher, field_class},
    text_capture::TextContentPolicy,
    workspace::{ApplicationActivationPolicy, ApplicationInfo, WorkspaceEvent},
};

use self::{event::AxEventBuilder, health::ObserverHealth};

pub use crate::ffi::ax::NativeWindow;

mod event;
mod health;
mod trigger;

use trigger::publish_snapshot_trigger;

const CLICK_CHANNEL_CAPACITY: usize = 1_024;
const MAX_CLICK_OBSERVATIONS_PER_TICK: usize = 1;
const AX_RUN_LOOP_SLICE: Duration = Duration::from_millis(50);
const REQUIRED_PERMISSIONS: [Permission; 1] = [Permission::Accessibility];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClickObservation {
    pub pid: i32,
    pub x: f64,
    pub y: f64,
    pub button: ClickButton,
    pub click_count: u64,
}

#[must_use]
pub fn click_channel() -> (SyncSender<ClickObservation>, Receiver<ClickObservation>) {
    sync_channel(CLICK_CHANNEL_CAPACITY)
}

pub struct AxCollector {
    lifecycle_receiver: Option<Receiver<WorkspaceEvent>>,
    click_receiver: Option<Receiver<ClickObservation>>,
    focused_field_publisher: Option<FocusedFieldPublisher>,
    authorizations: Option<InputAuthorizations>,
    secure_input_probe: Option<SecureInputProbe>,
    capture_text_content: bool,
    text_policy: TextContentPolicy,
    manual_accessibility_policy: ManualAccessibilityPolicy,
    snapshot_trigger_publisher: Option<SnapshotTriggerPublisher>,
    worker: Option<Worker>,
    dropped_events: Arc<AtomicU64>,
    degraded_operations: Arc<AtomicU64>,
    current_degraded_observers: Arc<AtomicU64>,
}

pub struct AxCollectorOptions {
    pub secure_input_probe: Option<SecureInputProbe>,
    pub capture_text_content: bool,
    pub capture_content_snapshot: bool,
    pub filter: FilterConfig,
    pub text_policy: TextContentPolicy,
    pub snapshot_trigger_publisher: Option<SnapshotTriggerPublisher>,
}

impl AxCollector {
    #[must_use]
    pub fn new(
        lifecycle_receiver: Receiver<WorkspaceEvent>,
        click_receiver: Receiver<ClickObservation>,
        focused_field_publisher: Option<FocusedFieldPublisher>,
        authorizations: InputAuthorizations,
        options: AxCollectorOptions,
    ) -> Self {
        Self {
            lifecycle_receiver: Some(lifecycle_receiver),
            click_receiver: Some(click_receiver),
            focused_field_publisher,
            authorizations: Some(authorizations),
            secure_input_probe: options.secure_input_probe,
            capture_text_content: options.capture_text_content,
            text_policy: options.text_policy,
            manual_accessibility_policy: ManualAccessibilityPolicy::new(
                options.capture_text_content,
                options.capture_content_snapshot,
                options.filter,
            ),
            snapshot_trigger_publisher: options.snapshot_trigger_publisher,
            worker: None,
            dropped_events: Arc::new(AtomicU64::new(0)),
            degraded_operations: Arc::new(AtomicU64::new(0)),
            current_degraded_observers: Arc::new(AtomicU64::new(0)),
        }
    }

    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn degraded_operations(&self) -> u64 {
        self.degraded_operations.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn degraded_observers(&self) -> u64 {
        self.current_degraded_observers.load(Ordering::Relaxed)
    }

    pub fn replace_filter(&self, filter: FilterConfig) {
        self.text_policy.replace_filter(filter.clone());
        self.manual_accessibility_policy.replace_filter(filter);
    }
}

impl Collector for AxCollector {
    fn name(&self) -> &str {
        "ax"
    }

    fn required_permissions(&self) -> &[Permission] {
        &REQUIRED_PERMISSIONS
    }

    fn start(&mut self, sender: SyncSender<RawEvent>) -> Result<(), CollectorError> {
        if self.worker.is_some() {
            return Err(CollectorError::AlreadyRunning {
                collector: self.name().to_owned(),
            });
        }
        let lifecycle_receiver =
            self.lifecycle_receiver
                .take()
                .ok_or_else(|| CollectorError::Start {
                    collector: self.name().to_owned(),
                    message: "AX lifecycle channel is unavailable".to_owned(),
                })?;
        let click_receiver = self
            .click_receiver
            .take()
            .ok_or_else(|| CollectorError::Start {
                collector: self.name().to_owned(),
                message: "AX click channel is unavailable".to_owned(),
            })?;
        let authorizations = self
            .authorizations
            .take()
            .ok_or_else(|| CollectorError::Start {
                collector: self.name().to_owned(),
                message: "input authorization channel is unavailable".to_owned(),
            })?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let dropped_events = Arc::clone(&self.dropped_events);
        let degraded_operations = Arc::clone(&self.degraded_operations);
        let current_degraded_observers = Arc::clone(&self.current_degraded_observers);
        let focused_field_publisher = self.focused_field_publisher.clone();
        let secure_input_probe = self.secure_input_probe.clone();
        let capture_text_content = self.capture_text_content;
        let text_policy = self.text_policy.clone();
        let manual_accessibility_policy = self.manual_accessibility_policy.clone();
        let snapshot_trigger_publisher = self.snapshot_trigger_publisher.clone();
        let handle = thread::Builder::new()
            .name("zanei-ax".to_owned())
            .spawn(move || {
                let authorizations = run_ax(
                    &thread_stop,
                    &sender,
                    &lifecycle_receiver,
                    &click_receiver,
                    focused_field_publisher.as_ref(),
                    authorizations,
                    secure_input_probe,
                    capture_text_content,
                    text_policy,
                    manual_accessibility_policy,
                    snapshot_trigger_publisher,
                    &dropped_events,
                    &degraded_operations,
                    current_degraded_observers,
                );
                (lifecycle_receiver, click_receiver, authorizations)
            })
            .map_err(|error| CollectorError::Start {
                collector: self.name().to_owned(),
                message: error.to_string(),
            })?;
        self.worker = Some(Worker { stop, handle });
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop.store(true, Ordering::Release);
            if let Ok((lifecycle_receiver, click_receiver, authorizations)) = worker.handle.join() {
                self.lifecycle_receiver = Some(lifecycle_receiver);
                self.click_receiver = Some(click_receiver);
                self.authorizations = Some(authorizations);
            }
        }
        self.current_degraded_observers.store(0, Ordering::Relaxed);
    }
}

impl Drop for AxCollector {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Worker {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<(
        Receiver<WorkspaceEvent>,
        Receiver<ClickObservation>,
        InputAuthorizations,
    )>,
}

#[allow(clippy::too_many_arguments)]
fn run_ax(
    stop: &AtomicBool,
    sender: &SyncSender<RawEvent>,
    lifecycle_receiver: &Receiver<WorkspaceEvent>,
    click_receiver: &Receiver<ClickObservation>,
    focused_field_publisher: Option<&FocusedFieldPublisher>,
    authorizations: InputAuthorizations,
    secure_input_probe: Option<SecureInputProbe>,
    capture_text_content: bool,
    text_policy: TextContentPolicy,
    manual_accessibility_policy: ManualAccessibilityPolicy,
    snapshot_trigger_publisher: Option<SnapshotTriggerPublisher>,
    dropped_events: &AtomicU64,
    degraded_operations: &AtomicU64,
    current_degraded_observers: Arc<AtomicU64>,
) -> InputAuthorizations {
    let mut api = SystemAxApi::new(
        capture_text_content,
        authorizations,
        secure_input_probe,
        text_policy.clone(),
    );
    run_ax_loop(
        &mut api,
        stop,
        sender,
        lifecycle_receiver,
        click_receiver,
        focused_field_publisher,
        text_policy,
        manual_accessibility_policy,
        snapshot_trigger_publisher.as_ref(),
        dropped_events,
        degraded_operations,
        current_degraded_observers,
    );
    api.into_authorizations()
}

#[allow(clippy::too_many_arguments)]
fn run_ax_loop(
    api: &mut impl AxApi,
    stop: &AtomicBool,
    sender: &SyncSender<RawEvent>,
    lifecycle_receiver: &Receiver<WorkspaceEvent>,
    click_receiver: &Receiver<ClickObservation>,
    focused_field_publisher: Option<&FocusedFieldPublisher>,
    text_policy: TextContentPolicy,
    manual_accessibility_policy: ManualAccessibilityPolicy,
    snapshot_trigger_publisher: Option<&SnapshotTriggerPublisher>,
    dropped_events: &AtomicU64,
    degraded_operations: &AtomicU64,
    current_degraded_observers: Arc<AtomicU64>,
) {
    let mut builder = AxEventBuilder::new(text_policy);
    let mut observer_health = ObserverHealth::new(current_degraded_observers);
    for app in api.running_applications() {
        let pending = attach_app(
            api,
            &mut builder,
            app,
            focused_field_publisher,
            &manual_accessibility_policy,
            degraded_operations,
            &mut observer_health,
        );
        send_events(sender, pending, dropped_events);
    }

    while !stop.load(Ordering::Acquire) {
        for event in lifecycle_receiver.try_iter() {
            if let WorkspaceEvent::Activated(app) = &event {
                observer_health.mark_used(app.pid);
            }
            match event {
                WorkspaceEvent::Activated(app) | WorkspaceEvent::Launched(app) => {
                    let pending = attach_app(
                        api,
                        &mut builder,
                        app,
                        focused_field_publisher,
                        &manual_accessibility_policy,
                        degraded_operations,
                        &mut observer_health,
                    );
                    send_events(sender, pending, dropped_events);
                }
                WorkspaceEvent::Terminated(app) => {
                    observer_health.remove(app.pid);
                    if let Ok(pid) = i32::try_from(app.pid) {
                        let pending = api.detach(pid);
                        send_observations(
                            sender,
                            &mut builder,
                            pending,
                            focused_field_publisher,
                            snapshot_trigger_publisher,
                            dropped_events,
                        );
                        builder.remove_app(pid);
                        remove_focused_field(focused_field_publisher, pid);
                    }
                }
                WorkspaceEvent::DidWake => {}
            }
        }
        for click in click_receiver
            .try_iter()
            .take(MAX_CLICK_OBSERVATIONS_PER_TICK)
        {
            if let Some(hit) = api.hit_test(click)
                && let Some(event) = builder.click_event(hit, click)
            {
                send_output(sender, event, dropped_events);
            }
        }
        for observation in api.poll(AX_RUN_LOOP_SLICE) {
            publish_snapshot_trigger(snapshot_trigger_publisher, &builder, &observation);
            publish_focus_observation(focused_field_publisher, &observation);
            if let Some(event) = builder.event(observation) {
                send_output(sender, event, dropped_events);
            }
        }
        let native_drops = api.take_dropped_events();
        dropped_events.fetch_add(native_drops, Ordering::Relaxed);
        degraded_operations.fetch_add(api.take_degraded_operations(), Ordering::Relaxed);
    }
    send_observations(
        sender,
        &mut builder,
        api.flush_pending(),
        focused_field_publisher,
        snapshot_trigger_publisher,
        dropped_events,
    );
    observer_health.clear();
}

fn send_observations(
    sender: &SyncSender<RawEvent>,
    builder: &mut AxEventBuilder,
    observations: Vec<NativeAxEvent>,
    focused_field_publisher: Option<&FocusedFieldPublisher>,
    snapshot_trigger_publisher: Option<&SnapshotTriggerPublisher>,
    dropped_events: &AtomicU64,
) {
    for observation in observations {
        publish_snapshot_trigger(snapshot_trigger_publisher, builder, &observation);
        publish_focus_observation(focused_field_publisher, &observation);
        if let Some(event) = builder.event(observation) {
            send_output(sender, event, dropped_events);
        }
    }
}

fn send_events(sender: &SyncSender<RawEvent>, events: Vec<RawEvent>, dropped_events: &AtomicU64) {
    for event in events {
        send_output(sender, event, dropped_events);
    }
}

fn send_output(sender: &SyncSender<RawEvent>, event: RawEvent, dropped_events: &AtomicU64) {
    match sender.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(event)) => {
            crate::trace::trace!(
                "component=ax phase=output action=drop event={} reason=output_full",
                event.event_type
            );
            dropped_events.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(event)) => {
            crate::trace::trace!(
                "component=ax phase=output action=drop event={} reason=output_disconnected",
                event.event_type
            );
            dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn attach_app(
    api: &mut impl AxApi,
    builder: &mut AxEventBuilder,
    app: ApplicationInfo,
    focused_field_publisher: Option<&FocusedFieldPublisher>,
    manual_accessibility_policy: &ManualAccessibilityPolicy,
    degraded_operations: &AtomicU64,
    observer_health: &mut ObserverHealth,
) -> Vec<RawEvent> {
    if app.activation_policy == ApplicationActivationPolicy::Prohibited {
        return Vec::new();
    }
    let Ok(pid) = i32::try_from(app.pid) else {
        observer_health.mark_unavailable(app.pid);
        degraded_operations.fetch_add(1, Ordering::Relaxed);
        return Vec::new();
    };
    builder.add_app(app.clone());
    let manual_accessibility = manual_accessibility_policy.allows(&app.raw_app());
    match api.attach(pid, app.raw_app(), manual_accessibility) {
        Ok(observations) => {
            observer_health.mark_available(app.pid);
            observations
                .into_iter()
                .filter_map(|observation| {
                    publish_focus_observation(focused_field_publisher, &observation);
                    matches!(&observation, NativeAxEvent::UiValueChanged { .. })
                        .then(|| builder.event(observation))
                        .flatten()
                })
                .collect()
        }
        Err(_) => {
            observer_health.mark_unavailable(app.pid);
            update_focused_field(focused_field_publisher, pid, None);
            degraded_operations.fetch_add(1, Ordering::Relaxed);
            Vec::new()
        }
    }
}

fn publish_focus_observation(
    publisher: Option<&FocusedFieldPublisher>,
    observation: &NativeAxEvent,
) {
    let NativeAxEvent::UiFocused {
        pid,
        generation,
        element,
        ..
    } = observation
    else {
        return;
    };
    let focused_field = element.as_ref().map(|element| FocusedField {
        generation: *generation,
        class: field_class(element.role.as_deref(), element.subrole.as_deref()),
    });
    update_focused_field(publisher, *pid, focused_field);
}

fn update_focused_field(
    publisher: Option<&FocusedFieldPublisher>,
    pid: i32,
    focused_field: Option<FocusedField>,
) {
    if let Some(publisher) = publisher {
        publisher.update(pid, focused_field);
    }
}

fn remove_focused_field(publisher: Option<&FocusedFieldPublisher>, pid: i32) {
    if let Some(publisher) = publisher {
        publisher.remove(pid);
    }
}

trait AxApi {
    type AttachError;

    fn running_applications(&self) -> Vec<ApplicationInfo>;
    fn attach(
        &mut self,
        pid: i32,
        app: App,
        manual_accessibility: bool,
    ) -> Result<Vec<NativeAxEvent>, Self::AttachError>;
    fn detach(&mut self, pid: i32) -> Vec<NativeAxEvent>;
    fn poll(&mut self, timeout: Duration) -> Vec<NativeAxEvent>;
    fn flush_pending(&mut self) -> Vec<NativeAxEvent>;
    fn hit_test(&self, click: ClickObservation) -> Option<NativeHitTest>;
    fn take_dropped_events(&self) -> u64;
    fn take_degraded_operations(&self) -> u64;
}

struct SystemAxApi {
    native: NativeAx,
}

impl SystemAxApi {
    fn new(
        capture_text_content: bool,
        authorizations: InputAuthorizations,
        secure_input_probe: Option<SecureInputProbe>,
        text_policy: TextContentPolicy,
    ) -> Self {
        Self {
            native: NativeAx::new(
                capture_text_content,
                authorizations,
                secure_input_probe,
                text_policy,
            ),
        }
    }

    fn into_authorizations(self) -> InputAuthorizations {
        self.native.into_authorizations()
    }
}

impl AxApi for SystemAxApi {
    type AttachError = NativeAxError;

    fn running_applications(&self) -> Vec<ApplicationInfo> {
        running_applications()
            .into_iter()
            .map(ApplicationInfo::from)
            .collect()
    }

    fn attach(
        &mut self,
        pid: i32,
        app: App,
        manual_accessibility: bool,
    ) -> Result<Vec<NativeAxEvent>, NativeAxError> {
        self.native.attach(pid, app, manual_accessibility)
    }

    fn detach(&mut self, pid: i32) -> Vec<NativeAxEvent> {
        self.native.detach(pid)
    }

    fn poll(&mut self, timeout: Duration) -> Vec<NativeAxEvent> {
        self.native.poll(timeout)
    }

    fn flush_pending(&mut self) -> Vec<NativeAxEvent> {
        self.native.flush_pending()
    }

    fn hit_test(&self, click: ClickObservation) -> Option<NativeHitTest> {
        self.native.hit_test(click.pid, click.x, click.y)
    }

    fn take_dropped_events(&self) -> u64 {
        self.native.take_dropped_events()
    }

    fn take_degraded_operations(&self) -> u64 {
        self.native.take_degraded_operations()
    }
}

#[cfg(test)]
mod tests;
