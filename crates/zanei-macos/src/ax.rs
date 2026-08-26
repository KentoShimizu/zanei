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

use zanei_collector::{Capability, Collector, CollectorError, RawEvent};
use zanei_core::config::FilterConfig;
use zanei_core::schema::ClickButton;

use crate::{
    CapturePolicy, InputAuthorizations, SecureInputProbe,
    chrome::ChromeObserver,
    content_snapshot::SnapshotTriggerPublisher,
    ffi::ax::{ManualAccessibilityPolicy, NativeAxEvent, NativeAxObservation},
    focus_context::FocusContext,
    focused_field::FocusedField,
    workspace::{ApplicationActivationPolicy, ApplicationInfo, WorkspaceEvent},
};

use self::{
    event::{AxEvent, AxEventBuilder},
    health::{AxFailurePublisher, AxRecoverySite, ObserverHealth},
};

pub use crate::ffi::ax::NativeWindow;
pub use health::{AxFailure, AxFailureKind, AxFailurePhase, AxFailureState};

#[cfg(test)]
use crate::ffi::ax::NativeHitTest;

mod event;
pub(crate) mod health;
mod output;
mod runtime;
mod trigger;

use output::AxOutput;
use runtime::{AxApi, SystemAxApi};
use trigger::publish_focus_transition;

const CLICK_CHANNEL_CAPACITY: usize = 1_024;
const MAX_CLICK_OBSERVATIONS_PER_TICK: usize = 1;
const AX_RUN_LOOP_SLICE: Duration = Duration::from_millis(50);
const REQUIRED_CAPABILITIES: [Capability; 1] = [Capability::ReadAccessibilityTree];

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
    authorizations: Option<InputAuthorizations>,
    secure_input_probe: Option<SecureInputProbe>,
    capture_text_content: bool,
    capture_policy: CapturePolicy,
    chrome_observer: Option<ChromeObserver>,
    manual_accessibility_policy: ManualAccessibilityPolicy,
    focus_context: FocusContext,
    snapshot_trigger_publisher: Option<SnapshotTriggerPublisher>,
    worker: Option<Worker>,
    dropped_events: Arc<AtomicU64>,
    degraded_operations: Arc<AtomicU64>,
    current_degraded_observers: Arc<AtomicU64>,
    failure_publisher: AxFailurePublisher,
}

pub struct AxCollectorOptions {
    pub secure_input_probe: Option<SecureInputProbe>,
    pub capture_text_content: bool,
    pub capture_content_snapshot: bool,
    pub filter: FilterConfig,
    pub capture_policy: CapturePolicy,
    pub chrome_observer: Option<ChromeObserver>,
    pub snapshot_trigger_publisher: Option<SnapshotTriggerPublisher>,
    pub focus_context: FocusContext,
}

impl AxCollector {
    #[must_use]
    pub fn new(
        lifecycle_receiver: Receiver<WorkspaceEvent>,
        click_receiver: Receiver<ClickObservation>,
        authorizations: InputAuthorizations,
        options: AxCollectorOptions,
    ) -> Self {
        Self {
            lifecycle_receiver: Some(lifecycle_receiver),
            click_receiver: Some(click_receiver),
            authorizations: Some(authorizations),
            secure_input_probe: options.secure_input_probe,
            capture_text_content: options.capture_text_content,
            capture_policy: options.capture_policy,
            chrome_observer: options.chrome_observer,
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
            failure_publisher: AxFailurePublisher::default(),
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

    #[must_use]
    pub fn failure_state(&self) -> AxFailureState {
        self.failure_publisher.state()
    }

    pub fn replace_filter(&self, filter: FilterConfig) {
        self.manual_accessibility_policy.replace_filter(filter);
    }
}

impl Collector for AxCollector {
    fn name(&self) -> &str {
        "ax"
    }

    fn required_capabilities(&self) -> &[Capability] {
        &REQUIRED_CAPABILITIES
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
        let failure_publisher = self.failure_publisher.clone();
        let secure_input_probe = self.secure_input_probe.clone();
        let capture_text_content = self.capture_text_content;
        let capture_policy = self.capture_policy.clone();
        let chrome_observer = self.chrome_observer.clone();
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
                    authorizations,
                    secure_input_probe,
                    capture_text_content,
                    capture_policy,
                    chrome_observer,
                    manual_accessibility_policy,
                    focus_context,
                    snapshot_trigger_publisher,
                    &dropped_events,
                    &degraded_operations,
                    current_degraded_observers,
                    failure_publisher,
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
        self.failure_publisher.clear();
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
    authorizations: InputAuthorizations,
    secure_input_probe: Option<SecureInputProbe>,
    capture_text_content: bool,
    capture_policy: CapturePolicy,
    chrome_observer: Option<ChromeObserver>,
    manual_accessibility_policy: ManualAccessibilityPolicy,
    focus_context: FocusContext,
    snapshot_trigger_publisher: Option<SnapshotTriggerPublisher>,
    dropped_events: &AtomicU64,
    degraded_operations: &AtomicU64,
    current_degraded_observers: Arc<AtomicU64>,
    failure_publisher: AxFailurePublisher,
) -> InputAuthorizations {
    let mut api = SystemAxApi::new(
        capture_text_content,
        authorizations,
        secure_input_probe,
        capture_policy.clone(),
        chrome_observer.is_some(),
        failure_publisher.clone(),
    );
    run_ax_loop(
        &mut api,
        stop,
        sender,
        lifecycle_receiver,
        click_receiver,
        capture_policy,
        chrome_observer,
        manual_accessibility_policy,
        focus_context,
        snapshot_trigger_publisher.as_ref(),
        dropped_events,
        degraded_operations,
        current_degraded_observers,
        &failure_publisher,
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
    capture_policy: CapturePolicy,
    chrome_observer: Option<ChromeObserver>,
    manual_accessibility_policy: ManualAccessibilityPolicy,
    focus_context: FocusContext,
    snapshot_trigger_publisher: Option<&SnapshotTriggerPublisher>,
    dropped_events: &AtomicU64,
    degraded_operations: &AtomicU64,
    current_degraded_observers: Arc<AtomicU64>,
    failure_publisher: &AxFailurePublisher,
) {
    let mut output = AxOutput::new(
        sender,
        dropped_events,
        capture_policy.clone(),
        chrome_observer.clone().unwrap_or_default(),
    );
    let mut builder = AxEventBuilder::new(capture_policy);
    let mut observer_health = ObserverHealth::new(current_degraded_observers);
    let initial_attached: Vec<_> = api
        .running_applications()
        .into_iter()
        .map(|app| {
            attach_app(
                api,
                &mut builder,
                app,
                &manual_accessibility_policy,
                degraded_operations,
                failure_publisher,
                &mut observer_health,
            )
        })
        .collect();
    if let Some(app) = api.frontmost_application() {
        let window = focused_window(api, &app);
        publish_focus_transition(
            snapshot_trigger_publisher,
            focus_context.activate(app, window),
        );
    }
    for attached in &initial_attached {
        publish_attached_focus(&focus_context, attached);
    }
    for attached in initial_attached {
        output.send_all(attached.output);
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
                    let attached = attach_app(
                        api,
                        &mut builder,
                        app.clone(),
                        &manual_accessibility_policy,
                        degraded_operations,
                        failure_publisher,
                        &mut observer_health,
                    );
                    let window = focused_window(api, &app);
                    publish_focus_transition(
                        snapshot_trigger_publisher,
                        focus_context.activate(app, window),
                    );
                    publish_attached_focus(&focus_context, &attached);
                    output.send_all(attached.output);
                }
                WorkspaceEvent::Launched(app) => {
                    let attached = attach_app(
                        api,
                        &mut builder,
                        app,
                        &manual_accessibility_policy,
                        degraded_operations,
                        failure_publisher,
                        &mut observer_health,
                    );
                    publish_attached_focus(&focus_context, &attached);
                    output.send_all(attached.output);
                }
                WorkspaceEvent::Terminated(app) => {
                    publish_focus_transition(
                        snapshot_trigger_publisher,
                        focus_context.terminate(app.pid),
                    );
                    observer_health.remove(app.pid);
                    failure_publisher.remove_pid(app.pid);
                    if let Ok(pid) = i32::try_from(app.pid) {
                        let pending = api.detach(pid);
                        send_observations(&mut output, &mut builder, pending, &focus_context);
                        builder.remove_app(pid);
                    }
                }
                WorkspaceEvent::DidWake => {
                    let transition = api.frontmost_application().map_or_else(
                        || focus_context.resync_without_focus(),
                        |app| {
                            let window = focused_window(api, &app);
                            focus_context.resync(app, window)
                        },
                    );
                    publish_focus_transition(snapshot_trigger_publisher, Some(transition));
                }
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
            match observation {
                NativeAxObservation::FocusedFieldObserved { pid, focused_field } => {
                    focus_context.update_focused_field(pid, focused_field);
                }
                NativeAxObservation::Event(observation) => {
                    let transition = match &observation {
                        NativeAxEvent::WindowFocused { pid, window, .. }
                        | NativeAxEvent::WindowTitleChanged { pid, window, .. } => {
                            focus_context.observe_window(*pid, window.clone())
                        }
                        NativeAxEvent::UiFocused { .. }
                        | NativeAxEvent::UiValueChanged(_)
                        | NativeAxEvent::PageLoaded { .. } => None,
                    };
                    publish_focus_transition(snapshot_trigger_publisher, transition);
                    publish_focus_observation(&focus_context, &observation);
                    if let NativeAxEvent::PageLoaded { pid } = &observation
                        && focus_context
                            .current()
                            .is_some_and(|focus| focus.app.pid == i64::from(*pid))
                        && let Some(observer) = chrome_observer.as_ref()
                    {
                        observer.page_loaded(i64::from(*pid));
                    }
                    if let Some(event) = builder.event(observation) {
                        output.send(event);
                    }
                }
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
        &focus_context,
    );
    degraded_operations.fetch_add(api.take_degraded_operations(), Ordering::Relaxed);
    output.flush();
    observer_health.clear();
    failure_publisher.clear();
}

fn focused_window(api: &mut impl AxApi, app: &ApplicationInfo) -> Option<NativeWindow> {
    i32::try_from(app.pid)
        .ok()
        .and_then(|pid| api.focused_window(pid).ok().flatten())
}

fn send_observations(
    output: &mut AxOutput<'_>,
    builder: &mut AxEventBuilder,
    observations: Vec<NativeAxEvent>,
    focus_context: &FocusContext,
) {
    for observation in observations {
        publish_focus_observation(focus_context, &observation);
        if let Some(event) = builder.event(observation) {
            output.send(event);
        }
    }
}

fn attach_app(
    api: &mut impl AxApi,
    builder: &mut AxEventBuilder,
    app: ApplicationInfo,
    policy: &ManualAccessibilityPolicy,
    degraded: &AtomicU64,
    failures: &AxFailurePublisher,
    observer_health: &mut ObserverHealth,
) -> AttachResult {
    if app.activation_policy == ApplicationActivationPolicy::Prohibited {
        return AttachResult::default();
    }
    let Ok(pid) = i32::try_from(app.pid) else {
        observer_health.mark_unavailable(app.pid);
        failures.record(
            degraded,
            AxRecoverySite::Attach,
            AxFailure::new(
                Some(app.pid),
                AxFailurePhase::Attach,
                AxFailureKind::InvalidPid,
            ),
        );
        return AttachResult::default();
    };
    builder.add_app(app.clone());
    match api.attach(pid, app.raw_app(), policy.allows(&app.raw_app())) {
        Ok(observations) => {
            observer_health.mark_available(app.pid);
            failures.recover(Some(app.pid), AxRecoverySite::Attach);
            observations
                .into_iter()
                .map(NativeAxEvent::internalize_focus)
                .fold(AttachResult::default(), |mut attached, observation| {
                    match observation {
                        NativeAxObservation::FocusedFieldObserved { pid, focused_field } => {
                            attached.focused_field = Some((pid, focused_field));
                        }
                        NativeAxObservation::Event(NativeAxEvent::UiValueChanged(event)) => {
                            attached
                                .output
                                .extend(builder.event(NativeAxEvent::UiValueChanged(event)));
                        }
                        NativeAxObservation::Event(_) => {}
                    }
                    attached
                })
        }
        Err(failure) => {
            observer_health.mark_unavailable(app.pid);
            failures.record(degraded, AxRecoverySite::Attach, failure);
            AttachResult::default()
        }
    }
}

#[derive(Default)]
struct AttachResult {
    output: Vec<AxEvent>,
    focused_field: Option<(i32, Option<FocusedField>)>,
}

fn publish_attached_focus(focus_context: &FocusContext, attached: &AttachResult) {
    if let Some((pid, focused_field)) = attached.focused_field {
        focus_context.update_focused_field(pid, focused_field);
    }
}

fn publish_focus_observation(focus_context: &FocusContext, observation: &NativeAxEvent) {
    let Some((pid, focused_field)) = observation.focused_field_observation() else {
        return;
    };
    focus_context.update_focused_field(pid, focused_field);
}

#[cfg(test)]
mod tests;
