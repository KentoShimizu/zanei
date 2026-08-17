use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration as StdDuration,
};

use tempfile::{NamedTempFile, TempDir};
use time::Duration;
use zanei_core::{
    config::Config,
    normalize::format_timestamp,
    store::{DaemonMode, DaemonState, StoreReader, StoreWriter},
};

use super::{
    CollectorSet, EXECUTABLE_REMOVED_MESSAGE, Pipeline, ensure_pipeline_running,
    executable_shutdown_requested, initialize_permission_dependent_runtime,
    merge_collector_failures, normalize_pause_request, shutdown_daemon,
};
use crate::daemon::{
    executable_guard::ExecutableGuard,
    permission_worker::{PermissionRequestPoll, PermissionRequestWorker},
};
use crate::permissions::PermissionRequestOutcome;

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
fn collector_failures_accumulate_across_daemon_instances() {
    let base = BTreeMap::from([("eventtap".to_owned(), 2), ("ax".to_owned(), u64::MAX)]);
    let current = BTreeMap::from([("eventtap".to_owned(), 3), ("ax".to_owned(), 1)]);

    assert_eq!(
        merge_collector_failures(&base, &current),
        BTreeMap::from([("ax".to_owned(), u64::MAX), ("eventtap".to_owned(), 5),])
    );
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
