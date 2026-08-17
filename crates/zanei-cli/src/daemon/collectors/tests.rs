use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use zanei_collector::{Permission, RawEvent};
use zanei_core::{
    config::CaptureSource,
    store::{DaemonPermissions, PermissionState},
};

use super::{CollectorSet, Managed, ManagedCollector, SourceGate, supervise_collector};

#[test]
fn source_gate_maps_support_collector_events_to_configured_families() {
    let gate = SourceGate::new(&[
        CaptureSource::Window,
        CaptureSource::Input,
        CaptureSource::Browser,
    ]);

    assert!(!gate.allows_type("app.activate"));
    assert!(gate.allows_type("window.focus"));
    assert!(!gate.allows_type("ui.focus"));
    assert!(gate.allows_type("input.key"));
    assert!(gate.allows_type("clipboard.copy"));
    assert!(gate.allows_type("browser.navigate"));
    assert!(!gate.allows_type("future.event"));
}

#[test]
fn source_gate_drops_every_family_when_capture_is_disabled() {
    let gate = SourceGate::new(&[]);

    for event_type in [
        "app.launch",
        "window.title",
        "ui.value",
        "input.scroll",
        "clipboard.paste",
        "browser.navigate",
    ] {
        assert!(!gate.allows_type(event_type));
    }
}

#[test]
fn ui_only_gate_rejects_input_and_clipboard_events() {
    let gate = SourceGate::new(&[CaptureSource::Ui]);

    for event_type in ["ui.focus", "ui.click", "ui.value"] {
        assert!(gate.allows_type(event_type));
    }
    for event_type in [
        "input.key",
        "input.scroll",
        "clipboard.copy",
        "clipboard.paste",
    ] {
        assert!(!gate.allows_type(event_type));
    }
}

#[test]
fn permissions_come_from_the_concrete_collectors_selected_by_config() {
    let mut config = zanei_core::config::Config::default();
    config.capture.sources = vec![CaptureSource::App];
    assert!(CollectorSet::new(&config).required_permissions().is_empty());

    config.capture.sources = vec![CaptureSource::Ui];
    let collectors = CollectorSet::new(&config);
    assert!(collectors.ax.is_some());
    assert!(collectors.eventtap.is_some());
    assert!(
        !collectors
            .eventtap
            .as_ref()
            .is_some_and(|eventtap| { eventtap.collector.secure_input_enabled() })
    );
    assert_eq!(
        collectors.required_permissions(),
        [Permission::Accessibility, Permission::InputMonitoring]
            .into_iter()
            .collect()
    );

    config.capture.text_content = true;
    let mut collectors = CollectorSet::new(&config);
    assert!(
        collectors
            .eventtap
            .as_mut()
            .expect("ui capture eventtap")
            .collector
            .prepare_main_thread()
            .expect("click-only eventtap preparation")
            .is_none()
    );
    config.capture.text_content = false;

    config.capture.sources = vec![CaptureSource::Ui, CaptureSource::Input];
    let permissions = CollectorSet::new(&config).required_permissions();
    assert_eq!(
        permissions,
        [Permission::Accessibility, Permission::InputMonitoring]
            .into_iter()
            .collect()
    );

    config.capture.sources = vec![CaptureSource::Input];
    let collectors = CollectorSet::new(&config);
    assert!(collectors.ax.is_none());
    assert!(collectors.eventtap.is_some());
    assert_eq!(
        collectors.required_permissions(),
        [Permission::InputMonitoring].into_iter().collect()
    );

    config.capture.text_content = true;
    let collectors = CollectorSet::new(&config);
    assert!(collectors.workspace.is_some());
    assert!(collectors.chrome.is_some());
    assert_eq!(
        collectors.required_permissions(),
        [
            Permission::InputMonitoring,
            Permission::Automation {
                bundle_id: "com.google.Chrome".to_owned(),
            },
        ]
        .into_iter()
        .collect()
    );

    config.capture.text_content = false;
    config.capture.sources = vec![CaptureSource::Browser];
    assert_eq!(
        CollectorSet::new(&config).required_permissions(),
        [Permission::Automation {
            bundle_id: "com.google.Chrome".to_owned(),
        }]
        .into_iter()
        .collect()
    );
}

#[test]
fn text_content_chrome_automation_permission_matrix() {
    let automation = Permission::Automation {
        bundle_id: "com.google.Chrome".to_owned(),
    };
    let mut config = zanei_core::config::Config::default();
    config.capture.text_content = true;

    for (source, expected) in [
        (
            CaptureSource::Window,
            BTreeSet::from([Permission::Accessibility]),
        ),
        (
            CaptureSource::Ui,
            BTreeSet::from([
                Permission::Accessibility,
                Permission::InputMonitoring,
                automation.clone(),
            ]),
        ),
        (
            CaptureSource::Input,
            BTreeSet::from([Permission::InputMonitoring, automation.clone()]),
        ),
    ] {
        config.capture.sources = vec![source];

        assert_eq!(CollectorSet::new(&config).required_permissions(), expected);
    }
}

#[test]
fn unexpected_collector_exit_is_degraded_and_restarted_after_backoff() {
    let state = Arc::new(FakeState::default());
    let mut managed = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::new(),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();
    let started = Instant::now();
    super::start_collector(&mut managed, &pipeline, &mut errors, started);
    state.finish();
    wait_for_relay(&managed);

    supervise_collector(
        &mut managed,
        &pipeline,
        &granted_permissions(),
        &mut errors,
        started,
    )
    .expect("supervise failed collector");
    assert_eq!(state.starts.load(Ordering::Relaxed), 1);
    assert!(errors["fake"].contains("terminated unexpectedly"));

    supervise_collector(
        &mut managed,
        &pipeline,
        &granted_permissions(),
        &mut errors,
        started + Duration::from_secs(5),
    )
    .expect("restart collector");
    assert_eq!(state.starts.load(Ordering::Relaxed), 2);
    assert!(!errors.contains_key("fake"));
}

#[test]
fn collector_start_failure_is_recorded_without_failing_the_daemon_start() {
    let state = Arc::new(FakeState::default());
    let mut managed = Some(Managed::new(FakeCollector::failing(Arc::clone(&state))));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();

    super::start_collector(&mut managed, &pipeline, &mut errors, Instant::now());

    assert_eq!(state.starts.load(Ordering::Relaxed), 1);
    assert_eq!(errors["fake"], "missing permission");
}

#[test]
fn permission_blocked_collector_waits_for_granted_transition() {
    let state = Arc::new(FakeState::default());
    let mut managed = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::from([Permission::Accessibility]),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();
    let started = Instant::now();
    super::start_collector(&mut managed, &pipeline, &mut errors, started);
    state.finish();
    wait_for_relay(&managed);

    supervise_collector(
        &mut managed,
        &pipeline,
        &denied_permissions(),
        &mut errors,
        started,
    )
    .expect("record permission failure");
    supervise_collector(
        &mut managed,
        &pipeline,
        &denied_permissions(),
        &mut errors,
        started + Duration::from_secs(60),
    )
    .expect("hold restart");
    assert_eq!(state.starts.load(Ordering::Relaxed), 1);

    supervise_collector(
        &mut managed,
        &pipeline,
        &granted_permissions(),
        &mut errors,
        started + Duration::from_secs(61),
    )
    .expect("permission recovery");
    assert_eq!(state.starts.load(Ordering::Relaxed), 2);
    assert!(!errors.contains_key("fake"));
}

#[derive(Default)]
struct FakeState {
    sender: Mutex<Option<SyncSender<RawEvent>>>,
    starts: AtomicUsize,
}

impl FakeState {
    fn finish(&self) {
        self.sender.lock().expect("fake sender").take();
    }
}

struct FakeCollector {
    state: Arc<FakeState>,
    required: BTreeSet<Permission>,
    fail_start: bool,
}

impl FakeCollector {
    fn new(state: Arc<FakeState>, required: BTreeSet<Permission>) -> Self {
        Self {
            state,
            required,
            fail_start: false,
        }
    }

    fn failing(state: Arc<FakeState>) -> Self {
        Self {
            state,
            required: BTreeSet::new(),
            fail_start: true,
        }
    }
}

impl ManagedCollector for FakeCollector {
    fn worker_name(&self) -> &str {
        "fake"
    }

    fn worker_permissions(&self) -> BTreeSet<Permission> {
        self.required.clone()
    }

    fn start_worker(&mut self, sender: SyncSender<RawEvent>) -> Result<(), String> {
        self.state.starts.fetch_add(1, Ordering::Relaxed);
        if self.fail_start {
            return Err("missing permission".to_owned());
        }
        *self.state.sender.lock().expect("fake sender") = Some(sender);
        Ok(())
    }

    fn stop_worker(&mut self) {
        self.state.finish();
    }
}

fn wait_for_relay(managed: &Option<Managed<FakeCollector>>) {
    for _ in 0..100 {
        if managed
            .as_ref()
            .and_then(|managed| managed.relay.as_ref())
            .is_some_and(super::Relay::is_finished)
        {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("collector relay did not finish");
}

fn granted_permissions() -> DaemonPermissions {
    DaemonPermissions {
        permissions_ok: true,
        accessibility: PermissionState::Granted,
        input_monitoring: PermissionState::Granted,
        automation: BTreeMap::new(),
    }
}

fn denied_permissions() -> DaemonPermissions {
    DaemonPermissions {
        permissions_ok: false,
        accessibility: PermissionState::Denied,
        ..granted_permissions()
    }
}
