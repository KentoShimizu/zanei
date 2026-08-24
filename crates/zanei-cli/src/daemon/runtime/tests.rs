use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration as StdDuration,
};

use tempfile::{NamedTempFile, TempDir};
use time::Duration;
use zanei_core::{
    config::{Config, ConfigWatcher},
    normalize::format_timestamp,
    store::{DaemonMode, DaemonState, StoreReader, StoreWriter},
};
use zanei_macos::chrome::{ChromeFailure, ChromeFailureState, ChromeQueryFailure};
use zanei_macos::permission::{PermissionError, PermissionStatus};

use super::{
    ActiveDaemon, CollectorSet, EXECUTABLE_REMOVED_MESSAGE, Pipeline, StoreOwner,
    configure_eventtap_start_gate, ensure_pipeline_running, executable_shutdown_requested,
    initialize_permission_dependent_runtime, merge_collector_failures, normalize_pause_request,
    queue_permission_expansion, service_permission_request_worker, shutdown_daemon,
};
use crate::daemon::{
    executable_guard::ExecutableGuard,
    permission_worker::{PermissionRequestPoll, PermissionRequestWorker},
    supervisor::{EventTapStartGate, chrome_failure_reason},
};
use crate::permissions::PermissionRequestOutcome;
use zanei_collector::Permission;

#[test]
fn initial_heartbeat_is_committed_before_permission_worker_starts() {
    let directory = TempDir::new().expect("temporary directory");
    let store_path = directory.path().join("store.sqlite");
    let worker_store_path = store_path.clone();
    let (probe_started, probe_started_rx) = mpsc::sync_channel(1);
    let (release_probe, release_probe_rx) = mpsc::sync_channel(1);
    let heartbeat_at = format_timestamp(time::OffsetDateTime::now_utc());
    let state = DaemonState {
        pid: Some(42),
        started_at: Some("2026-08-17T10:00:00.000Z".to_owned()),
        instance_id: Some("42@2026-08-17T10:00:00.000Z".to_owned()),
        mode: Some(DaemonMode::Launchd),
        heartbeat_at: Some(heartbeat_at),
        retention_hours: Some(48),
        ..DaemonState::default()
    };

    let writer = StoreWriter::open(&store_path).expect("store writer");
    let permission_worker = initialize_permission_dependent_runtime(&writer, &state, || {
        PermissionRequestWorker::start_with(move || {
            let status = StoreReader::open(&worker_store_path)
                .expect("worker store reader")
                .status()
                .expect("worker store status");
            assert!(status.running);
            assert_eq!(status.permissions, None);
            probe_started.send(()).expect("announce blocking probe");
            release_probe_rx.recv().expect("release blocking probe");
            Ok(PermissionRequestOutcome::Completed)
        })
    })
    .expect("daemon startup sequence");

    probe_started_rx.recv().expect("permission probe started");
    let status = StoreReader::open(&store_path)
        .expect("store reader")
        .status()
        .expect("store status");
    assert!(status.running);
    assert_eq!(
        status.instance_id.as_deref(),
        Some("42@2026-08-17T10:00:00.000Z")
    );
    assert_eq!(status.permissions, None);

    release_probe.send(()).expect("release permission probe");
    loop {
        match permission_worker.poll() {
            PermissionRequestPoll::Pending => thread::yield_now(),
            PermissionRequestPoll::Complete(result) => {
                assert!(matches!(result, Ok(PermissionRequestOutcome::Completed)));
                break;
            }
            PermissionRequestPoll::Stopped => panic!("worker must report its result"),
        }
    }
}

#[test]
fn initial_input_monitoring_grant_allows_immediate_eventtap_start() {
    let mut granted_gate = EventTapStartGate::open();
    let mut denied_gate = EventTapStartGate::open();
    let mut degraded = BTreeMap::new();

    configure_eventtap_start_gate(
        Some(Ok(PermissionStatus::Granted)),
        &mut granted_gate,
        &mut degraded,
    );
    configure_eventtap_start_gate(
        Some(Ok(PermissionStatus::Denied)),
        &mut denied_gate,
        &mut degraded,
    );

    assert!(granted_gate.allows_start());
    assert!(!denied_gate.allows_start());
}

#[test]
fn newly_required_chrome_automation_is_queued_after_filter_reload() {
    let previous = BTreeSet::from([Permission::Accessibility]);
    let automation = Permission::Automation {
        bundle_id: "com.google.Chrome".to_owned(),
    };
    let current = BTreeSet::from([Permission::Accessibility, automation.clone()]);
    let mut pending = None;

    queue_permission_expansion(&previous, &current, &mut pending);

    assert_eq!(pending, Some(current.clone()));
    queue_permission_expansion(&current, &previous, &mut pending);
    assert_eq!(pending, None, "permission removal does not open a prompt");
}

#[test]
fn permission_worker_error_and_disconnect_release_eventtap_start() {
    let mut failed_worker = Some(
        PermissionRequestWorker::start_with(|| {
            Err(PermissionError::AccessibilityRequestOptionsCreation)
        })
        .expect("permission worker"),
    );
    let mut degraded = BTreeMap::new();
    let mut terminal_starts = 0;
    wait_for_permission_worker(&mut failed_worker, &mut degraded, true, |start_now| {
        terminal_starts += usize::from(start_now);
    });
    assert!(degraded["permission_request"].contains("Accessibility"));

    let mut stopped_worker = Some(
        PermissionRequestWorker::start_with(|| {
            panic!("simulated permission worker disconnect");
        })
        .expect("permission worker"),
    );
    wait_for_permission_worker(&mut stopped_worker, &mut degraded, true, |start_now| {
        terminal_starts += usize::from(start_now);
    });
    assert_eq!(terminal_starts, 2);
    assert_eq!(
        degraded.get("permission_request").map(String::as_str),
        Some(super::PERMISSION_REQUEST_WORKER_STOPPED_MESSAGE),
    );
}

#[test]
fn pending_permission_gate_does_not_block_shutdown() {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let (release, release_rx) = mpsc::sync_channel(1);
    let (ready, ready_rx) = mpsc::sync_channel(1);
    let (stopped, stopped_rx) = mpsc::sync_channel(1);
    let runtime = thread::spawn(move || {
        let mut worker = Some(
            PermissionRequestWorker::start_with(move || {
                release_rx.recv().expect("release permission worker");
                Ok(PermissionRequestOutcome::Completed)
            })
            .expect("permission worker"),
        );
        let mut degraded = BTreeMap::new();
        ready.send(()).expect("runtime ready");
        while !worker_stop.load(Ordering::Relaxed) {
            service_permission_request_worker(&mut worker, &mut degraded, false, |_| {
                panic!("pending worker must not complete")
            });
            thread::yield_now();
        }
        stopped.send(()).expect("runtime stopped");
    });

    ready_rx.recv().expect("runtime started");
    stop.store(true, Ordering::Relaxed);
    stopped_rx
        .recv_timeout(StdDuration::from_secs(1))
        .expect("pending gate must observe shutdown");
    release.send(()).expect("release detached worker");
    runtime.join().expect("runtime thread");
}

fn wait_for_permission_worker(
    worker: &mut Option<PermissionRequestWorker>,
    degraded: &mut BTreeMap<String, String>,
    start_now: bool,
    mut on_complete: impl FnMut(bool),
) {
    for _ in 0..1_000 {
        service_permission_request_worker(worker, degraded, start_now, |start_now| {
            on_complete(start_now);
        });
        if worker.is_none() {
            return;
        }
        thread::sleep(StdDuration::from_millis(1));
    }
    panic!("permission worker did not complete");
}

#[test]
fn collector_failures_accumulate_across_daemon_instances() {
    let base = BTreeMap::from([("eventtap".to_owned(), 2), ("ax".to_owned(), u64::MAX)]);
    let first = BTreeMap::from([("eventtap".to_owned(), 3), ("ax".to_owned(), 1)]);
    let next = BTreeMap::from([("eventtap".to_owned(), 4), ("ax".to_owned(), 2)]);

    assert_eq!(
        merge_collector_failures(&base, &first),
        BTreeMap::from([("ax".to_owned(), u64::MAX), ("eventtap".to_owned(), 5),])
    );
    assert_eq!(
        merge_collector_failures(&base, &next),
        BTreeMap::from([("ax".to_owned(), u64::MAX), ("eventtap".to_owned(), 6),])
    );
}

#[test]
fn collector_degraded_state_is_persisted_by_the_heartbeat() {
    let directory = TempDir::new().expect("temporary directory");
    let store_path = directory.path().join("store.sqlite");
    let writer = Arc::new(Mutex::new(
        StoreWriter::open(&store_path).expect("store writer"),
    ));
    let reader = StoreReader::open(&store_path).expect("store reader");
    let config = Config::default();
    let mut config_watcher =
        ConfigWatcher::new(directory.path().join("config.toml")).expect("config watcher");
    let mut pipeline = Pipeline::store(&config, Arc::clone(&writer)).expect("pipeline");
    let mut collectors = CollectorSet::new(&config);
    let chrome_reason = chrome_failure_reason(ChromeFailureState::Unavailable(
        ChromeFailure::Query(ChromeQueryFailure::AppleEvent(-1712)),
    ))
    .expect("Chrome failure reason");
    collectors
        .start_errors
        .insert("chrome".to_owned(), chrome_reason.clone());
    let owner = StoreOwner::new(DaemonMode::Launchd, "2026-08-24T10:00:00.000Z".to_owned());
    let base_collector_failures = BTreeMap::new();
    let mut paused = false;
    let mut degraded = BTreeMap::new();

    ActiveDaemon {
        store_path: &store_path,
        config_watcher: &mut config_watcher,
        active_retention_hours: config.output.retention_hours,
        pending_retention_hours: None,
        writer: &writer,
        reader: &reader,
        pipeline: &pipeline,
        collectors: &mut collectors,
        owner: &owner,
        base_dropped: 0,
        base_collector_failures: &base_collector_failures,
        paused: &mut paused,
        intake_suspended: false,
        degraded: &mut degraded,
        last_status: reader.status().expect("initial store status"),
        last_permissions: None,
        initial_input_monitoring_status: None,
        permission_request_worker: None,
        pending_permission_request: None,
        executable_guard: ExecutableGuard::new(directory.path().join("zanei")),
    }
    .publish_heartbeat_with_permissions(None)
    .expect("publish heartbeat");
    pipeline.flush().expect("flush heartbeat");

    assert_eq!(
        reader
            .status()
            .expect("persisted heartbeat")
            .degraded
            .get("chrome"),
        Some(&chrome_reason)
    );
    pipeline.shutdown().expect("pipeline shutdown");
}

#[test]
fn expired_pause_is_cleared_atomically() {
    let store = NamedTempFile::new().expect("temporary store");
    let writer = Arc::new(Mutex::new(
        StoreWriter::open(store.path()).expect("store writer"),
    ));
    let expired = format_timestamp(time::OffsetDateTime::now_utc() - Duration::seconds(1));
    writer
        .lock()
        .expect("writer lock")
        .set_paused_until(Some(&expired))
        .expect("pause state");

    assert!(!normalize_pause_request(&writer, Some(&expired)).expect("pause request"));
    assert_eq!(
        StoreReader::open(store.path())
            .expect("store reader")
            .status()
            .expect("store status")
            .paused_until,
        None
    );
}

#[test]
fn pipeline_panic_makes_the_daemon_tick_fail() {
    let config = Config::default();
    let mut pipeline = Pipeline::panicking_store(&config).expect("pipeline");
    pipeline
        .heartbeat(DaemonState::default())
        .expect("trigger writer panic");
    for _ in 0..100 {
        if pipeline.is_finished() {
            break;
        }
        thread::sleep(StdDuration::from_millis(1));
    }
    assert!(pipeline.is_finished());

    let mut collectors = CollectorSet::new(&config);
    assert!(matches!(
        ensure_pipeline_running(&pipeline, &mut collectors),
        Err(super::DaemonError::ThreadTerminated { thread: "pipeline" })
    ));
    assert!(pipeline.shutdown().is_err());
}

#[test]
fn three_missing_executable_checks_trigger_shutdown_and_clear_heartbeat() {
    let directory = TempDir::new().expect("executable removal fixture");
    let executable = directory.path().join("zanei");
    fs::write(&executable, b"fake executable").expect("fake executable");
    let executable = fs::canonicalize(executable).expect("canonical fake executable");
    let mut guard = ExecutableGuard::new(executable.clone());
    fs::remove_file(&executable).expect("remove fake executable");

    let store_path = directory.path().join("store.sqlite");
    let writer = StoreWriter::open(&store_path).expect("store writer");
    writer
        .write_daemon_state(&DaemonState {
            pid: Some(42),
            started_at: Some("2026-08-17T10:00:00.000Z".to_owned()),
            instance_id: Some("42@2026-08-17T10:00:00.000Z".to_owned()),
            mode: Some(DaemonMode::Launchd),
            heartbeat_at: Some(format_timestamp(time::OffsetDateTime::now_utc())),
            retention_hours: Some(48),
            ..DaemonState::default()
        })
        .expect("initial heartbeat");
    let writer = Arc::new(Mutex::new(writer));
    let reader = StoreReader::open(&store_path).expect("store reader");
    let config = Config::from_toml("[capture]\nsources = []\n").expect("daemon config");
    let mut pipeline = Pipeline::store(&config, Arc::clone(&writer)).expect("pipeline");
    let mut collectors = CollectorSet::new(&config);
    let mut notifications = Vec::new();

    for _ in 0..2 {
        assert!(!executable_shutdown_requested(
            &mut guard,
            |path| fs::metadata(path).is_ok(),
            |message| notifications.push(message.to_owned()),
        ));
    }
    assert!(executable_shutdown_requested(
        &mut guard,
        |path| fs::metadata(path).is_ok(),
        |message| notifications.push(message.to_owned()),
    ));
    assert_eq!(notifications, [EXECUTABLE_REMOVED_MESSAGE]);

    shutdown_daemon(
        Ok(()),
        &writer,
        &reader,
        &mut collectors,
        &mut pipeline,
        0,
        &BTreeMap::new(),
    )
    .expect("daemon shutdown");

    let status = reader.status().expect("cleared daemon status");
    assert!(!status.running);
    assert_eq!(status.pid, None);
    assert_eq!(status.instance_id, None);
    assert_eq!(status.heartbeat_at, None);
}

#[test]
fn one_or_two_missing_executable_checks_do_not_trigger_shutdown() {
    let directory = TempDir::new().expect("missing executable fixture");
    let executable = directory.path().join("zanei");
    let mut guard = ExecutableGuard::new(executable.clone());
    let mut notifications = 0;
    let mut check = |exists| {
        executable_shutdown_requested(
            &mut guard,
            |path| {
                assert_eq!(path, executable);
                exists
            },
            |_| notifications += 1,
        )
    };

    assert!(!check(false));
    assert!(!check(false));
    assert!(!check(true));
    assert!(!check(false));
    assert!(!check(false));
    assert_eq!(notifications, 0);
}

#[test]
fn continuously_existing_executable_does_not_trigger_shutdown() {
    let executable = NamedTempFile::new().expect("existing executable fixture");
    let mut guard = ExecutableGuard::new(executable.path().to_owned());
    let mut notifications = 0;

    for _ in 0..10 {
        assert!(!executable_shutdown_requested(
            &mut guard,
            |path| fs::metadata(path).is_ok(),
            |_| notifications += 1,
        ));
    }
    assert_eq!(notifications, 0);
}
