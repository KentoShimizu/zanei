use std::{
    collections::BTreeMap,
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
    CollectorSet, Pipeline, ensure_pipeline_running, initialize_permission_dependent_runtime,
    merge_collector_failures, normalize_pause_request,
};
use crate::daemon::permission_worker::{PermissionRequestPoll, PermissionRequestWorker};
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
