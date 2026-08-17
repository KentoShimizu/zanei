use std::fs;
use std::path::Path;
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use tempfile::TempDir;
use time::OffsetDateTime;
use zanei_core::config::Config;
use zanei_core::normalize::normalize;
use zanei_core::schema::{App, EmptyData, EventData, KNOWN_EVENT_TYPES, RawEvent};
use zanei_core::store::{QueryFilter, StoreReader, StoreWriter};
use zanei_core::timeline::MIN_TIMELINE_TOKEN_BUDGET_TOKENS;

mod support;

use support::Fixture;

const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const DAEMON_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[test]
fn query_timeline_and_export_read_fixture_store() {
    let fixture = Fixture::populated();

    let query = fixture
        .command()
        .args(["query", "--since", "1h", "--format", "json"])
        .output()
        .expect("query output");
    assert!(query.status.success());
    assert!(String::from_utf8_lossy(&query.stdout).contains("app.launch"));

    let timeline = fixture
        .command()
        .args(["timeline", "--since", "1h", "--format", "md"])
        .output()
        .expect("timeline output");
    assert!(timeline.status.success());
    assert!(String::from_utf8_lossy(&timeline.stdout).contains("FixtureApp"));

    let output = fixture.directory.path().join("export.jsonl");
    fixture
        .command()
        .args(["export", "--since", "1h", "--out"])
        .arg(&output)
        .assert()
        .success();
    assert!(
        fs::read_to_string(output)
            .expect("export")
            .contains("app.launch")
    );
}

#[test]
fn status_uses_the_owner_lock_instead_of_a_fresh_orphaned_heartbeat() {
    let fixture = Fixture::populated();
    let output = fixture
        .command()
        .args(["status", "--json"])
        .output()
        .expect("status output");

    assert_eq!(output.status.code(), Some(4));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(value["state"], "stopped");
    assert_eq!(value["running"], false);
    assert_eq!(value["instance"], serde_json::Value::Null);
    assert_eq!(value["mode"], serde_json::Value::Null);
    assert_eq!(value["events_captured"], KNOWN_EVENT_TYPES.len());
    assert_eq!(value["events_dropped"], 2);
    assert_eq!(value["collector_failures"]["eventtap"], 1);
    assert_eq!(value["degraded"], serde_json::json!({}));
    assert_eq!(value["capture"]["sources"][0], "app");
    assert!(value["store"]["size_bytes"].as_u64().is_some());
    assert_eq!(value["permissions_ok"], true);
}

#[test]
fn status_ignores_orphaned_heartbeat_retention() {
    let fixture = Fixture::populated();
    fs::write(
        &fixture.config,
        "[capture]\nsources = [\"app\"]\n\n[output]\nretention_hours = 1\n",
    )
    .expect("replace configured retention");

    let output = fixture
        .command()
        .args(["status", "--json"])
        .output()
        .expect("status output");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(value["store"]["retention_hours"], 1);
}

#[test]
fn status_ignores_orphaned_recorder_permissions() {
    let fixture = Fixture::populated();
    fixture.set_recorder_permissions(false);

    let output = fixture
        .command()
        .args(["status", "--json"])
        .output()
        .expect("status output");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(value["permissions_ok"], true);
}

#[test]
fn status_human_output_shows_text_content_opt_in_state() {
    let fixture = Fixture::populated();
    let output = fixture
        .command()
        .arg("status")
        .output()
        .expect("human status output");

    assert_eq!(output.status.code(), Some(4));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("STATE             stopped"));
    assert!(stdout.contains("INSTANCE          -"));
    assert!(stdout.contains("MODE              -"));
    assert!(
        stdout
            .contains("TEXT CONTENT      off (opt-in: zanei config set capture.text_content true)")
    );

    fs::write(
        &fixture.config,
        "[capture]\nsources = [\"app\"]\ntext_content = true\n",
    )
    .expect("opt-in config");
    let output = fixture
        .command()
        .arg("status")
        .output()
        .expect("opt-in human status output");

    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stdout).contains("TEXT CONTENT      on (opt-in)"));
}

#[test]
fn status_reports_a_corrupt_store_without_overwriting_it() {
    let fixture = Fixture::empty();
    let corrupt_header = b"not a sqlite store\n";
    fs::write(&fixture.store, corrupt_header).expect("corrupt fixture store header");

    let output = fixture
        .command()
        .args(["status", "--json"])
        .output()
        .expect("corrupt store JSON status");

    assert_eq!(output.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(value["state"], "store_corrupt");
    assert_eq!(value["running"], false);
    assert_eq!(value["events_captured"], serde_json::Value::Null);
    assert!(value["degraded"]["store"].as_str().is_some());
    assert_eq!(
        fs::read(&fixture.store).expect("preserved corrupt store"),
        corrupt_header
    );

    let human = fixture
        .command()
        .arg("status")
        .output()
        .expect("corrupt store human status");
    assert_eq!(human.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&human.stdout).contains("STATE             store_corrupt"));
    assert_eq!(
        fs::read(&fixture.store).expect("preserved corrupt store after human status"),
        corrupt_header
    );
}

#[test]
fn relevant_help_describes_every_option_and_config_init() {
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["doctor", "--help"],
            &[
                "Open System Settings for each missing permission",
                "Use this configuration file instead of the default path",
                "Print structured JSON when supported",
            ],
        ),
        (
            &["start", "--help"],
            &[
                "Run recording in the foreground without launchd",
                "Use this event store instead of the default path",
                "Suppress progress messages and notices",
            ],
        ),
        (
            &["status", "--help"],
            &[
                "Print structured JSON when supported",
                "Print diagnostic details to stderr",
            ],
        ),
        (
            &["pause", "--help"],
            &["Pause for this duration; omit to pause indefinitely"],
        ),
        (
            &["record", "--help"],
            &[
                "Stream events to stdout as they occur",
                "Write events to this file instead of stdout",
                "Write events in this output format",
            ],
        ),
        (
            &["query", "--help"],
            &[
                "Start of the time range (relative duration, RFC3339 timestamp, or now)",
                "End of the time range (relative duration, RFC3339 timestamp, or now)",
                "Filter by comma-separated event types; trailing wildcards are allowed",
                "Filter by application name",
                "Filter by application bundle identifier",
                "Return at most this many events",
                "Write events in this output format",
            ],
        ),
        (
            &["timeline", "--help"],
            &[
                "Start of the time range (relative duration, RFC3339 timestamp, or now)",
                "End of the time range (relative duration, RFC3339 timestamp, or now)",
                "Write LLM-ready Markdown or structured JSON",
                "Approximate token cap; content is coarsened to fit",
                "Summarize by session or by interaction",
            ],
        ),
        (
            &["export", "--help"],
            &[
                "Start of the time range (relative duration, RFC3339 timestamp, or now)",
                "End of the time range (relative duration, RFC3339 timestamp, or now)",
                "Write events in this output format",
                "Write output to this file instead of stdout",
            ],
        ),
        (
            &["purge", "--help"],
            &[
                "Delete events older than this time expression",
                "Delete every stored event; prompts unless --quiet",
            ],
        ),
        (
            &["setup", "--help"],
            &[
                "Agent integration to configure",
                "Configure the current project or user account; ignored by opencode and pi",
                "Preview planned file changes without writing; opencode and pi are always write-free",
            ],
        ),
        (
            &["config", "--help"],
            &[
                "init  Create a fully commented configuration template",
                "Use this configuration file instead of the default path",
            ],
        ),
    ];

    for (arguments, expected) in cases {
        let output = Command::cargo_bin("zanei")
            .expect("zanei binary")
            .args(*arguments)
            .output()
            .expect("help output");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        for text in *expected {
            assert!(stdout.contains(text), "missing {text:?} in:\n{stdout}");
        }
    }
}

#[test]
fn config_init_creates_a_complete_template_and_never_overwrites() {
    let home = TempDir::new().expect("default config init home");
    let default_config = home.path().join(".config/zanei/config.toml");
    let output = Command::cargo_bin("zanei")
        .expect("zanei binary")
        .env("HOME", home.path())
        .args(["config", "init"])
        .output()
        .expect("default config init output");

    assert!(output.status.success());
    assert!(default_config.exists());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&default_config.display().to_string())
    );

    let directory = TempDir::new().expect("config init fixture");
    let config = directory.path().join("nested/config.toml");
    let output = Command::cargo_bin("zanei")
        .expect("zanei binary")
        .arg("--config")
        .arg(&config)
        .args(["config", "init"])
        .output()
        .expect("config init output");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains(&config.display().to_string()));
    let template = fs::read_to_string(&config).expect("generated configuration");
    assert_eq!(
        Config::from_toml(&template).expect("generated template should parse"),
        Config::default()
    );
    assert_eq!(
        template
            .lines()
            .filter(|line| line.starts_with("# "))
            .count(),
        9
    );

    let sentinel = "# keep this existing file\n";
    fs::write(&config, sentinel).expect("existing configuration");
    let output = Command::cargo_bin("zanei")
        .expect("zanei binary")
        .arg("--config")
        .arg(&config)
        .args(["config", "init"])
        .output()
        .expect("existing config init output");

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains(&config.display().to_string()));
    assert_eq!(
        fs::read_to_string(&config).expect("preserved configuration"),
        sentinel
    );
}

#[test]
fn config_set_persists_a_valid_scalar_value() {
    let fixture = Fixture::empty();

    fixture
        .command()
        .args(["config", "set", "output.retention_hours", "72"])
        .assert()
        .success();

    let config = Config::load(&fixture.config).expect("edited config");
    assert_eq!(config.output.retention_hours, 72);
}

#[test]
fn config_set_rejects_unknown_keys_and_invalid_values_without_writing() {
    let fixture = Fixture::empty();
    let original = fs::read_to_string(&fixture.config).expect("original config");
    let cases = [
        (
            ["config", "set", "capture.unknown", "true"],
            "unknown configuration key: capture.unknown",
        ),
        (
            ["config", "set", "capture.sources", "app"],
            "arrays are managed with dedicated commands (filter) or config edit",
        ),
        (
            ["config", "set", "output.mode", "stream"],
            "unknown configuration key: output.mode",
        ),
        (
            ["config", "set", "output.store", "sqlite"],
            "unknown configuration key: output.store",
        ),
        (
            ["config", "set", "capture.text_content", "yes"],
            "invalid value for capture.text_content: yes; expected true or false",
        ),
    ];

    for (arguments, expected_error) in cases {
        let output = fixture
            .command()
            .args(arguments)
            .output()
            .expect("config set error output");

        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "missing {expected_error:?} in:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(&fixture.config).expect("unchanged config"),
            original
        );
    }
}

#[test]
fn config_set_restart_hint_requires_a_live_store_owner() {
    let fixture = Fixture::populated();
    let expected = "Restart recording with `zanei stop && zanei start` for this to take effect.";

    let orphaned_output = fixture
        .command()
        .args(["config", "set", "capture.text_content", "true"])
        .output()
        .expect("orphaned heartbeat config output");
    assert!(orphaned_output.status.success());
    assert!(!String::from_utf8_lossy(&orphaned_output.stdout).contains(expected));

    let directory = TempDir::new().expect("live recorder config fixture");
    let config = directory.path().join("config.toml");
    let store = directory.path().join("store.sqlite");
    fs::write(&config, "[capture]\nsources = []\n").expect("live recorder config");
    let mut child = spawn_foreground_daemon(&config, &store);
    wait_for_daemon_ready(&mut child, &store);
    let live_output = command(&config, &store)
        .args(["config", "set", "capture.text_content", "true"])
        .output()
        .expect("live recorder config output");
    assert!(live_output.status.success());
    assert!(String::from_utf8_lossy(&live_output.stdout).contains(expected));
    signal_child(&mut child, "TERM");
    assert!(wait_for_child(&mut child).success());
}

#[test]
fn purge_all_quiet_deletes_fixture_events_without_prompt() {
    let fixture = Fixture::populated();
    fixture
        .command()
        .args(["purge", "--all", "--quiet"])
        .assert()
        .success();

    let events = StoreReader::open(&fixture.store)
        .expect("reader")
        .query(&QueryFilter::default(), 48)
        .expect("remaining events");
    assert!(events.is_empty());
}

#[test]
fn filter_edits_are_validated_and_persisted() {
    let fixture = Fixture::populated();
    fixture
        .command()
        .args(["filter", "only-site", "add", "github.com"])
        .assert()
        .success();
    let config = Config::load(&fixture.config).expect("edited config");
    assert_eq!(config.filter.include_only_websites, ["github.com"]);

    fixture
        .command()
        .args(["filter", "only-site", "remove", "github.com"])
        .assert()
        .success();
    let config = Config::load(&fixture.config).expect("edited config");
    assert!(config.filter.include_only_websites.is_empty());
}

#[test]
fn every_public_command_classifies_invalid_arguments_as_usage_errors() {
    let fixture = Fixture::uninitialized();
    let cases: &[&[&str]] = &[
        &["doctor", "--fix=maybe"],
        &["start", "--foreground=maybe"],
        &["stop", "unexpected"],
        &["pause", "--for", "bogus"],
        &["resume", "unexpected"],
        &["status", "unexpected"],
        &["record", "--format", "xml", "--stream"],
        &["query", "--limit", "invalid"],
        &["query", "--since", "bogus"],
        &["query", "--types", "browser.*.navigate"],
        &["timeline", "--token-budget", "0"],
        &["export", "--format", "xml"],
        &["purge", "--before", "bogus"],
        &["filter", "only-site", "add", "https://example.com"],
        &["config", "set", "output.mode", "stream"],
        &["mcp", "unexpected"],
        &["setup", "--agent", "unknown"],
    ];

    for arguments in cases {
        let output = fixture
            .command()
            .args(*arguments)
            .output()
            .expect("invalid argument output");
        assert_eq!(
            output.status.code(),
            Some(2),
            "{arguments:?} produced stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!fixture.store.exists());
}

#[test]
fn timeline_rejects_token_budget_below_minimum_as_usage_error() {
    let fixture = Fixture::uninitialized();
    let output = fixture
        .command()
        .args(["timeline", "--token-budget"])
        .arg((MIN_TIMELINE_TOKEN_BUDGET_TOKENS - 1).to_string())
        .output()
        .expect("invalid token budget output");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains(&MIN_TIMELINE_TOKEN_BUDGET_TOKENS.to_string())
    );
    assert!(!fixture.store.exists());
}

#[test]
fn missing_daemon_uses_the_dedicated_exit_code() {
    let output = Fixture::uninitialized()
        .command()
        .args(["status", "--json"])
        .output()
        .expect("missing daemon status");

    assert_eq!(output.status.code(), Some(4));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(value["state"], "stopped");
    assert_eq!(value["running"], false);
    assert_eq!(value["events_captured"], serde_json::Value::Null);
}

// The runner's TCC state is out of the test's control (developer machines
// usually lack Input Monitoring, CI images usually have it), so assert that
// doctor's exit code and JSON are consistent with whichever state is real.
#[test]
fn doctor_json_matches_the_real_permission_state() {
    let directory = TempDir::new().expect("doctor fixture");
    let config = directory.path().join("config.toml");
    let store = directory.path().join("missing.sqlite");
    fs::write(&config, "[capture]\nsources = [\"input\"]\n").expect("doctor config");

    let output = command(&config, &store)
        .args(["doctor", "--json"])
        .output()
        .expect("doctor output");

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
    assert_eq!(value["capture_sources"], serde_json::json!(["input"]));
    assert_eq!(value["reported_by_recorder"], false);
    let granted = value["permissions"]["input_monitoring"]["status"] == "granted";
    if granted {
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(value["ok"], true);
        assert_eq!(value["missing_required"], serde_json::json!([]));
    } else {
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(value["ok"], false);
        assert_eq!(
            value["missing_required"],
            serde_json::json!(["input_monitoring"])
        );
        assert_eq!(
            value["settings_pane"],
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
        );
    }
}

#[test]
fn setup_codex_print_shows_user_skill_and_writes_nothing() {
    let fixture = Fixture::populated();
    let project = fixture.directory.path().join("project");
    fs::create_dir(&project).expect("project");
    let home = TempDir::new().expect("home tempdir");
    let skill = home.path().join(".codex/skills/zanei/SKILL.md");
    let mut command = fixture.command();
    command.current_dir(&project).env("HOME", home.path());
    let output = command
        .args(["setup", "--agent", "codex", "--print"])
        .output()
        .expect("setup output");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&skill.display().to_string()));
    assert!(stdout.contains("---\nname: zanei\n"));
    assert!(stdout.contains("zanei timeline --since 2h --format md"));
    assert!(stdout.contains("zanei config set capture.text_content true"));
    assert!(stdout.contains("zanei stop && zanei start"));
    assert!(stdout.contains("codex mcp add zanei -- zanei mcp"));
    assert!(!skill.exists());
    assert!(!project.join("AGENTS.md").exists());
    assert!(!home.path().join(".codex/AGENTS.md").exists());
}

#[test]
fn setup_opencode_and_pi_always_show_manual_steps_and_write_nothing() {
    let fixture = Fixture::populated();
    let home = TempDir::new().expect("home tempdir");

    for agent in ["opencode", "pi"] {
        for print_only in [false, true] {
            let project = fixture
                .directory
                .path()
                .join(format!("{agent}-{print_only}"));
            fs::create_dir(&project).expect("project");
            let agents = project.join("AGENTS.md");
            let readme = project.join("README.md");
            let opencode_config = project.join("opencode.json");
            fs::write(&agents, "# Existing agent instructions\n").expect("AGENTS");
            fs::write(&readme, "# Existing README\n").expect("README");
            fs::write(&opencode_config, r#"{"theme":"existing"}"#).expect("opencode config");

            let mut arguments = vec!["setup", "--agent", agent];
            if print_only {
                arguments.push("--print");
            }
            let mut command = fixture.command();
            command.current_dir(&project).env("HOME", home.path());
            let output = command.args(arguments).output().expect("setup output");

            assert!(output.status.success());
            assert_eq!(
                fs::read_to_string(agents).expect("unchanged AGENTS"),
                "# Existing agent instructions\n"
            );
            assert_eq!(
                fs::read_to_string(readme).expect("unchanged README"),
                "# Existing README\n"
            );
            assert_eq!(
                fs::read_to_string(opencode_config).expect("unchanged opencode config"),
                r#"{"theme":"existing"}"#
            );
            assert!(!project.join(".pi").exists());

            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains("[manual setup]"));
            assert!(stdout.contains("Zanei activity context"));
            assert!(stdout.contains("zanei timeline --since 2h --format md"));
            assert!(!stdout.contains("name: zanei"));
            assert!(!stdout.contains("description: Recover recent local activity context"));
            if agent == "opencode" {
                assert!(stdout.contains("Paste these instructions anywhere in your AGENTS.md:"));
                assert!(stdout.contains("Add this mcp.zanei server entry to your opencode.json:"));
                assert!(stdout.contains(r#""mcp": {"#));
                assert!(stdout.contains(r#""command": ["zanei", "mcp"]"#));
            } else {
                assert!(stdout.contains(
                    "Paste these instructions into a README or another file that pi reads:"
                ));
                assert!(stdout.contains("does not register an MCP server"));
                assert!(!stdout.contains("opencode.json"));
                assert!(!stdout.contains("[mcp command]"));
            }
        }
    }
}

#[test]
fn record_non_tty_path_exits_without_creating_a_store() {
    let directory = TempDir::new().expect("record fixture");
    let config = directory.path().join("config.toml");
    let store = directory.path().join("must-not-exist.sqlite");
    fs::write(&config, "[capture]\nsources = []\n").expect("config");
    command(&config, &store)
        .args(["record", "--stream"])
        .write_stdin("")
        .assert()
        .success();
    assert!(!store.exists());
}

#[test]
fn daemon_exits_cleanly_on_sigint_and_sigterm() {
    for signal in ["INT", "TERM"] {
        assert_daemon_shutdown(signal);
    }
}

#[test]
fn second_recorder_for_the_same_store_is_rejected_by_the_owner_lock() {
    let directory = TempDir::new().expect("daemon ownership fixture");
    let config = directory.path().join("config.toml");
    let store = directory.path().join("store.sqlite");
    fs::write(&config, "[capture]\nsources = []\n").expect("daemon config");
    let mut owner = spawn_foreground_daemon(&config, &store);
    wait_for_daemon_ready(&mut owner, &store);

    let output = command(&config, &store)
        .args(["start", "--foreground"])
        .output()
        .expect("second recorder output");

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains(&format!(
        "another recorder owns this store (pid {})",
        owner.id()
    )));
    assert_eq!(owner.try_wait().expect("poll first recorder"), None);
    signal_child(&mut owner, "TERM");
    assert!(wait_for_child(&mut owner).success());
}

#[test]
fn daemon_hot_reloads_retention_purges_immediately_and_reports_the_active_value() {
    let directory = TempDir::new().expect("retention reload fixture");
    let config = directory.path().join("config.toml");
    let store = directory.path().join("store.sqlite");
    fs::write(
        &config,
        "[capture]\nsources = []\n\n[output]\nretention_hours = 48\n",
    )
    .expect("initial daemon config");
    let mut child = spawn_foreground_daemon(&config, &store);
    wait_for_daemon_ready(&mut child, &store);

    let now = OffsetDateTime::now_utc();
    let expired = normalize(
        RawEvent {
            source: "macos.workspace".to_owned(),
            event_type: "app.launch".to_owned(),
            app: App {
                name: "ExpiredFixture".to_owned(),
                bundle_id: Some("com.example.ExpiredFixture".to_owned()),
                pid: Some(42),
            },
            window: None,
            element: None,
            data: EventData::AppLaunch(EmptyData::default()),
        },
        now - time::Duration::hours(2),
        1,
    )
    .expect("normalize expired fixture");
    StoreWriter::open(&store)
        .and_then(|mut writer| writer.append(&expired))
        .expect("store expired fixture");

    fs::write(
        &config,
        "[capture]\nsources = []\n\n[output]\nretention_hours = 1\n",
    )
    .expect("reload daemon config");
    let deadline = Instant::now() + DAEMON_STARTUP_TIMEOUT;
    loop {
        if let Ok(reader) = StoreReader::open(&store) {
            let applied = reader
                .status()
                .is_ok_and(|status| status.running && status.retention_hours == Some(1));
            let purged = reader
                .oldest_event_ts()
                .is_ok_and(|oldest| oldest.is_none());
            if applied && purged {
                break;
            }
        }
        if let Some(status) = child.try_wait().expect("poll daemon reload") {
            panic!("daemon exited before retention reload completed: {status}");
        }
        if Instant::now() >= deadline {
            stop_child(&mut child);
            panic!("retention reload did not complete within {DAEMON_STARTUP_TIMEOUT:?}");
        }
        thread::sleep(PROCESS_POLL_INTERVAL);
    }

    signal_child(&mut child, "TERM");
    assert!(wait_for_child(&mut child).success());
}

#[test]
fn stop_terminates_only_the_foreground_instance_owning_the_selected_store() {
    let directory = TempDir::new().expect("targeted stop fixture");
    let config = directory.path().join("config.toml");
    let first_store = directory.path().join("first.sqlite");
    let second_store = directory.path().join("second.sqlite");
    fs::write(&config, "[capture]\nsources = []\n").expect("daemon config");
    let mut first = spawn_foreground_daemon(&config, &first_store);
    let mut second = spawn_foreground_daemon(&config, &second_store);
    wait_for_daemon_ready(&mut first, &first_store);
    wait_for_daemon_ready(&mut second, &second_store);

    let status_output = command(&config, &first_store)
        .args(["status", "--json"])
        .output()
        .expect("selected recorder status");
    assert!(status_output.status.success());
    let status_json: serde_json::Value =
        serde_json::from_slice(&status_output.stdout).expect("selected recorder status JSON");
    assert_eq!(status_json["state"], "running");
    assert_eq!(status_json["running"], true);
    assert_eq!(status_json["mode"], "foreground");
    assert!(
        status_json["instance"]
            .as_str()
            .is_some_and(|instance| instance.starts_with(&format!("{}@", first.id())))
    );

    let output = command(&config, &first_store)
        .arg("stop")
        .output()
        .expect("targeted stop output");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(wait_for_child(&mut first).success());
    assert_eq!(second.try_wait().expect("poll other recorder"), None);
    let other_status = StoreReader::open(&second_store)
        .and_then(|reader| reader.status())
        .expect("other recorder status");
    assert_eq!(other_status.pid, Some(i64::from(second.id())));
    assert!(other_status.running);

    signal_child(&mut second, "TERM");
    assert!(wait_for_child(&mut second).success());
}

fn assert_daemon_shutdown(signal: &str) {
    let directory = TempDir::new().expect("daemon fixture");
    let config = directory.path().join("config.toml");
    let store = directory.path().join("store.sqlite");
    fs::write(&config, "[capture]\nsources = []\n").expect("daemon config");
    let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_zanei"))
        .arg("--config")
        .arg(&config)
        .arg("--store")
        .arg(&store)
        .arg("__daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start foreground daemon");

    wait_for_daemon_ready(&mut child, &store);
    signal_child(&mut child, signal);

    let status = wait_for_child(&mut child);
    assert!(
        status.success(),
        "daemon failed after SIG{signal}: {status}"
    );
    assert_eq!(
        StoreReader::open(&store)
            .expect("daemon store reader")
            .status()
            .expect("daemon status")
            .pid,
        None
    );
}

fn spawn_foreground_daemon(config: &Path, store: &Path) -> Child {
    ProcessCommand::new(env!("CARGO_BIN_EXE_zanei"))
        .arg("--config")
        .arg(config)
        .arg("--store")
        .arg(store)
        .args(["start", "--foreground"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start foreground daemon")
}

fn signal_child(child: &mut Child, signal: &str) {
    let status = ProcessCommand::new("/bin/kill")
        .args([format!("-{signal}"), child.id().to_string()])
        .status()
        .expect("signal daemon");
    if !status.success() {
        stop_child(child);
        panic!("failed to send SIG{signal}: {status}");
    }
}

fn wait_for_daemon_ready(child: &mut Child, store: &Path) {
    let deadline = Instant::now() + DAEMON_STARTUP_TIMEOUT;
    loop {
        if store.exists()
            && StoreReader::open(store)
                .and_then(|reader| reader.status())
                .is_ok_and(|status| status.pid.is_some())
        {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll daemon startup") {
            panic!("daemon exited before its heartbeat was ready: {status}");
        }
        if Instant::now() >= deadline {
            stop_child(child);
            panic!("daemon heartbeat was not ready within {DAEMON_STARTUP_TIMEOUT:?}");
        }
        thread::sleep(PROCESS_POLL_INTERVAL);
    }
}

fn wait_for_child(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + DAEMON_SHUTDOWN_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("poll daemon shutdown") {
            return status;
        }
        if Instant::now() >= deadline {
            stop_child(child);
            panic!("daemon did not exit within {DAEMON_SHUTDOWN_TIMEOUT:?}");
        }
        thread::sleep(PROCESS_POLL_INTERVAL);
    }
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn command(config: &Path, store: &Path) -> Command {
    let mut command = Command::cargo_bin("zanei").expect("zanei binary");
    command
        .arg("--config")
        .arg(config)
        .arg("--store")
        .arg(store);
    command
}
