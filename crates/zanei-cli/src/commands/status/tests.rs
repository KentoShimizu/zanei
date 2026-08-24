use std::collections::BTreeMap;

use tempfile::TempDir;
use time::OffsetDateTime;
use zanei_core::{
    config::Config,
    normalize::format_timestamp,
    store::{DaemonMode, StoreError, StoreFormat, StoreStatus, StoreWriter},
};

use super::{
    HeartbeatFreshness, StatusReport, StatusState, StoreWriteState, infer_store_write_state,
    inspect, render::render_human, store_error_report,
};
use crate::{
    daemon::{StoreOwner, StoreOwnership},
    paths::Paths,
};

const CONTROL_TEXT_COMPONENT: &str = "chrome\nforged\r\u{1b}[2J";
const CONTROL_TEXT_REASON: &str = "failed\r\n\u{1b}[31m";

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

    let stale = StatusReport::readable(
        &paths,
        &config,
        &status,
        Some(&owner),
        super::StoreInspection {
            size_bytes: 0,
            oldest_event_ts: None,
            format: StoreFormat::Encrypted,
            retired: super::RetiredReport::default(),
        },
    )
    .expect("stale status report");
    assert_eq!(stale.store.retention_hours, Some(48));

    status.heartbeat_at = Some(format_timestamp(OffsetDateTime::now_utc()));
    let fresh = StatusReport::readable(
        &paths,
        &config,
        &status,
        Some(&owner),
        super::StoreInspection {
            size_bytes: 0,
            oldest_event_ts: None,
            format: StoreFormat::Encrypted,
            retired: super::RetiredReport::default(),
        },
    )
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

    let report = StatusReport::readable(
        &paths,
        &config,
        &status,
        None,
        super::StoreInspection {
            size_bytes: 0,
            oldest_event_ts: None,
            format: StoreFormat::Encrypted,
            retired: super::RetiredReport::default(),
        },
    )
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

    let report = StatusReport::readable(
        &paths,
        &config,
        &status,
        Some(&owner),
        super::StoreInspection {
            size_bytes: 0,
            oldest_event_ts: None,
            format: StoreFormat::Encrypted,
            retired: super::RetiredReport::default(),
        },
    )
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

#[test]
fn healthy_json_and_human_output_snapshot() {
    let report = readable_output_report(output_store_status());
    assert_output_snapshot(
        &report,
        r#"{
  "state": "running",
  "running": true,
  "paused": false,
  "since": "2026-08-24T10:00:00Z",
  "instance": "current-instance",
  "mode": "foreground",
  "uptime_s": 120,
  "events_captured": 42,
  "events_dropped": 0,
  "collector_failures": {},
  "last_event_ts": "2026-08-24T10:01:57Z",
  "heartbeat_freshness": "fresh",
  "heartbeat_age_s": 2,
  "last_event_age_s": 3,
  "store_write_state": "healthy",
  "degraded": {},
  "store": {
    "path": "/tmp/zanei-test/store.sqlite",
    "size_bytes": 4096,
    "retention_hours": 72,
    "oldest_event_ts": "2026-08-23T10:00:00Z",
    "encryption": "sqlcipher",
    "retired_plaintext": []
  },
  "capture": {
    "sources": [],
    "text_content": false,
    "content_snapshot": false
  },
  "permissions_ok": true
}"#,
        r#"STATE             running
PAUSED            false
SINCE             2026-08-24T10:00:00Z
INSTANCE          current-instance
MODE              foreground
EVENTS CAPTURED   42
EVENTS DROPPED    0
LAST EVENT        2026-08-24T10:01:57Z
HEARTBEAT         fresh (2s old)
STORE WRITES      healthy
STORE             /tmp/zanei-test/store.sqlite (encrypted)
TEXT CONTENT      off (opt-in: zanei config set capture.text_content true)
CONTENT SNAPSHOT  off (opt-in: zanei config set capture.content_snapshot true)
PERMISSIONS OK    true
COLLECTOR FAILURES none
DEGRADED          false
"#,
    );
}

#[test]
fn degraded_json_and_human_output_snapshot() {
    let mut status = output_store_status();
    status.collector_failures = BTreeMap::from([("chrome".to_owned(), 3)]);
    status.degraded = BTreeMap::from([(
        "chrome".to_owned(),
        "state=unavailable phase=query kind=apple_event code=-1712".to_owned(),
    )]);
    let report = readable_output_report(status);
    assert_output_snapshot(
        &report,
        r#"{
  "state": "running",
  "running": true,
  "paused": false,
  "since": "2026-08-24T10:00:00Z",
  "instance": "current-instance",
  "mode": "foreground",
  "uptime_s": 120,
  "events_captured": 42,
  "events_dropped": 0,
  "collector_failures": {
    "chrome": 3
  },
  "last_event_ts": "2026-08-24T10:01:57Z",
  "heartbeat_freshness": "fresh",
  "heartbeat_age_s": 2,
  "last_event_age_s": 3,
  "store_write_state": "healthy",
  "degraded": {
    "chrome": "state=unavailable phase=query kind=apple_event code=-1712"
  },
  "store": {
    "path": "/tmp/zanei-test/store.sqlite",
    "size_bytes": 4096,
    "retention_hours": 72,
    "oldest_event_ts": "2026-08-23T10:00:00Z",
    "encryption": "sqlcipher",
    "retired_plaintext": []
  },
  "capture": {
    "sources": [],
    "text_content": false,
    "content_snapshot": false
  },
  "permissions_ok": true
}"#,
        r#"STATE             running
PAUSED            false
SINCE             2026-08-24T10:00:00Z
INSTANCE          current-instance
MODE              foreground
EVENTS CAPTURED   42
EVENTS DROPPED    0
LAST EVENT        2026-08-24T10:01:57Z
HEARTBEAT         fresh (2s old)
STORE WRITES      healthy
STORE             /tmp/zanei-test/store.sqlite (encrypted)
TEXT CONTENT      off (opt-in: zanei config set capture.text_content true)
CONTENT SNAPSHOT  off (opt-in: zanei config set capture.content_snapshot true)
PERMISSIONS OK    true
COLLECTOR FAILURES
  chrome: 3
DEGRADED          true
  chrome: state=unavailable phase=query kind=apple_event code=-1712
"#,
    );
}

#[test]
fn control_text_is_escaped_in_human_output_and_preserved_in_json() {
    let mut status = output_store_status();
    status.collector_failures = BTreeMap::from([(CONTROL_TEXT_COMPONENT.to_owned(), 3)]);
    status.degraded = BTreeMap::from([(
        CONTROL_TEXT_COMPONENT.to_owned(),
        CONTROL_TEXT_REASON.to_owned(),
    )]);
    let report = readable_output_report(status);

    let human = render_human(&report);
    assert!(human.contains(
        "COLLECTOR FAILURES\n  chrome\\nforged\\r\\u{1b}[2J: 3\n\
         DEGRADED          true\n  chrome\\nforged\\r\\u{1b}[2J: failed\\r\\n\\u{1b}[31m\n"
    ));
    let json = serde_json::to_value(&report).expect("serialize status report");
    assert_eq!(json["collector_failures"][CONTROL_TEXT_COMPONENT], 3);
    assert_eq!(
        json["degraded"][CONTROL_TEXT_COMPONENT],
        CONTROL_TEXT_REASON
    );
}

#[test]
fn stale_json_and_human_output_snapshot() {
    let mut report = readable_output_report(output_store_status());
    report.heartbeat_freshness = Some(HeartbeatFreshness::Stale);
    report.heartbeat_age_s = Some(61);
    report.store_write_state = Some(StoreWriteState::HeartbeatStale);
    assert_output_snapshot(
        &report,
        r#"{
  "state": "running",
  "running": true,
  "paused": false,
  "since": "2026-08-24T10:00:00Z",
  "instance": "current-instance",
  "mode": "foreground",
  "uptime_s": 120,
  "events_captured": 42,
  "events_dropped": 0,
  "collector_failures": {},
  "last_event_ts": "2026-08-24T10:01:57Z",
  "heartbeat_freshness": "stale",
  "heartbeat_age_s": 61,
  "last_event_age_s": 3,
  "store_write_state": "heartbeat_stale",
  "degraded": {},
  "store": {
    "path": "/tmp/zanei-test/store.sqlite",
    "size_bytes": 4096,
    "retention_hours": 72,
    "oldest_event_ts": "2026-08-23T10:00:00Z",
    "encryption": "sqlcipher",
    "retired_plaintext": []
  },
  "capture": {
    "sources": [],
    "text_content": false,
    "content_snapshot": false
  },
  "permissions_ok": true
}"#,
        r#"STATE             running
PAUSED            false
SINCE             2026-08-24T10:00:00Z
INSTANCE          current-instance
MODE              foreground
EVENTS CAPTURED   42
EVENTS DROPPED    0
LAST EVENT        2026-08-24T10:01:57Z
HEARTBEAT         stale (61s old)
STORE WRITES      heartbeat_stale
STORE             /tmp/zanei-test/store.sqlite (encrypted)
TEXT CONTENT      off (opt-in: zanei config set capture.text_content true)
CONTENT SNAPSHOT  off (opt-in: zanei config set capture.content_snapshot true)
PERMISSIONS OK    true
COLLECTOR FAILURES none
DEGRADED          false
"#,
    );
}

#[test]
fn owner_mismatch_json_and_human_output_snapshot() {
    let mut status = output_store_status();
    status.instance_id = Some("prior-instance".to_owned());
    status.collector_failures = BTreeMap::from([("chrome".to_owned(), 3)]);
    let report = readable_output_report(status);
    assert_output_snapshot(
        &report,
        r#"{
  "state": "running",
  "running": true,
  "paused": false,
  "since": "2026-08-24T10:00:00Z",
  "instance": "current-instance",
  "mode": "foreground",
  "uptime_s": 120,
  "events_captured": 42,
  "events_dropped": 0,
  "collector_failures": {
    "chrome": 3
  },
  "last_event_ts": "2026-08-24T10:01:57Z",
  "heartbeat_freshness": "fresh",
  "heartbeat_age_s": 2,
  "last_event_age_s": 3,
  "store_write_state": "suspected_unavailable",
  "degraded": {},
  "store": {
    "path": "/tmp/zanei-test/store.sqlite",
    "size_bytes": 4096,
    "retention_hours": 72,
    "oldest_event_ts": "2026-08-23T10:00:00Z",
    "encryption": "sqlcipher",
    "retired_plaintext": []
  },
  "capture": {
    "sources": [],
    "text_content": false,
    "content_snapshot": false
  },
  "permissions_ok": true
}"#,
        r#"STATE             running
PAUSED            false
SINCE             2026-08-24T10:00:00Z
INSTANCE          current-instance
MODE              foreground
EVENTS CAPTURED   42
EVENTS DROPPED    0
LAST EVENT        2026-08-24T10:01:57Z
HEARTBEAT         fresh (2s old)
STORE WRITES      suspected_unavailable
STORE             /tmp/zanei-test/store.sqlite (encrypted)
TEXT CONTENT      off (opt-in: zanei config set capture.text_content true)
CONTENT SNAPSHOT  off (opt-in: zanei config set capture.content_snapshot true)
PERMISSIONS OK    true
COLLECTOR FAILURES
  chrome: 3
DEGRADED          false
"#,
    );
}

#[test]
fn store_failure_json_and_human_output_snapshot() {
    let report = store_failure_output_report();
    let json = r#"{
  "state": "store_unavailable",
  "running": false,
  "paused": null,
  "since": null,
  "instance": null,
  "mode": null,
  "uptime_s": null,
  "events_captured": null,
  "events_dropped": null,
  "collector_failures": null,
  "last_event_ts": null,
  "heartbeat_freshness": null,
  "heartbeat_age_s": null,
  "last_event_age_s": null,
  "store_write_state": null,
  "degraded": {
    "store": "failed to read database: database is unavailable"
  },
  "store": {
    "path": "/tmp/zanei-test/store.sqlite",
    "size_bytes": 0,
    "retention_hours": null,
    "oldest_event_ts": null,
    "encryption": null,
    "retired_plaintext": []
  },
  "capture": {
    "sources": [],
    "text_content": false,
    "content_snapshot": false
  },
  "permissions_ok": true
}"#
    .replace("/tmp/zanei-test/store.sqlite", &report.store.path);
    let human = r#"STATE             store_unavailable
PAUSED            -
SINCE             -
INSTANCE          -
MODE              -
EVENTS CAPTURED   -
EVENTS DROPPED    -
LAST EVENT        -
HEARTBEAT         -
STORE WRITES      -
STORE             /tmp/zanei-test/store.sqlite
TEXT CONTENT      off (opt-in: zanei config set capture.text_content true)
CONTENT SNAPSHOT  off (opt-in: zanei config set capture.content_snapshot true)
PERMISSIONS OK    true
COLLECTOR FAILURES -
DEGRADED          true
  store: failed to read database: database is unavailable
"#
    .replace("/tmp/zanei-test/store.sqlite", &report.store.path);
    assert_output_snapshot(&report, &json, &human);
}

fn test_paths(directory: &TempDir) -> Paths {
    Paths {
        config: directory.path().join("config.toml"),
        store: directory.path().join("store.sqlite"),
    }
}

fn output_store_status() -> StoreStatus {
    StoreStatus {
        instance_id: Some("current-instance".to_owned()),
        heartbeat_at: Some(format_timestamp(OffsetDateTime::now_utc())),
        retention_hours: Some(72),
        events_captured: 42,
        last_event_ts: Some("2026-08-24T10:01:57Z".to_owned()),
        ..StoreStatus::default()
    }
}

fn readable_output_report(status: StoreStatus) -> StatusReport {
    let paths = Paths {
        config: "/tmp/zanei-test/config.toml".into(),
        store: "/tmp/zanei-test/store.sqlite".into(),
    };
    let config = Config::from_toml("[capture]\nsources = []\n\n[output]\nretention_hours = 72\n")
        .expect("snapshot config");
    let owner = StoreOwner {
        pid: 42,
        instance_id: "current-instance".to_owned(),
        mode: DaemonMode::Foreground,
        started_at: "2026-08-24T10:00:00Z".to_owned(),
    };
    let mut report = StatusReport::readable(
        &paths,
        &config,
        &status,
        Some(&owner),
        super::StoreInspection {
            size_bytes: 4096,
            oldest_event_ts: Some("2026-08-23T10:00:00Z".to_owned()),
            format: StoreFormat::Encrypted,
            retired: super::RetiredReport::default(),
        },
    )
    .expect("readable snapshot report");
    report.uptime_s = Some(120);
    report.heartbeat_age_s = Some(2);
    report.last_event_age_s = Some(3);
    report
}

fn store_failure_output_report() -> StatusReport {
    let directory = TempDir::new().expect("temporary directory");
    let paths = test_paths(&directory);
    std::fs::write(&paths.store, []).expect("empty store fixture");
    let config = Config::from_toml("[capture]\nsources = []\n").expect("snapshot config");
    let error = StoreError::io(
        "read database",
        std::io::Error::other("database is unavailable"),
    );
    store_error_report(&paths, &config, None, &error).expect("store failure snapshot report")
}

fn assert_output_snapshot(report: &StatusReport, json: &str, human: &str) {
    assert_eq!(
        serde_json::to_string_pretty(report).expect("serialize status report"),
        json
    );
    assert_eq!(render_human(report), human)
}
