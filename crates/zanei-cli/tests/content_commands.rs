use std::fs;

use time::OffsetDateTime;
use zanei_core::config::Config;
use zanei_core::normalize::format_timestamp;
use zanei_core::schema::{
    App, BrowserMode, BrowserNavigateData, BrowserTransition, EmptyData, Event, EventData,
    Redaction, Window,
};
use zanei_core::store::{QueryFilter, StoreReader, StoreWriter};

mod support;

use support::Fixture;

const RETENTION_HOURS: u64 = 24 * 365 * 100;

#[test]
fn query_json_stays_an_array_and_unknown_warning_is_quiet_aware() {
    let fixture = Fixture::populated();
    let retired = retired_path(&fixture);
    let now = OffsetDateTime::now_utc();
    let unknown = app_event(
        "evt_01K00000000000000000003001",
        "UnknownRetired",
        now - time::Duration::minutes(2),
    );
    let known = app_event(
        "evt_01K00000000000000000003002",
        "KnownRetired",
        now - time::Duration::minutes(1),
    );
    StoreWriter::open(&retired)
        .and_then(|mut writer| writer.append_batch(&[unknown.clone(), known]))
        .expect("retired query fixtures");
    rusqlite::Connection::open(&retired)
        .expect("open retired fixture")
        .execute(
            "UPDATE events SET type = 'future.event' WHERE id = ?1",
            [&unknown.id],
        )
        .expect("write unknown fixture");

    let output = fixture
        .command()
        .args([
            "query",
            "--since",
            "1h",
            "--types",
            "future.*,app.launch",
            "--limit",
            "1",
            "--format",
            "json",
        ])
        .output()
        .expect("query output");
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("query JSON");
    assert!(value.is_array(), "query JSON must remain an array: {value}");
    assert_eq!(value.as_array().unwrap().len(), 1);
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "warning: skipped 1 events with unknown types\n"
    );

    let quiet = fixture
        .command()
        .args([
            "query",
            "--since",
            "1h",
            "--types",
            "future.*,app.launch",
            "--limit",
            "1",
            "--format",
            "jsonl",
            "--quiet",
        ])
        .output()
        .expect("quiet query output");
    assert!(quiet.status.success());
    assert!(quiet.stderr.is_empty());
    assert_eq!(String::from_utf8_lossy(&quiet.stdout).lines().count(), 1);

    let timeline = fixture
        .command()
        .args(["timeline", "--since", "1h", "--format", "json"])
        .output()
        .expect("timeline output");
    assert!(timeline.status.success());
    assert!(timeline.stderr.is_empty());
    let timeline: serde_json::Value =
        serde_json::from_slice(&timeline.stdout).expect("timeline JSON");
    assert_eq!(timeline["skipped_unknown_types"], 1);
}

#[test]
fn export_and_scoped_purge_apply_types_to_active_and_retired_stores() {
    let fixture = Fixture::populated();
    let retired = retired_path(&fixture);
    let now = OffsetDateTime::now_utc();
    let active_app = app_event(
        "evt_01K00000000000000000003003",
        "TargetApp",
        now - time::Duration::minutes(3),
    );
    let retired_app = app_event(
        "evt_01K00000000000000000003004",
        "TargetApp",
        now - time::Duration::minutes(2),
    );
    let retired_browser = browser_event(
        "evt_01K00000000000000000003005",
        now - time::Duration::minutes(1),
    );
    fixture
        .open_writer()
        .append(&active_app)
        .expect("active purge fixture");
    StoreWriter::open(&retired)
        .and_then(|mut writer| writer.append_batch(&[retired_app, retired_browser]))
        .expect("retired export fixtures");

    let export = fixture
        .command()
        .args([
            "export",
            "--since",
            "1h",
            "--types",
            "browser.*",
            "--format",
            "json",
        ])
        .output()
        .expect("JSON export");
    assert!(export.status.success());
    let events: Vec<Event> = serde_json::from_slice(&export.stdout).expect("export array");
    assert!(
        events
            .iter()
            .all(|event| event.event_type == "browser.navigate")
    );
    assert!(
        events
            .iter()
            .any(|event| event.app.name == "RetiredBrowser")
    );

    let snapshot = fixture.directory.path().join("apps.sqlite");
    fixture
        .command()
        .args([
            "export", "--since", "1h", "--types", "app.*", "--format", "sqlite",
        ])
        .arg("--out")
        .arg(&snapshot)
        .assert()
        .success();
    let copied = StoreReader::open(&snapshot)
        .and_then(|reader| {
            reader.query(
                &QueryFilter {
                    types: vec!["*".to_owned()],
                    ..QueryFilter::default()
                },
                RETENTION_HOURS,
            )
        })
        .expect("query SQLite export");
    assert!(!copied.events.is_empty());
    assert!(
        copied
            .events
            .iter()
            .all(|event| event.event_type.starts_with("app."))
    );
    assert!(
        copied
            .events
            .iter()
            .filter(|event| event.app.name == "TargetApp")
            .count()
            >= 2
    );

    fixture
        .command()
        .args([
            "purge",
            "--types",
            "app.*",
            "--bundle-id",
            "dev.example.TargetApp",
            "--quiet",
        ])
        .assert()
        .success();
    assert!(retired.exists(), "scoped purge must keep the retired file");
    let merged = fixture
        .open_reader()
        .query(
            &QueryFilter {
                types: vec!["app.*".to_owned()],
                bundle_id: Some("dev.example.TargetApp".to_owned()),
                ..QueryFilter::default()
            },
            RETENTION_HOURS,
        )
        .expect("query purged stores");
    assert!(merged.events.is_empty());
    let remaining_apps = fixture
        .open_reader()
        .query(
            &QueryFilter {
                types: vec!["app.*".to_owned()],
                ..QueryFilter::default()
            },
            RETENTION_HOURS,
        )
        .expect("query non-target app events");
    assert!(!remaining_apps.events.is_empty());
    let browsers = fixture
        .open_reader()
        .query(
            &QueryFilter {
                types: vec!["browser.*".to_owned()],
                ..QueryFilter::default()
            },
            RETENTION_HOURS,
        )
        .expect("query retained browser events");
    assert!(
        browsers
            .events
            .iter()
            .any(|event| event.app.name == "RetiredBrowser")
    );
}

#[test]
fn purge_constraints_and_failed_retired_all_are_fail_closed() {
    let fixture = Fixture::populated();
    fixture
        .command()
        .args(["purge", "--app", "Example"])
        .assert()
        .failure()
        .code(2);
    for command in ["purge", "export"] {
        fixture
            .command()
            .args([command, "--types", "app.*.*"])
            .assert()
            .failure()
            .code(2);
    }

    let damaged = retired_path(&fixture);
    fs::write(&damaged, vec![0xA5; 4096]).expect("unexpected-format retired fixture");
    let output = fixture
        .command()
        .args(["purge", "--all", "--quiet"])
        .output()
        .expect("purge unexpected-format output");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("expected plaintext, found sqlcipher")
    );
    assert!(
        damaged.exists(),
        "failed purge must not delete the retired file"
    );
}

#[test]
fn config_init_writes_eighteen_keys_and_content_snapshot_set_is_noninteractive_in_a1() {
    let fixture = Fixture::uninitialized();
    fs::remove_file(&fixture.config).expect("remove fixture config");
    fixture
        .command()
        .args(["config", "init"])
        .assert()
        .success();

    let source = fs::read_to_string(&fixture.config).expect("initialized config");
    let value: toml::Value = toml::from_str(&source).expect("parse initialized config");
    assert_eq!(leaf_count(&value), 18, "{source}");
    assert_eq!(
        source.lines().filter(|line| line.starts_with("# ")).count(),
        18
    );
    assert_eq!(
        Config::load(&fixture.config).expect("load initialized config"),
        Config::default()
    );

    fixture
        .command()
        .args([
            "config",
            "set",
            "capture.content_snapshot",
            "true",
            "--quiet",
        ])
        .assert()
        .success();
    assert!(
        Config::load(&fixture.config)
            .expect("load edited config")
            .capture
            .content_snapshot
    );
}

fn retired_path(fixture: &Fixture) -> std::path::PathBuf {
    fixture
        .directory
        .path()
        .join("store.sqlite.plaintext-20260823T000000Z")
}

fn app_event(id: &str, app_name: &str, at: OffsetDateTime) -> Event {
    event(
        id,
        app_name,
        at,
        EventData::AppLaunch(EmptyData::default()),
        None,
    )
}

fn browser_event(id: &str, at: OffsetDateTime) -> Event {
    event(
        id,
        "RetiredBrowser",
        at,
        EventData::BrowserNavigate(BrowserNavigateData {
            url: "https://example.com".to_owned().into(),
            tab_title: Some("Retired".to_owned()),
            mode: BrowserMode::Normal,
            transition: Some(BrowserTransition::Navigate),
        }),
        Some(Window {
            title: Some("Retired".to_owned()),
            id: Some(9),
        }),
    )
}

fn event(
    id: &str,
    app_name: &str,
    at: OffsetDateTime,
    data: EventData,
    window: Option<Window>,
) -> Event {
    Event {
        version: 1,
        id: id.to_owned(),
        ts: format_timestamp(at),
        mono_ns: 1,
        source: "test.content_commands".to_owned(),
        event_type: data.event_type().to_owned(),
        app: App {
            name: app_name.to_owned(),
            bundle_id: Some(format!("dev.example.{app_name}")),
            pid: Some(7),
        },
        window,
        element: None,
        data,
        redaction: Redaction {
            applied: false,
            rules: Vec::new(),
        },
    }
}

fn leaf_count(value: &toml::Value) -> usize {
    match value {
        toml::Value::Table(table) => table.values().map(leaf_count).sum(),
        _ => 1,
    }
}
