//! Per-application Accessibility observers and AX event construction.

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
use zanei_core::config::FilterConfig;
use zanei_core::schema::ClickButton;

use crate::{
    InputAuthorizations, SecureInputProbe,
    content_snapshot::SnapshotTriggerPublisher,
    ffi::ax::{ManualAccessibilityPolicy, NativeAxEvent},
    focus_context::FocusContext,
    focused_field::{FocusedField, FocusedFieldPublisher, field_class},
    text_capture::TextContentPolicy,
    workspace::{ApplicationActivationPolicy, ApplicationInfo, WorkspaceEvent},
};

use self::{event::AxEventBuilder, health::ObserverHealth};

pub use crate::ffi::ax::NativeWindow;

#[cfg(test)]
use crate::ffi::ax::NativeHitTest;

mod event;
mod health;
mod output;
mod runtime;
mod trigger;

use output::AxOutput;
use runtime::{AxApi, SystemAxApi};
use trigger::publish_focus_transition;

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
    pub observed_at: time::OffsetDateTime,
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
    focus_context: FocusContext,
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
    pub focus_context: FocusContext,
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
            focus_context: options.focus_context,
            snapshot_trigger_publisher: options.snapshot_trigger_publisher,
            worker: None,
            dropped_events: Arc::new(AtomicU64::new(0)),
            degraded_operations: Arc::new(AtomicU64::new(0)),
            current_degraded_observers: Arc::new(AtomicU64::new(0)),
        }
    }

    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed).saturating_add(
            self.snapshot_trigger_publisher
                .as_ref()
                .map_or(0, SnapshotTriggerPublisher::dropped),
        )
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
        let focus_context = self.focus_context.clone();
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
                    focus_context,
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
    focus_context: FocusContext,
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
        focus_context,
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
    focus_context: FocusContext,
    snapshot_trigger_publisher: Option<&SnapshotTriggerPublisher>,
    dropped_events: &AtomicU64,
    degraded_operations: &AtomicU64,
    current_degraded_observers: Arc<AtomicU64>,
) {
    let mut output = AxOutput::new(sender, dropped_events, text_policy.clone());
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
        output.send_all(pending);
    }
    if let Some(app) = api.frontmost_application() {
        let window = i32::try_from(app.pid)
            .ok()
            .and_then(|pid| api.focused_window(pid).ok().flatten());
        publish_focus_transition(
            snapshot_trigger_publisher,
            focus_context.activate(app, window),
        );
    }
    let mut manual_accessibility_generation = manual_accessibility_policy.generation();

    while !stop.load(Ordering::Acquire) {
        output.release_due();
        let current_generation = manual_accessibility_policy.generation();
        if current_generation != manual_accessibility_generation {
            api.reconcile_manual_accessibility(&manual_accessibility_policy);
            manual_accessibility_generation = current_generation;
        }
        for event in lifecycle_receiver.try_iter() {
            if let WorkspaceEvent::Activated(app) = &event {
                observer_health.mark_used(app.pid);
            }
            match event {
                WorkspaceEvent::Activated(app) => {
                    let pending = attach_app(
                        api,
                        &mut builder,
                        app.clone(),
                        focused_field_publisher,
                        &manual_accessibility_policy,
                        degraded_operations,
                        &mut observer_health,
                    );
                    output.send_all(pending);
                    let window = i32::try_from(app.pid)
                        .ok()
                        .and_then(|pid| api.focused_window(pid).ok().flatten());
                    publish_focus_transition(
                        snapshot_trigger_publisher,
                        focus_context.activate(app, window),
                    );
                }
                WorkspaceEvent::Launched(app) => {
                    let pending = attach_app(
                        api,
                        &mut builder,
                        app,
                        focused_field_publisher,
                        &manual_accessibility_policy,
                        degraded_operations,
                        &mut observer_health,
                    );
                    output.send_all(pending);
                }
                WorkspaceEvent::Terminated(app) => {
                    publish_focus_transition(
                        snapshot_trigger_publisher,
                        focus_context.terminate(app.pid),
                    );
                    observer_health.remove(app.pid);
                    if let Ok(pid) = i32::try_from(app.pid) {
                        let pending = api.detach(pid);
                        send_observations(
                            &mut output,
                            &mut builder,
                            pending,
                            focused_field_publisher,
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
                output.send(event);
            }
        }
        for observation in api.poll(AX_RUN_LOOP_SLICE) {
            let transition = match &observation {
                NativeAxEvent::WindowFocused { pid, window, .. }
                | NativeAxEvent::WindowTitleChanged { pid, window, .. } => {
                    focus_context.observe_window(*pid, window.clone())
                }
                NativeAxEvent::UiFocused { .. } | NativeAxEvent::UiValueChanged { .. } => None,
            };
            publish_focus_transition(snapshot_trigger_publisher, transition);
            publish_focus_observation(focused_field_publisher, &observation);
            if let Some(event) = builder.event(observation) {
                output.send(event);
            }
        }
        let native_drops = api.take_dropped_events();
        dropped_events.fetch_add(native_drops, Ordering::Relaxed);
        degraded_operations.fetch_add(api.take_degraded_operations(), Ordering::Relaxed);
    }
    send_observations(
        &mut output,
        &mut builder,
        api.flush_pending(),
        focused_field_publisher,
    );
    output.flush();
    observer_health.clear();
}

fn send_observations(
    output: &mut AxOutput<'_>,
    builder: &mut AxEventBuilder,
    observations: Vec<NativeAxEvent>,
    focused_field_publisher: Option<&FocusedFieldPublisher>,
) {
    for observation in observations {
        publish_focus_observation(focused_field_publisher, &observation);
        if let Some(event) = builder.event(observation) {
            output.send(event);
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

#[cfg(test)]
mod tests;
