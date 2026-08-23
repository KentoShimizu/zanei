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
use zanei_macos::permission::PermissionStatus;

use super::{
    CollectorSet, Managed, ManagedCollector, SourceGate, chrome_tracking_required, start_collector,
    start_collector_if_allowed, supervise_collector,
};
use crate::{
    daemon::{
        permission_worker::PermissionRequestWorker,
        runtime::{configure_eventtap_start_gate, service_permission_request_worker},
        supervisor::{CollectorKind, EventTapStartGate, START_ORDER, STOP_ORDER},
    },
    permissions::PermissionRequestOutcome,
};

#[test]
fn source_gate_maps_support_collector_events_to_configured_families() {
    let gate = SourceGate::new(
        &[
            CaptureSource::Window,
            CaptureSource::Input,
            CaptureSource::Browser,
        ],
        false,
    );

    assert!(!gate.allows_type("app.activate"));
    assert!(gate.allows_type("window.focus"));
    assert!(!gate.allows_type("ui.focus"));
    assert!(gate.allows_type("input.key"));
    assert!(gate.allows_type("clipboard.copy"));
    assert!(gate.allows_type("browser.navigate"));
    assert!(!gate.allows_type("content.snapshot"));
    assert!(!gate.allows_type("future.event"));
}

#[test]
fn content_snapshot_gate_is_independent_from_capture_sources() {
    let gate = SourceGate::new(&[], true);

    assert!(gate.allows_type("content.snapshot"));
    assert!(!gate.allows_type("window.focus"));
}

#[test]
fn secure_input_monitor_is_created_only_for_an_enabled_consumer() {
    let mut config = zanei_core::config::Config::default();
    config.capture.sources.clear();
    config.capture.text_content = false;
    config.capture.content_snapshot = false;
    assert!(CollectorSet::new(&config)._secure_input_monitor.is_none());

    config.capture.text_content = true;
    assert!(
        CollectorSet::new(&config)._secure_input_monitor.is_none(),
        "text opt-in without the input source has no Secure Input consumer"
    );

    config.capture.sources = vec![CaptureSource::Input];
    assert!(CollectorSet::new(&config)._secure_input_monitor.is_some());

    config.capture.sources.clear();
    config.capture.text_content = false;
    config.capture.content_snapshot = true;
    assert!(CollectorSet::new(&config)._secure_input_monitor.is_some());
}

#[test]
fn content_snapshot_collector_and_internal_dependencies_exist_only_when_opted_in() {
    let mut config = zanei_core::config::Config::default();
    config.capture.sources.clear();
    let disabled = CollectorSet::new(&config);
    assert!(disabled.content_snapshot.is_none());
    assert!(disabled.ax.is_none());
    assert!(disabled.workspace.is_none());

    config.capture.content_snapshot = true;
    let enabled = CollectorSet::new(&config);
    assert!(enabled.content_snapshot.is_some());
    assert!(enabled.ax.is_some(), "AX produces snapshot triggers");
    assert!(
        enabled.workspace.is_some(),
        "workspace cleans process state"
    );
    assert!(
        !enabled
            .content_snapshot
            .as_ref()
            .expect("content collector")
            .collector
            .is_running(),
        "construction alone does not start zanei-content"
    );
}

#[test]
fn content_snapshot_health_uses_the_stable_component_name() {
    let mut config = zanei_core::config::Config::default();
    config.capture.content_snapshot = true;
    let mut collectors = CollectorSet::new(&config);
    collectors.start_errors.insert(
        "content_snapshot".to_owned(),
        "simulated content worker failure".to_owned(),
    );

    let health = collectors.health();
    assert_eq!(
        health.degraded.get("content_snapshot").map(String::as_str),
        Some("simulated content worker failure")
    );
}

#[test]
fn content_worker_lifecycle_order_keeps_trigger_production_outside_its_lifetime() {
    assert_eq!(
        START_ORDER,
        [
            CollectorKind::ContentSnapshot,
            CollectorKind::Ax,
            CollectorKind::Chrome,
            CollectorKind::Workspace,
            CollectorKind::EventTap,
        ]
    );
    let ax_stop = STOP_ORDER
        .iter()
        .position(|collector| *collector == CollectorKind::Ax)
        .expect("AX stop position");
    let content_stop = STOP_ORDER
        .iter()
        .position(|collector| *collector == CollectorKind::ContentSnapshot)
        .expect("content stop position");
    assert!(
        ax_stop < content_stop,
        "AX producer stops before content discard"
    );
}

#[test]
fn source_gate_drops_every_family_when_capture_is_disabled() {
    let gate = SourceGate::new(&[], false);

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
    let gate = SourceGate::new(&[CaptureSource::Ui], false);

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
    assert!(collectors.ax.is_some());
    assert!(collectors.eventtap.is_some());
    assert_eq!(
        collectors.required_permissions(),
        [Permission::Accessibility, Permission::InputMonitoring]
            .into_iter()
            .collect()
    );

    config.capture.text_content = true;
    let collectors = CollectorSet::new(&config);
    assert!(collectors.workspace.is_some());
    assert!(collectors.chrome.is_some());
    assert_eq!(
        collectors.required_permissions(),
        [
            Permission::Accessibility,
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
        [
            Permission::Accessibility,
            Permission::Automation {
                bundle_id: "com.google.Chrome".to_owned(),
            },
        ]
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
            BTreeSet::from([
                Permission::Accessibility,
                Permission::InputMonitoring,
                automation.clone(),
            ]),
        ),
    ] {
        config.capture.sources = vec![source];

        assert_eq!(CollectorSet::new(&config).required_permissions(), expected);
    }
}

#[test]
fn content_snapshot_permission_matrix_honors_global_and_scoped_app_rules() {
    let accessibility = Permission::Accessibility;
    let automation = Permission::Automation {
        bundle_id: "com.google.Chrome".to_owned(),
    };
    let mut config = zanei_core::config::Config::default();
    config.capture.sources.clear();
    config.capture.content_snapshot = true;

    assert_eq!(
        CollectorSet::new(&config).required_permissions(),
        BTreeSet::from([accessibility.clone(), automation.clone()])
    );

    config
        .filter
        .content_snapshot
        .exclude_apps
        .push("com.google.Chrome".to_owned());
    assert_eq!(
        CollectorSet::new(&config).required_permissions(),
        BTreeSet::from([accessibility.clone()])
    );

    config.filter.content_snapshot.exclude_apps.clear();
    config
        .filter
        .exclude_apps
        .push("com.google.Chrome".to_owned());
    assert_eq!(
        CollectorSet::new(&config).required_permissions(),
        BTreeSet::from([accessibility])
    );
}

#[test]
fn filter_reload_reconciles_chrome_collector_topology_without_applescript() {
    let mut config = zanei_core::config::Config::default();
    config.capture.sources.clear();
    config.capture.content_snapshot = true;
    config
        .filter
        .content_snapshot
        .exclude_apps
        .push("com.google.Chrome".to_owned());
    let mut collectors = CollectorSet::new(&config);
    assert!(!chrome_tracking_required(&config.capture, &config.filter));
    assert!(collectors.chrome.is_none());

    let mut admitted = config.filter.clone();
    admitted.content_snapshot.exclude_apps.clear();
    assert!(chrome_tracking_required(&config.capture, &admitted));
    collectors.replace_filter(admitted);
    assert!(
        collectors.chrome.is_some(),
        "reload creates the managed collector"
    );

    collectors.replace_filter(config.filter.clone());
    assert!(
        collectors.chrome.is_none(),
        "reload stops and drops the collector"
    );
}

mod supervisor_tests;

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

#[allow(clippy::too_many_arguments)]
fn complete_permission_worker(
    worker: &mut Option<PermissionRequestWorker>,
    gate: &mut EventTapStartGate,
    eventtap: &mut Option<Managed<FakeCollector>>,
    pipeline: &SyncSender<RawEvent>,
    errors: &mut BTreeMap<String, String>,
    degraded: &mut BTreeMap<String, String>,
    start_now: bool,
) {
    for _ in 0..1_000 {
        service_permission_request_worker(worker, degraded, start_now, |start_now| {
            gate.allow();
            if start_now {
                start_collector_if_allowed(eventtap, pipeline, errors, Instant::now(), *gate);
            }
        });
        if worker.is_none() {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("permission worker did not complete");
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
