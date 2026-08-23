use std::fs;

use clap::Parser;
use time::OffsetDateTime;
use zanei_cli::Cli;
use zanei_collector::{AppDirectory, AppDirectoryError, AppInfo};
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
fn app_filter_resolution_normalizes_rejects_typos_and_removes_uninstalled_values() {
    let fixture = Fixture::uninitialized();
    let directory = FakeAppDirectory::terminal();

    assert_eq!(
        run_injected(&fixture, &directory, ["apps", "Terminal", "--json"])
            .expect("apps JSON through injected directory"),
        0
    );

    assert_eq!(
        run_injected(
            &fixture,
            &directory,
            ["filter", "content-snapshot", "only-app", "add", "Terminal"]
        )
        .expect("resolve display name"),
        0
    );
    let config = Config::load(&fixture.config).expect("resolved config");
    assert_eq!(
        config.filter.content_snapshot.include_only_apps,
        ["com.apple.Terminal"]
    );

    assert_eq!(
        run_injected(
            &fixture,
            &directory,
            [
                "filter",
                "content-snapshot",
                "only-app",
                "add",
                "com.apple.Terminal",
            ]
        )
        .expect("resolve bundle ID"),
        0
    );
    let before_typo = fs::read(&fixture.config).expect("config before typo");
    let typo = run_injected(
        &fixture,
        &directory,
        ["filter", "exclude-app", "add", "Termial"],
    )
    .expect_err("typo must fail");
    assert_eq!(typo.exit_code(), 2);
    assert!(typo.to_string().contains("Did you mean Terminal"));
    assert_eq!(
        fs::read(&fixture.config).expect("config after typo"),
        before_typo
    );

    run_injected(
        &fixture,
        &directory,
        [
            "filter",
            "text-content",
            "exclude-app",
            "add",
            "FutureApp",
            "--unverified",
        ],
    )
    .expect("save unverified value");
    assert!(
        Config::load(&fixture.config)
            .expect("unverified config")
            .filter
            .text_content
            .exclude_apps
            .iter()
            .any(|value| value == "FutureApp")
    );
    run_injected(
        &fixture,
        &directory,
        [
            "filter",
            "text-content",
            "exclude-app",
            "remove",
            "FutureApp",
        ],
    )
    .expect("remove uninstalled value");
    assert!(
        !Config::load(&fixture.config)
            .expect("removed config")
            .filter
            .text_content
            .exclude_apps
            .iter()
            .any(|value| value == "FutureApp")
    );
}

#[test]
fn apps_json_includes_recent_fixture_and_filter_show_renders_three_scopes() {
    let fixture = Fixture::populated();
    let output = fixture
        .command()
        .args(["apps", "FixtureApp", "--json"])
        .output()
        .expect("apps JSON");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("apps report");
    let apps = report["apps"].as_array().expect("apps array");
    assert_eq!(apps.len(), 1, "{report}");
    assert_eq!(apps[0]["name"], "FixtureApp");
    assert!(apps[0]["last_used"].is_string());
    assert!(report.get("recent_unavailable").is_some());

    let mut config = Config::default();
    config.filter.include_only_apps = vec!["dev.example.Global".to_owned()];
    config.filter.text_content.include_only_websites = vec!["github.com".to_owned()];
    config.filter.content_snapshot.include_only_apps = vec!["com.apple.Terminal".to_owned()];
    zanei_core::config::save(&config, &fixture.config).expect("three-scope config");
    let show = fixture
        .command()
        .args(["filter", "show"])
        .output()
        .expect("filter show");
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let stdout = String::from_utf8(show.stdout).expect("filter show UTF-8");
    for line in [
        "Apps (all events):",
        "Sites (all events):",
        "Text content — apps:",
        "Text content — sites:",
        "Content snapshots — apps:",
        "Content snapshots — sites:",
        "Built-in excluded apps:",
        "(not installed)",
    ] {
        assert!(stdout.contains(line), "missing {line:?}:\n{stdout}");
    }
}

#[test]
fn config_init_and_content_snapshot_non_tty_confirmation_preserve_bytes_until_quiet() {
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

    let before = fs::read(&fixture.config).expect("config before non-TTY enable");
    let refused = fixture
        .command()
        .args(["config", "set", "capture.content_snapshot", "true"])
        .output()
        .expect("non-TTY content snapshot enable");
    assert_eq!(refused.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("Current scope (change it first if this is not what you want):")
    );
    assert_eq!(
        fs::read(&fixture.config).expect("config after refused non-TTY enable"),
        before
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

fn run_injected<const N: usize>(
    fixture: &Fixture,
    app_directory: &dyn AppDirectory,
    args: [&str; N],
) -> Result<u8, zanei_cli::CliError> {
    let mut command = vec![
        "zanei".to_owned(),
        "--config".to_owned(),
        fixture.config.display().to_string(),
        "--store".to_owned(),
        fixture.store.display().to_string(),
    ];
    command.extend(args.map(str::to_owned));
    let cli = Cli::try_parse_from(command).expect("injected CLI arguments");
    zanei_cli::run_with_app_directory(cli, app_directory)
}

struct FakeAppDirectory {
    installed: Vec<AppInfo>,
    running: Vec<AppInfo>,
}

impl FakeAppDirectory {
    fn terminal() -> Self {
        Self {
            installed: vec![AppInfo {
                name: "Terminal".to_owned(),
                bundle_id: Some("com.apple.Terminal".to_owned()),
                path: Some("/Applications/Utilities/Terminal.app".into()),
            }],
            running: Vec::new(),
        }
    }
}

impl AppDirectory for FakeAppDirectory {
    fn installed(&self) -> Result<Vec<AppInfo>, AppDirectoryError> {
        Ok(self.installed.clone())
    }

    fn running(&self) -> Result<Vec<AppInfo>, AppDirectoryError> {
        Ok(self.running.clone())
    }

    fn installed_by_id(&self, bundle_id: &str) -> Result<Option<AppInfo>, AppDirectoryError> {
        Ok(self
            .installed
            .iter()
            .find(|app| {
                app.bundle_id
                    .as_ref()
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(bundle_id))
            })
            .cloned())
    }
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
