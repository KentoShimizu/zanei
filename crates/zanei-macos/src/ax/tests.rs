use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, sync_channel},
    },
    time::{Duration, Instant},
};

use zanei_core::{
    config::FilterConfig,
    schema::{EventData, FieldKind},
};

use super::{
    ApplicationActivationPolicy, ApplicationInfo, AxApi, AxEventBuilder, ClickObservation,
    NativeAxEvent, NativeHitTest, ObserverHealth, WorkspaceEvent, attach_app, click_channel,
    publish_focus_observation, run_ax_loop,
};
use crate::{
    chrome::chrome_eligibility_channel,
    ffi::ax::{NativeElement, NativeWindow},
    focused_field::{FieldClass, FocusedField, focused_field_channel},
    text_capture::{TextContentPolicy, input_authorization_channel},
};

fn text_policy() -> TextContentPolicy {
    let (_, tracker) = chrome_eligibility_channel(FilterConfig::default());
    TextContentPolicy::new(tracker)
}

fn builder() -> AxEventBuilder {
    AxEventBuilder::new(text_policy())
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

#[test]
fn title_changes_include_the_previous_title() {
    let mut builder = builder();
    builder.add_app(app());
    let first = NativeWindow {
        title: Some("First".to_owned()),
        id: Some(1),
    };
    let second = NativeWindow {
        title: Some("Second".to_owned()),
        id: Some(1),
    };
    let _ = builder.event(NativeAxEvent::WindowFocused {
        pid: 7,
        window: first,
    });
    let event = builder
        .event(NativeAxEvent::WindowTitleChanged {
            pid: 7,
            window: second,
        })
        .unwrap();

    let EventData::WindowTitle(data) = event.data else {
        panic!("expected a window.title payload");
    };
    assert_eq!(data.prev_title.as_deref(), Some("First"));
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
        })
        .expect("search field focus should emit");
    let value = builder
        .event(NativeAxEvent::UiValueChanged {
            pid: 7,
            window: Some(window()),
            element: element("AXIncrementor", None),
            text: Some("1".to_owned()),
        })
        .expect("numeric value change should emit");

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
fn chrome_ui_value_text_follows_window_eligibility() {
    let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let mut builder = AxEventBuilder::new(TextContentPolicy::new(tracker));
    builder.add_app(ApplicationInfo {
        name: "Google Chrome".to_owned(),
        bundle_id: Some("com.google.Chrome".to_owned()),
        pid: 7,
        activation_policy: ApplicationActivationPolicy::Regular,
    });

    publisher.publish_incognito(7, Some(1));
    let incognito = builder
        .event(NativeAxEvent::UiValueChanged {
            pid: 7,
            window: Some(window()),
            element: element("AXTextField", None),
            text: Some("private".to_owned()),
        })
        .expect("ui.value metadata remains available");
    let EventData::UiValue(incognito) = incognito.data else {
        panic!("expected ui.value");
    };
    assert_eq!(incognito.text, None);

    publisher.publish_normal(7, Some(1), "https://example.com");
    let normal = builder
        .event(NativeAxEvent::UiValueChanged {
            pid: 7,
            window: Some(window()),
            element: element("AXTextField", None),
            text: Some("normal".to_owned()),
        })
        .expect("ui.value event");
    let EventData::UiValue(normal) = normal.data else {
        panic!("expected ui.value");
    };
    assert_eq!(normal.text.as_deref(), Some("normal"));
}

#[test]
fn cleared_focus_does_not_emit_a_ui_focus_event() {
    let mut builder = builder();
    builder.add_app(app());
    let (publisher, tracker) = focused_field_channel();
    publisher.update(
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
    };

    publish_focus_observation(Some(&publisher), &cleared);

    assert!(builder.event(cleared).is_none());
    assert_eq!(tracker.focused_field(7), None);
}

#[derive(Default)]
struct FakeAxApi {
    running_applications: Vec<ApplicationInfo>,
    attached_pids: Vec<i32>,
    attach_observations: Vec<NativeAxEvent>,
    attach_results: VecDeque<Result<Vec<NativeAxEvent>, ()>>,
    current_degraded_observers: Option<Arc<AtomicU64>>,
    observed_degraded_observers: Option<u64>,
    stop_after_poll: Option<Arc<AtomicBool>>,
}

impl AxApi for FakeAxApi {
    type AttachError = ();

    fn running_applications(&self) -> Vec<ApplicationInfo> {
        self.running_applications.clone()
    }

    fn attach(&mut self, pid: i32) -> Result<Vec<NativeAxEvent>, Self::AttachError> {
        self.attached_pids.push(pid);
        self.attach_results
            .pop_front()
            .unwrap_or_else(|| Ok(std::mem::take(&mut self.attach_observations)))
    }

    fn detach(&mut self, _pid: i32) -> Vec<NativeAxEvent> {
        Vec::new()
    }

    fn poll(&mut self, _timeout: Duration) -> Vec<NativeAxEvent> {
        self.observed_degraded_observers = self
            .current_degraded_observers
            .as_ref()
            .map(|current| current.load(Ordering::Relaxed));
        if let Some(stop) = self.stop_after_poll.as_ref() {
            stop.store(true, Ordering::Release);
        }
        Vec::new()
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
    let (_click_sender, click_receiver) = click_channel();
    let (output_sender, _output_receiver) = sync_channel(1);
    api.current_degraded_observers = Some(Arc::clone(&current_degraded_observers));
    run_ax_loop(
        api,
        stop,
        &output_sender,
        lifecycle_receiver,
        &click_receiver,
        None,
        text_policy(),
        &AtomicU64::new(0),
        degraded_operations,
        current_degraded_observers,
    );
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
        None,
        &degraded_operations,
        &mut observer_health,
    );

    assert!(pending.is_empty());
    assert!(api.attached_pids.is_empty());
    assert_eq!(degraded_operations.load(Ordering::Relaxed), 0);
    assert_eq!(current_degraded_observers.load(Ordering::Relaxed), 0);
}

#[test]
fn attach_publishes_the_initial_focused_field() {
    let mut api = FakeAxApi {
        attach_observations: vec![NativeAxEvent::UiFocused {
            pid: 7,
            generation: 4,
            window: Some(window()),
            element: Some(element("AXTextArea", None)),
        }],
        ..FakeAxApi::default()
    };
    let mut builder = builder();
    let (publisher, tracker) = focused_field_channel();
    let current_degraded_observers = Arc::new(AtomicU64::new(0));
    let mut observer_health = ObserverHealth::new(Arc::clone(&current_degraded_observers));

    let pending = attach_app(
        &mut api,
        &mut builder,
        app(),
        Some(&publisher),
        &AtomicU64::new(0),
        &mut observer_health,
    );

    assert!(pending.is_empty());
    assert_eq!(
        tracker.focused_field(7),
        Some(FocusedField {
            generation: 4,
            class: FieldClass::KnownText(FieldKind::Text),
        })
    );
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
    let (focused_publisher, focused_tracker) = focused_field_channel();
    let focus = NativeAxEvent::UiFocused {
        pid: 7,
        generation: 4,
        window: Some(window()),
        element: Some(element("AXTextArea", None)),
    };
    publish_focus_observation(Some(&focused_publisher), &focus);
    let focused = focused_tracker
        .focused_field(7)
        .expect("initial focus should be available to EventTap");
    let input_at = Instant::now();
    let (authorization_publisher, mut authorizations) = input_authorization_channel();

    let authorization = authorization_publisher
        .prepare(7, focused.generation, input_at)
        .expect("authorization channel should accept the reservation");
    authorization.confirm();

    assert!(authorizations.matching_for_test(7, focused.generation, input_at));
}
