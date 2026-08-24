use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, sync_channel},
    },
    time::{Duration, Instant},
};

use zanei_collector::RawEvent;
use zanei_core::{
    config::FilterConfig,
    privacy::PrivacyScope,
    schema::{App, EventData, FieldKind},
};

mod chromium;
mod manual_accessibility;
mod title;

use super::{
    ApplicationActivationPolicy, ApplicationInfo, AxApi, AxEventBuilder, ClickObservation,
    NativeAxEvent, NativeHitTest, ObserverHealth, WorkspaceEvent, attach_app, click_channel,
    publish_focus_observation, run_ax_loop,
};
use crate::{
    CapturePolicy,
    chrome::{ChromeEligibilityObservation, chrome_eligibility_channel},
    content_snapshot::{SnapshotTriggerMessage, snapshot_trigger_channel},
    ffi::ax::{
        ManualAccessibilityPolicy, NativeAxObservation, NativeElement, NativeUiValueEvent,
        NativeWindow,
    },
    focus_context::FocusContext,
    focused_field::{FieldClass, FocusedField},
    text_capture::input_authorization_channel,
};

pub(super) fn capture_policy() -> CapturePolicy {
    let filter = FilterConfig::default();
    let (_, tracker) = chrome_eligibility_channel(filter.clone());
    CapturePolicy::new(tracker, filter, None)
}

fn builder() -> AxEventBuilder {
    AxEventBuilder::new(capture_policy())
}

fn manual_accessibility_policy() -> ManualAccessibilityPolicy {
    ManualAccessibilityPolicy::new(true, false, FilterConfig::default())
}

fn app() -> ApplicationInfo {
    app_with_policy(7, ApplicationActivationPolicy::Regular)
}

fn app_with_policy(pid: i64, activation_policy: ApplicationActivationPolicy) -> ApplicationInfo {
    ApplicationInfo {
        name: "Example".to_owned(),
        bundle_id: Some("dev.example.App".to_owned()),
        pid,
        activation_policy,
    }
}

fn window() -> NativeWindow {
    NativeWindow {
        title: Some("Example".to_owned()),
        id: Some(1),
    }
}

fn element(role: &str, subrole: Option<&str>) -> NativeElement {
    NativeElement {
        role: Some(role.to_owned()),
        subrole: subrole.map(str::to_owned),
        title: None,
        value: None,
        value_len: Some(3),
    }
}

fn focused_field_observation(pid: i32, generation: u64, class: FieldClass) -> NativeAxObservation {
    NativeAxObservation::FocusedFieldObserved {
        pid,
        focused_field: Some(FocusedField { generation, class }),
    }
}

fn focused_event(pid: i32, generation: u64, role: &str) -> NativeAxEvent {
    NativeAxEvent::UiFocused {
        pid,
        generation,
        window: Some(window()),
        element: Some(element(role, None)),
        observed_at: time::OffsetDateTime::UNIX_EPOCH,
    }
}

#[test]
fn ui_events_derive_field_kind_from_the_ax_snapshot() {
    let mut builder = builder();
    builder.add_app(app());
    let focus = builder
        .event(NativeAxEvent::UiFocused {
            pid: 7,
            generation: 1,
            window: Some(window()),
            element: Some(element("AXTextField", Some("AXSearchField"))),
            observed_at: time::OffsetDateTime::UNIX_EPOCH,
        })
        .expect("search field focus should emit")
        .into_parts()
        .0;
    let value = builder
        .event(NativeAxEvent::UiValueChanged(Box::new(
            NativeUiValueEvent {
                pid: 7,
                window: Some(window()),
                element: element("AXIncrementor", None),
                text: Some("1".to_owned()),
                capture_decision: None,
                observed_at: time::OffsetDateTime::UNIX_EPOCH,
            },
        )))
        .expect("numeric value change should emit")
        .into_parts()
        .0;

    let EventData::UiFocus(focus) = focus.data else {
        panic!("expected ui.focus");
    };
    let EventData::UiValue(value) = value.data else {
        panic!("expected ui.value");
    };
    assert_eq!(focus.field_kind, Some(FieldKind::Search));
    assert_eq!(value.field_kind, Some(FieldKind::Number));
    assert_eq!(value.text.as_deref(), Some("1"));
}

#[test]
fn chrome_ui_value_keeps_its_read_decision_for_output() {
    let filter = FilterConfig::default();
    let (publisher, tracker) = chrome_eligibility_channel(filter.clone());
    let policy = CapturePolicy::new(tracker, filter, None);
    let mut builder = AxEventBuilder::new(policy.clone());
    let chrome_app = ApplicationInfo {
        name: "Google Chrome".to_owned(),
        bundle_id: Some("com.google.Chrome".to_owned()),
        pid: 7,
        activation_policy: ApplicationActivationPolicy::Regular,
    };
    builder.add_app(chrome_app.clone());

    publisher.observe(
        7,
        ChromeEligibilityObservation::Incognito { window_id: Some(1) },
    );
    let incognito_decision =
        policy.decision(PrivacyScope::TextContent, &chrome_app.raw_app(), Some(1));
    let (incognito, bound_incognito_decision) = builder
        .event(NativeAxEvent::UiValueChanged(Box::new(
            NativeUiValueEvent {
                pid: 7,
                window: Some(window()),
                element: element("AXTextField", None),
                text: Some("private".to_owned()),
                capture_decision: Some(incognito_decision.clone()),
                observed_at: time::OffsetDateTime::UNIX_EPOCH,
            },
        )))
        .expect("ui.value metadata remains available")
        .into_parts();
    let EventData::UiValue(incognito) = incognito.data else {
        panic!("expected ui.value");
    };
    assert_eq!(incognito.text.as_deref(), Some("private"));
    assert_eq!(bound_incognito_decision, Some(incognito_decision));

    publisher.observe(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(1),
            url: "https://example.com".to_owned(),
        },
    );
    let normal_decision =
        policy.decision(PrivacyScope::TextContent, &chrome_app.raw_app(), Some(1));
    let (normal, bound_normal_decision) = builder
        .event(NativeAxEvent::UiValueChanged(Box::new(
            NativeUiValueEvent {
                pid: 7,
                window: Some(window()),
                element: element("AXTextField", None),
                text: Some("normal".to_owned()),
                capture_decision: Some(normal_decision.clone()),
                observed_at: time::OffsetDateTime::UNIX_EPOCH,
            },
        )))
        .expect("ui.value event")
        .into_parts();
    let EventData::UiValue(normal) = normal.data else {
        panic!("expected ui.value");
    };
    assert_eq!(normal.text.as_deref(), Some("normal"));
    assert_eq!(bound_normal_decision, Some(normal_decision));
}

#[test]
fn cleared_focus_does_not_emit_a_ui_focus_event() {
    let mut builder = builder();
    builder.add_app(app());
    let focus_context = FocusContext::new();
    focus_context.activate(app(), Some(window()));
    focus_context.update_focused_field(
        7,
        Some(FocusedField {
            generation: 1,
            class: FieldClass::KnownText(FieldKind::Text),
        }),
    );
    let cleared = NativeAxEvent::UiFocused {
        pid: 7,
        generation: 2,
        window: None,
        element: None,
        observed_at: time::OffsetDateTime::UNIX_EPOCH,
    };

    publish_focus_observation(&focus_context, &cleared);

    assert!(builder.event(cleared).is_none());
    assert_eq!(
        focus_context
            .current()
            .and_then(|focus| focus.focused_field),
        None
    );
}

#[derive(Default)]
struct FakeAxApi {
    running_applications: Vec<ApplicationInfo>,
    frontmost_application: Option<ApplicationInfo>,
    attached_pids: Vec<i32>,
    attach_events: Vec<NativeAxEvent>,
    attach_results: VecDeque<Result<Vec<NativeAxEvent>, ()>>,
    poll_observations: VecDeque<Vec<NativeAxObservation>>,
    current_degraded_observers: Option<Arc<AtomicU64>>,
    observed_degraded_observers: Option<u64>,
    stop_after_poll: Option<Arc<AtomicBool>>,
    stop_after_polls: Option<usize>,
    polls: usize,
    attached_apps: Vec<App>,
    reconciled_manual_accessibility: Vec<bool>,
    replacement_on_first_poll: Option<(ManualAccessibilityPolicy, FilterConfig)>,
    focused_window: Option<NativeWindow>,
}

impl FakeAxApi {
    fn chromium_profile() -> Self {
        let chrome = ApplicationInfo {
            name: "Google Chrome".to_owned(),
            bundle_id: Some("com.google.Chrome".to_owned()),
            pid: 7,
            activation_policy: ApplicationActivationPolicy::Regular,
        };
        Self {
            running_applications: vec![chrome.clone()],
            frontmost_application: Some(chrome),
            // Resolved from bounds because Chromium exposes no AXWindowNumber.
            focused_window: Some(NativeWindow {
                title: Some("Chromium".to_owned()),
                id: Some(11),
            }),
            // Chromium sends no focused-window notification on activation.
            attach_events: Vec::new(),
            ..Self::default()
        }
    }
}

impl AxApi for FakeAxApi {
    type AttachError = ();

    fn running_applications(&self) -> Vec<ApplicationInfo> {
        self.running_applications.clone()
    }

    fn frontmost_application(&self) -> Option<ApplicationInfo> {
        self.frontmost_application.clone()
    }

    fn attach(
        &mut self,
        pid: i32,
        app: App,
        _manual_accessibility: bool,
    ) -> Result<Vec<NativeAxEvent>, Self::AttachError> {
        self.attached_pids.push(pid);
        self.attached_apps.push(app);
        self.attach_results
            .pop_front()
            .unwrap_or_else(|| Ok(std::mem::take(&mut self.attach_events)))
    }

    fn detach(&mut self, _pid: i32) -> Vec<NativeAxEvent> {
        Vec::new()
    }

    fn focused_window(&mut self, _pid: i32) -> Result<Option<NativeWindow>, Self::AttachError> {
        Ok(self.focused_window.clone())
    }

    fn reconcile_manual_accessibility(&mut self, policy: &ManualAccessibilityPolicy) {
        self.reconciled_manual_accessibility
            .extend(self.attached_apps.iter().map(|app| policy.allows(app)));
    }

    fn poll(&mut self, _timeout: Duration) -> Vec<NativeAxObservation> {
        self.polls = self.polls.saturating_add(1);
        if self.polls == 1
            && let Some((policy, filter)) = self.replacement_on_first_poll.take()
        {
            policy.replace_filter(filter);
        }
        self.observed_degraded_observers = self
            .current_degraded_observers
            .as_ref()
            .map(|current| current.load(Ordering::Relaxed));
        if self.stop_after_polls.is_none()
            && let Some(stop) = self.stop_after_poll.as_ref()
        {
            stop.store(true, Ordering::Release);
        }
        if self.stop_after_polls == Some(self.polls)
            && let Some(stop) = self.stop_after_poll.as_ref()
        {
            stop.store(true, Ordering::Release);
        }
        self.poll_observations.pop_front().unwrap_or_default()
    }

    fn flush_pending(&mut self) -> Vec<NativeAxEvent> {
        Vec::new()
    }

    fn hit_test(&self, _click: ClickObservation) -> Option<NativeHitTest> {
        None
    }

    fn take_dropped_events(&self) -> u64 {
        0
    }

    fn take_degraded_operations(&self) -> u64 {
        0
    }
}

fn run_fake_ax_loop(
    api: &mut FakeAxApi,
    stop: &AtomicBool,
    lifecycle_receiver: &Receiver<WorkspaceEvent>,
    degraded_operations: &AtomicU64,
    current_degraded_observers: Arc<AtomicU64>,
) {
    run_fake_ax_loop_with_policy(
        api,
        stop,
        lifecycle_receiver,
        degraded_operations,
        current_degraded_observers,
        manual_accessibility_policy(),
    );
}

fn run_fake_ax_loop_with_policy(
    api: &mut FakeAxApi,
    stop: &AtomicBool,
    lifecycle_receiver: &Receiver<WorkspaceEvent>,
    degraded_operations: &AtomicU64,
    current_degraded_observers: Arc<AtomicU64>,
    manual_policy: ManualAccessibilityPolicy,
) {
    let _ = run_fake_ax_loop_with_context(
        api,
        stop,
        lifecycle_receiver,
        degraded_operations,
        current_degraded_observers,
        manual_policy,
        FocusContext::new(),
    );
}

#[allow(clippy::too_many_arguments)]
fn run_fake_ax_loop_with_context(
    api: &mut FakeAxApi,
    stop: &AtomicBool,
    lifecycle_receiver: &Receiver<WorkspaceEvent>,
    degraded_operations: &AtomicU64,
    current_degraded_observers: Arc<AtomicU64>,
    manual_policy: ManualAccessibilityPolicy,
    focus_context: FocusContext,
) -> Vec<RawEvent> {
    let (_click_sender, click_receiver) = click_channel();
    let (output_sender, output_receiver) = sync_channel(16);
    api.current_degraded_observers = Some(Arc::clone(&current_degraded_observers));
    run_ax_loop(
        api,
        stop,
        &output_sender,
        lifecycle_receiver,
        &click_receiver,
        capture_policy(),
        None,
        manual_policy,
        focus_context,
        None,
        &AtomicU64::new(0),
        degraded_operations,
        current_degraded_observers,
    );
    output_receiver.try_iter().collect()
}

#[test]
fn initial_enumeration_attaches_regular_and_accessory_applications_only() {
    let mut api = FakeAxApi {
        running_applications: vec![
            app_with_policy(7, ApplicationActivationPolicy::Regular),
            app_with_policy(8, ApplicationActivationPolicy::Accessory),
            app_with_policy(9, ApplicationActivationPolicy::Prohibited),
        ],
        ..FakeAxApi::default()
    };
    let (_lifecycle_sender, lifecycle_receiver) = sync_channel(1);
    let degraded_operations = AtomicU64::new(0);
    let current_degraded_observers = Arc::new(AtomicU64::new(0));

    run_fake_ax_loop(
        &mut api,
        &AtomicBool::new(true),
        &lifecycle_receiver,
        &degraded_operations,
        Arc::clone(&current_degraded_observers),
    );

    assert_eq!(api.attached_pids, vec![7, 8]);
    assert_eq!(degraded_operations.load(Ordering::Relaxed), 0);
    assert_eq!(current_degraded_observers.load(Ordering::Relaxed), 0);
}

#[test]
fn attach_known_field_syncs_frontmost_tracker_without_output() {
    let target = app();
    let mut api = FakeAxApi {
        running_applications: vec![target.clone()],
        frontmost_application: Some(target),
        attach_events: vec![focused_event(7, 1, "AXTextField")],
        ..FakeAxApi::default()
    };
    let (_lifecycle_sender, lifecycle_receiver) = sync_channel(1);
    let focus_context = FocusContext::new();

    let events = run_fake_ax_loop_with_context(
        &mut api,
        &AtomicBool::new(true),
        &lifecycle_receiver,
        &AtomicU64::new(0),
        Arc::new(AtomicU64::new(0)),
        manual_accessibility_policy(),
        focus_context.clone(),
    );

    assert_eq!(
        focus_context
            .current()
            .and_then(|focus| focus.focused_field),
        Some(FocusedField {
            generation: 1,
            class: FieldClass::KnownText(FieldKind::Text),
        })
    );
    assert!(events.is_empty());
}

#[test]
fn delayed_unknown_to_known_updates_tracker_without_output() {
    let stop = Arc::new(AtomicBool::new(false));
    let target = app();
    let mut api = FakeAxApi {
        running_applications: vec![target.clone()],
        frontmost_application: Some(target),
        attach_events: vec![focused_event(7, 1, "AXDocument")],
        poll_observations: VecDeque::from([vec![focused_field_observation(
            7,
            1,
            FieldClass::KnownText(FieldKind::Text),
        )]]),
        stop_after_poll: Some(Arc::clone(&stop)),
        ..FakeAxApi::default()
    };
    let (_lifecycle_sender, lifecycle_receiver) = sync_channel(1);
    let focus_context = FocusContext::new();

    let events = run_fake_ax_loop_with_context(
        &mut api,
        stop.as_ref(),
        &lifecycle_receiver,
        &AtomicU64::new(0),
        Arc::new(AtomicU64::new(0)),
        manual_accessibility_policy(),
        focus_context.clone(),
    );

    assert_eq!(
        focus_context
            .current()
            .and_then(|focus| focus.focused_field),
        Some(FocusedField {
            generation: 1,
            class: FieldClass::KnownText(FieldKind::Text),
        })
    );
    assert!(events.is_empty());
}

#[test]
fn background_reconcile_is_ignored_and_not_emitted() {
    let stop = Arc::new(AtomicBool::new(false));
    let frontmost = app();
    let background = app_with_policy(8, ApplicationActivationPolicy::Regular);
    let mut api = FakeAxApi {
        running_applications: vec![frontmost.clone(), background],
        frontmost_application: Some(frontmost),
        attach_results: VecDeque::from([
            Ok(vec![focused_event(7, 1, "AXDocument")]),
            Ok(vec![focused_event(8, 1, "AXDocument")]),
        ]),
        poll_observations: VecDeque::from([vec![focused_field_observation(
            8,
            1,
            FieldClass::KnownText(FieldKind::Text),
        )]]),
        stop_after_poll: Some(Arc::clone(&stop)),
        ..FakeAxApi::default()
    };
    let (_lifecycle_sender, lifecycle_receiver) = sync_channel(1);
    let focus_context = FocusContext::new();

    let events = run_fake_ax_loop_with_context(
        &mut api,
        stop.as_ref(),
        &lifecycle_receiver,
        &AtomicU64::new(0),
        Arc::new(AtomicU64::new(0)),
        manual_accessibility_policy(),
        focus_context.clone(),
    );

    let current = focus_context.current().expect("frontmost focus");
    assert_eq!(current.app.pid, 7);
    assert_eq!(
        current.focused_field,
        Some(FocusedField {
            generation: 1,
            class: FieldClass::Unknown,
        })
    );
    assert!(events.is_empty());
}

#[test]
fn real_focus_then_reconcile_preserves_latest_tracker_and_emits_real_ui_focus() {
    let stop = Arc::new(AtomicBool::new(false));
    let target = app();
    let mut api = FakeAxApi {
        running_applications: vec![target.clone()],
        frontmost_application: Some(target),
        attach_events: vec![focused_event(7, 1, "AXDocument")],
        poll_observations: VecDeque::from([vec![
            NativeAxObservation::Event(NativeAxEvent::UiFocused {
                pid: 7,
                generation: 2,
                window: Some(window()),
                element: Some(element("AXTextArea", None)),
                observed_at: time::OffsetDateTime::UNIX_EPOCH,
            }),
            focused_field_observation(7, 3, FieldClass::KnownSafeNonText),
        ]]),
        stop_after_poll: Some(Arc::clone(&stop)),
        ..FakeAxApi::default()
    };
    let (_lifecycle_sender, lifecycle_receiver) = sync_channel(1);
    let focus_context = FocusContext::new();

    let events = run_fake_ax_loop_with_context(
        &mut api,
        stop.as_ref(),
        &lifecycle_receiver,
        &AtomicU64::new(0),
        Arc::new(AtomicU64::new(0)),
        manual_accessibility_policy(),
        focus_context.clone(),
    );

    assert_eq!(
        focus_context
            .current()
            .and_then(|focus| focus.focused_field),
        Some(FocusedField {
            generation: 3,
            class: FieldClass::KnownSafeNonText,
        })
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "ui.focus");
}

#[test]
fn launched_prohibited_application_is_not_attached() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut api = FakeAxApi {
        stop_after_poll: Some(Arc::clone(&stop)),
        ..FakeAxApi::default()
    };
    let (lifecycle_sender, lifecycle_receiver) = sync_channel(1);
    lifecycle_sender
        .send(WorkspaceEvent::Launched(app_with_policy(
            9,
            ApplicationActivationPolicy::Prohibited,
        )))
        .expect("lifecycle receiver should be connected");
    let degraded_operations = AtomicU64::new(0);
    let current_degraded_observers = Arc::new(AtomicU64::new(0));

    run_fake_ax_loop(
        &mut api,
        stop.as_ref(),
        &lifecycle_receiver,
        &degraded_operations,
        Arc::clone(&current_degraded_observers),
    );

    assert!(api.attached_pids.is_empty());
    assert_eq!(degraded_operations.load(Ordering::Relaxed), 0);
    assert_eq!(current_degraded_observers.load(Ordering::Relaxed), 0);
}

#[test]
fn prohibited_application_is_skipped_before_pid_and_failure_accounting() {
    let mut api = FakeAxApi::default();
    let mut builder = builder();
    let degraded_operations = AtomicU64::new(0);
    let current_degraded_observers = Arc::new(AtomicU64::new(0));
    let mut observer_health = ObserverHealth::new(Arc::clone(&current_degraded_observers));

    let pending = attach_app(
        &mut api,
        &mut builder,
        app_with_policy(i64::MAX, ApplicationActivationPolicy::Prohibited),
        &manual_accessibility_policy(),
        &degraded_operations,
        &mut observer_health,
    );

    assert!(pending.output.is_empty());
    assert!(pending.focused_field.is_none());
    assert!(api.attached_pids.is_empty());
    assert_eq!(degraded_operations.load(Ordering::Relaxed), 0);
    assert_eq!(current_degraded_observers.load(Ordering::Relaxed), 0);
}

#[test]
fn frontmost_focus_notification_updates_focus_context() {
    let focus = NativeAxEvent::UiFocused {
        pid: 7,
        generation: 4,
        window: Some(window()),
        element: Some(element("AXTextArea", None)),
        observed_at: time::OffsetDateTime::UNIX_EPOCH,
    };
    let context = FocusContext::new();
    context.activate(app(), Some(window()));
    publish_focus_observation(&context, &focus);
    assert_eq!(
        context.current().and_then(|focus| focus.focused_field),
        Some(FocusedField {
            generation: 4,
            class: FieldClass::KnownText(FieldKind::Text),
        })
    );
}

#[test]
fn did_wake_resyncs_focus_and_publishes_a_focus_trigger() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut api = FakeAxApi {
        frontmost_application: Some(app()),
        focused_window: Some(window()),
        stop_after_poll: Some(Arc::clone(&stop)),
        ..FakeAxApi::default()
    };
    let (lifecycle_sender, lifecycle_receiver) = sync_channel(1);
    lifecycle_sender
        .send(WorkspaceEvent::DidWake)
        .expect("queue wake event");
    let (_click_sender, click_receiver) = click_channel();
    let (output_sender, _output_receiver) = sync_channel(1);
    let focus_context = FocusContext::new();
    focus_context.activate(app(), Some(window()));
    let (trigger_publisher, trigger_receiver) = snapshot_trigger_channel();
    let current_degraded_observers = Arc::new(AtomicU64::new(0));

    run_ax_loop(
        &mut api,
        stop.as_ref(),
        &output_sender,
        &lifecycle_receiver,
        &click_receiver,
        capture_policy(),
        None,
        manual_accessibility_policy(),
        focus_context.clone(),
        Some(&trigger_publisher),
        &AtomicU64::new(0),
        &AtomicU64::new(0),
        current_degraded_observers,
    );

    let SnapshotTriggerMessage::FocusTransition { transition, .. } =
        trigger_receiver.try_recv().expect("wake resync transition")
    else {
        panic!("FocusTransition message");
    };
    assert!(transition.resynced);
    assert!(transition.current.is_some());
    assert_eq!(focus_context.generation(), 2);
}

#[test]
fn did_wake_without_frontmost_app_clears_stale_focus() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut api = FakeAxApi {
        stop_after_poll: Some(Arc::clone(&stop)),
        ..FakeAxApi::default()
    };
    let (lifecycle_sender, lifecycle_receiver) = sync_channel(1);
    lifecycle_sender
        .send(WorkspaceEvent::DidWake)
        .expect("queue wake event");
    let (_click_sender, click_receiver) = click_channel();
    let (output_sender, _output_receiver) = sync_channel(1);
    let focus_context = FocusContext::new();
    focus_context.activate(app(), Some(window()));
    let transitions = focus_context.subscribe();
    let current_degraded_observers = Arc::new(AtomicU64::new(0));

    run_ax_loop(
        &mut api,
        stop.as_ref(),
        &output_sender,
        &lifecycle_receiver,
        &click_receiver,
        capture_policy(),
        None,
        manual_accessibility_policy(),
        focus_context.clone(),
        None,
        &AtomicU64::new(0),
        &AtomicU64::new(0),
        current_degraded_observers,
    );

    let transition = transitions.try_recv().expect("wake clear transition");
    assert!(transition.resynced);
    assert!(transition.current.is_none());
    assert!(focus_context.current().is_none());
    assert_eq!(focus_context.generation(), 2);
}

#[test]
fn observer_health_clears_each_pid_only_after_its_recovery() {
    let published = Arc::new(AtomicU64::new(0));
    let mut health = ObserverHealth::new(Arc::clone(&published));

    health.mark_used(7);
    health.mark_unavailable(7);
    assert_eq!(published.load(Ordering::Relaxed), 1);
    health.mark_used(8);
    health.mark_unavailable(8);
    assert_eq!(published.load(Ordering::Relaxed), 2);

    health.mark_available(7);
    assert_eq!(published.load(Ordering::Relaxed), 1);
    health.mark_available(8);
    assert_eq!(published.load(Ordering::Relaxed), 0);
}

#[test]
fn failed_attach_for_unactivated_app_is_not_degraded() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut api = FakeAxApi {
        running_applications: vec![app()],
        attach_results: VecDeque::from([Err(())]),
        stop_after_poll: Some(Arc::clone(&stop)),
        ..FakeAxApi::default()
    };
    let (_lifecycle_sender, lifecycle_receiver) = sync_channel(1);
    let degraded_operations = AtomicU64::new(0);
    let current_degraded_observers = Arc::new(AtomicU64::new(0));

    run_fake_ax_loop(
        &mut api,
        stop.as_ref(),
        &lifecycle_receiver,
        &degraded_operations,
        Arc::clone(&current_degraded_observers),
    );

    assert_eq!(api.attached_pids, vec![7]);
    assert_eq!(api.observed_degraded_observers, Some(0));
    assert_eq!(current_degraded_observers.load(Ordering::Relaxed), 0);
    assert_eq!(degraded_operations.load(Ordering::Relaxed), 1);
}

#[test]
fn failed_attach_for_activated_app_is_degraded() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut api = FakeAxApi {
        attach_results: VecDeque::from([Err(())]),
        stop_after_poll: Some(Arc::clone(&stop)),
        ..FakeAxApi::default()
    };
    let (lifecycle_sender, lifecycle_receiver) = sync_channel(1);
    lifecycle_sender
        .send(WorkspaceEvent::Activated(app()))
        .expect("lifecycle receiver should be connected");
    let degraded_operations = AtomicU64::new(0);
    let current_degraded_observers = Arc::new(AtomicU64::new(0));

    run_fake_ax_loop(
        &mut api,
        stop.as_ref(),
        &lifecycle_receiver,
        &degraded_operations,
        Arc::clone(&current_degraded_observers),
    );

    assert_eq!(api.attached_pids, vec![7]);
    assert_eq!(api.observed_degraded_observers, Some(1));
    assert_eq!(current_degraded_observers.load(Ordering::Relaxed), 0);
    assert_eq!(degraded_operations.load(Ordering::Relaxed), 1);
}

#[test]
fn successful_reattach_clears_activated_app_degradation() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut api = FakeAxApi {
        attach_results: VecDeque::from([Err(()), Ok(Vec::new()), Ok(Vec::new())]),
        stop_after_poll: Some(Arc::clone(&stop)),
        ..FakeAxApi::default()
    };
    let (lifecycle_sender, lifecycle_receiver) = sync_channel(3);
    for activated_app in [
        app(),
        app_with_policy(8, ApplicationActivationPolicy::Regular),
        app(),
    ] {
        lifecycle_sender
            .send(WorkspaceEvent::Activated(activated_app))
            .expect("lifecycle receiver should be connected");
    }
    let degraded_operations = AtomicU64::new(0);
    let current_degraded_observers = Arc::new(AtomicU64::new(0));

    run_fake_ax_loop(
        &mut api,
        stop.as_ref(),
        &lifecycle_receiver,
        &degraded_operations,
        Arc::clone(&current_degraded_observers),
    );

    assert_eq!(api.attached_pids, vec![7, 8, 7]);
    assert_eq!(api.observed_degraded_observers, Some(0));
    assert_eq!(current_degraded_observers.load(Ordering::Relaxed), 0);
    assert_eq!(degraded_operations.load(Ordering::Relaxed), 1);
}

#[test]
fn initial_focus_snapshot_authorizes_same_generation_input() {
    let context = FocusContext::new();
    context.activate(app(), Some(window()));
    let focus = NativeAxEvent::UiFocused {
        pid: 7,
        generation: 4,
        window: Some(window()),
        element: Some(element("AXTextArea", None)),
        observed_at: time::OffsetDateTime::UNIX_EPOCH,
    };
    publish_focus_observation(&context, &focus);
    let focused = context
        .current()
        .and_then(|focus| focus.focused_field)
        .expect("initial focus should be available to EventTap");
    let input_at = Instant::now();
    let (authorization_publisher, mut authorizations) = input_authorization_channel();

    let authorization = authorization_publisher
        .prepare(7, focused.generation, input_at)
        .expect("authorization channel should accept the reservation");
    authorization.confirm();

    assert!(authorizations.matching_for_test(7, focused.generation, input_at));
}
