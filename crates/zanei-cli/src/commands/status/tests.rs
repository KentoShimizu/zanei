use std::collections::BTreeMap;

use tempfile::TempDir;
use time::OffsetDateTime;
use zanei_core::{
    config::Config,
    normalize::format_timestamp,
    store::{DaemonMode, StoreError, StoreStatus, StoreWriter},
};

use super::{
    HeartbeatFreshness, StatusReport, StatusState, StoreWriteState, infer_store_write_state,
    inspect, store_error_report,
};
use crate::{
    daemon::{StoreOwner, StoreOwnership},
    paths::Paths,
};

#[test]
fn lock_owner_is_required_for_running_and_healthy_store_writes() {
    assert_eq!(StatusState::Stopped.exit_code(), super::EXIT_NO_DAEMON);
    assert_eq!(StatusState::Running.exit_code(), super::EXIT_SUCCESS);
    assert_eq!(
        infer_store_write_state(HeartbeatFreshness::Fresh, false, false, false),
        StoreWriteState::Stopped
    );
    assert_eq!(
        infer_store_write_state(HeartbeatFreshness::Fresh, true, true, false),
        StoreWriteState::Healthy
    );
}

#[test]
fn held_owner_with_an_unlinked_store_is_store_missing() {
    let directory = TempDir::new().expect("temporary directory");
    let store = directory.path().join("store.sqlite");
    StoreWriter::open(&store).expect("temporary store");
    let owner = StoreOwner::new(
        DaemonMode::Foreground,
        "2026-08-17T10:00:00.000Z".to_owned(),
    );
    let _ownership = StoreOwnership::acquire(&store, owner).expect("temporary store ownership");
    std::fs::remove_file(&store).expect("unlink temporary store");
    let config = Config::from_toml("[capture]\nsources = []\n").expect("test config");
    let paths = Paths {
        config: directory.path().join("config.toml"),
        store,
    };

    let probed_owner = StoreOwnership::probe(&paths.store).expect("probe temporary owner");
    let report =
        inspect(&paths, &config, probed_owner.as_ref()).expect("inspect missing owned store");

    assert_eq!(report.state, StatusState::StoreMissing);
    assert!(report.running);
    assert_eq!(report.events_captured, None);
    assert!(report.degraded.contains_key("store"));
}

#[test]
fn active_retention_requires_a_fresh_matching_heartbeat() {
    let directory = TempDir::new().expect("temporary directory");
    let paths = test_paths(&directory);
    let config = Config::from_toml("[capture]\nsources = []\n\n[output]\nretention_hours = 48\n")
        .expect("test config");
    let owner = StoreOwner::new(
        DaemonMode::Foreground,
        "2026-08-17T10:00:00.000Z".to_owned(),
    );
    let mut status = StoreStatus {
        instance_id: Some(owner.instance_id.clone()),
        heartbeat_at: Some("2020-01-01T00:00:00Z".to_owned()),
        retention_hours: Some(72),
        ..StoreStatus::default()
    };

    let stale = StatusReport::readable(&paths, &config, &status, Some(&owner), 0, None)
        .expect("stale status report");
    assert_eq!(stale.store.retention_hours, Some(48));

    status.heartbeat_at = Some(format_timestamp(OffsetDateTime::now_utc()));
    let fresh = StatusReport::readable(&paths, &config, &status, Some(&owner), 0, None)
        .expect("fresh status report");
    assert_eq!(fresh.store.retention_hours, Some(72));
}

#[test]
fn stopped_status_clears_current_degradation_but_retains_failure_counters() {
    let directory = TempDir::new().expect("temporary directory");
    let paths = test_paths(&directory);
    let config = Config::from_toml("[capture]\nsources = []\n").expect("test config");
    let status = StoreStatus {
        heartbeat_at: Some(format_timestamp(OffsetDateTime::now_utc())),
        degraded: BTreeMap::from([("chrome".to_owned(), "permission denied".to_owned())]),
        collector_failures: BTreeMap::from([("chrome".to_owned(), 2)]),
        ..StoreStatus::default()
    };

    let report = StatusReport::readable(&paths, &config, &status, None, 0, None)
        .expect("stopped status report");

    assert_eq!(report.state, StatusState::Stopped);
    assert!(report.degraded.is_empty());
    assert_eq!(
        report.collector_failures,
        Some(BTreeMap::from([("chrome".to_owned(), 2)]))
    );
}

#[test]
fn mismatched_owner_clears_prior_instance_degradation_but_retains_failure_counters() {
    let directory = TempDir::new().expect("temporary directory");
    let paths = test_paths(&directory);
    let config = Config::from_toml("[capture]\nsources = []\n").expect("test config");
    let owner = StoreOwner::new(
        DaemonMode::Foreground,
        "2026-08-17T10:00:00.000Z".to_owned(),
    );
    let status = StoreStatus {
        instance_id: Some("prior-instance".to_owned()),
        heartbeat_at: Some(format_timestamp(OffsetDateTime::now_utc())),
        degraded: BTreeMap::from([("chrome".to_owned(), "permission denied".to_owned())]),
        collector_failures: BTreeMap::from([("chrome".to_owned(), 2)]),
        ..StoreStatus::default()
    };

    let report = StatusReport::readable(&paths, &config, &status, Some(&owner), 0, None)
        .expect("mismatched owner status report");

    assert_eq!(report.state, StatusState::Running);
    assert!(report.degraded.is_empty());
    assert_eq!(
        report.collector_failures,
        Some(BTreeMap::from([("chrome".to_owned(), 2)]))
    );
}

#[test]
fn invalid_persisted_timestamp_is_store_corrupt_with_exit_one() {
    let directory = TempDir::new().expect("temporary directory");
    let paths = test_paths(&directory);
    StoreWriter::open(&paths.store).expect("temporary store");
    let config = Config::from_toml("[capture]\nsources = []\n").expect("test config");
    let error = StoreError::InvalidTimestamp {
        field: "last_event_ts",
        value: "invalid".to_owned(),
    };

    let report =
        store_error_report(&paths, &config, None, &error).expect("store corruption status report");

    assert_eq!(report.state, StatusState::StoreCorrupt);
    assert_eq!(report.state.exit_code(), 1);
}

fn test_paths(directory: &TempDir) -> Paths {
    Paths {
        config: directory.path().join("config.toml"),
        store: directory.path().join("store.sqlite"),
    }
}
