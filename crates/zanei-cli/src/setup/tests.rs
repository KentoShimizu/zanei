use std::ffi::OsString;
use std::fs;
use std::path::Path;

use serde_json::Value;
use tempfile::TempDir;

use super::assets::SKILL;
use super::{Agent, Scope, SetupError, SetupReport, resolve_config_directory, run_at};

/// `run_at` with the config directory an unset `XDG_CONFIG_HOME` would produce.
fn setup(
    agent: Agent,
    scope: Scope,
    print_only: bool,
    project_dir: &Path,
    home_dir: &Path,
) -> Result<SetupReport, SetupError> {
    run_at(
        agent,
        scope,
        print_only,
        project_dir,
        home_dir,
        &home_dir.join(".config"),
    )
}

#[test]
fn print_plan_does_not_write() {
    let project = TempDir::new().expect("project tempdir");
    let home = TempDir::new().expect("home tempdir");

    let report = setup(
        Agent::Claude,
        Scope::Project,
        true,
        project.path(),
        home.path(),
    )
    .expect("preview");

    assert!(!project.path().join(".claude").exists());
    let output = report.to_string();
    assert!(output.contains("zanei timeline --since 2h --format md"));
    assert!(output.contains("claude mcp add --scope project"));
}

#[test]
fn claude_skill_and_mcp_command_follow_the_requested_scope() {
    let project = TempDir::new().expect("project tempdir");
    let home = TempDir::new().expect("home tempdir");

    let project_report = setup(
        Agent::Claude,
        Scope::Project,
        false,
        project.path(),
        home.path(),
    )
    .expect("project setup");
    let project_skill = project.path().join(".claude/skills/zanei/SKILL.md");
    assert_eq!(
        fs::read_to_string(project_skill).expect("project skill"),
        SKILL
    );
    assert!(
        project_report
            .to_string()
            .contains("claude mcp add --scope project zanei -- zanei mcp")
    );

    let user_report = setup(
        Agent::Claude,
        Scope::User,
        false,
        project.path(),
        home.path(),
    )
    .expect("user setup");
    let user_skill = home.path().join(".claude/skills/zanei/SKILL.md");
    assert_eq!(fs::read_to_string(user_skill).expect("user skill"), SKILL);
    assert!(
        user_report
            .to_string()
            .contains("claude mcp add --scope user zanei -- zanei mcp")
    );
}

#[test]
fn codex_skill_is_user_global_exact_and_idempotent_for_both_scopes() {
    let project = TempDir::new().expect("project tempdir");
    let home = TempDir::new().expect("home tempdir");
    let agents = project.path().join("AGENTS.md");
    fs::write(&agents, "# Existing instructions\n").expect("seed AGENTS");

    let skill = home.path().join(".codex/skills/zanei/SKILL.md");

    let first_report = setup(
        Agent::Codex,
        Scope::Project,
        false,
        project.path(),
        home.path(),
    )
    .expect("first setup");
    let first = fs::read_to_string(&skill).expect("first skill");
    let second_report = setup(
        Agent::Codex,
        Scope::User,
        false,
        project.path(),
        home.path(),
    )
    .expect("second setup");
    let second = fs::read_to_string(&skill).expect("second skill");

    assert!(first_report.has_changes());
    assert_eq!(first, SKILL);
    assert_eq!(first, second);
    assert!(!second_report.has_changes());
    assert_eq!(
        fs::read_to_string(agents).expect("unchanged AGENTS"),
        "# Existing instructions\n"
    );
    assert!(!home.path().join(".codex/AGENTS.md").exists());
    assert!(
        second_report
            .to_string()
            .contains("codex mcp add zanei -- zanei mcp")
    );
}

#[test]
fn opencode_skill_follows_the_requested_scope_and_prints_the_mcp_entry() {
    let project = TempDir::new().expect("project tempdir");
    let home = TempDir::new().expect("home tempdir");
    let project_config = project.path().join("opencode.json");
    fs::write(&project_config, r#"{"theme":"project"}"#).expect("project config");

    let project_report = setup(
        Agent::Opencode,
        Scope::Project,
        false,
        project.path(),
        home.path(),
    )
    .expect("project setup");
    assert_eq!(
        fs::read_to_string(project.path().join(".opencode/skills/zanei/SKILL.md"))
            .expect("project skill"),
        SKILL
    );

    let user_report = setup(
        Agent::Opencode,
        Scope::User,
        false,
        project.path(),
        home.path(),
    )
    .expect("user setup");
    assert_eq!(
        fs::read_to_string(home.path().join(".config/opencode/skills/zanei/SKILL.md"))
            .expect("user skill"),
        SKILL
    );

    // setup never edits opencode.json itself; the entry is printed for the user to place.
    assert_eq!(
        fs::read_to_string(project_config).expect("unchanged project config"),
        r#"{"theme":"project"}"#
    );
    for report in [project_report, user_report] {
        let output = report.to_string();
        assert!(output.contains("Add this mcp.zanei server entry to your opencode.json:"));
        assert!(output.contains(r#""command": ["zanei", "mcp"]"#));
        assert!(!output.contains("[mcp command]"));
    }
}

#[test]
fn opencode_preview_does_not_write_the_skill() {
    let project = TempDir::new().expect("project tempdir");
    let home = TempDir::new().expect("home tempdir");

    let report = setup(
        Agent::Opencode,
        Scope::Project,
        true,
        project.path(),
        home.path(),
    )
    .expect("preview");

    assert!(!project.path().join(".opencode").exists());
    assert!(report.to_string().contains("zanei timeline --since 2h"));
}

#[test]
fn hermes_skill_and_mcp_command_remain_user_global() {
    let project = TempDir::new().expect("project tempdir");
    let home = TempDir::new().expect("home tempdir");

    let report = setup(
        Agent::Hermes,
        Scope::Project,
        false,
        project.path(),
        home.path(),
    )
    .expect("setup");

    let skill = home.path().join(".hermes/skills/zanei/SKILL.md");
    assert_eq!(fs::read_to_string(skill).expect("Hermes skill"), SKILL);
    assert!(!project.path().join(".hermes").exists());
    let output = report.to_string();
    assert!(output.contains("hermes mcp add zanei --command zanei --args mcp"));
    assert!(output.contains("Hermes setup is always user-global"));
}

#[test]
fn claude_desktop_json_merge_preserves_unrelated_servers() {
    let project = TempDir::new().expect("project tempdir");
    let home = TempDir::new().expect("home tempdir");
    let directory = home.path().join("Library/Application Support/Claude");
    fs::create_dir_all(&directory).expect("desktop directory");
    let config = directory.join("claude_desktop_config.json");
    fs::write(
        &config,
        r#"{"preferences":{"compact":true},"mcpServers":{"other":{"command":"other"}}}"#,
    )
    .expect("seed config");

    setup(
        Agent::ClaudeDesktop,
        Scope::Project,
        false,
        project.path(),
        home.path(),
    )
    .expect("setup");

    let value: Value =
        serde_json::from_str(&fs::read_to_string(config).expect("config")).expect("valid json");
    assert_eq!(value["preferences"]["compact"], true);
    assert_eq!(value["mcpServers"]["other"]["command"], "other");
    assert_eq!(
        value["mcpServers"]["zanei"]["args"],
        serde_json::json!(["mcp"])
    );
}

#[test]
fn pi_skill_follows_the_requested_scope_and_registers_no_mcp() {
    let project = TempDir::new().expect("project tempdir");
    let home = TempDir::new().expect("home tempdir");

    let project_report = setup(
        Agent::Pi,
        Scope::Project,
        false,
        project.path(),
        home.path(),
    )
    .expect("project setup");
    assert_eq!(
        fs::read_to_string(project.path().join(".pi/skills/zanei/SKILL.md"))
            .expect("project skill"),
        SKILL
    );
    assert!(!home.path().join(".pi").exists());

    let user_report =
        setup(Agent::Pi, Scope::User, false, project.path(), home.path()).expect("user setup");
    assert_eq!(
        fs::read_to_string(home.path().join(".pi/agent/skills/zanei/SKILL.md"))
            .expect("user skill"),
        SKILL
    );

    for report in [project_report, user_report] {
        let output = report.to_string();
        assert!(output.contains("pi does not support MCP"));
        assert!(!output.contains("[mcp command]"));
        assert!(!output.contains("[manual setup]"));
    }
}

#[test]
fn pi_preview_does_not_write_the_skill() {
    let project = TempDir::new().expect("project tempdir");
    let home = TempDir::new().expect("home tempdir");

    let report = setup(Agent::Pi, Scope::User, true, project.path(), home.path()).expect("preview");

    assert!(!home.path().join(".pi").exists());
    assert!(report.to_string().contains("zanei timeline --since 2h"));
}

#[test]
fn opencode_user_skill_follows_xdg_config_home() {
    let project = TempDir::new().expect("project tempdir");
    let home = TempDir::new().expect("home tempdir");
    let xdg = TempDir::new().expect("xdg tempdir");

    run_at(
        Agent::Opencode,
        Scope::User,
        false,
        project.path(),
        home.path(),
        xdg.path(),
    )
    .expect("user setup");

    assert_eq!(
        fs::read_to_string(xdg.path().join("opencode/skills/zanei/SKILL.md")).expect("xdg skill"),
        SKILL
    );
    assert!(!home.path().join(".config").exists());
}

#[test]
fn config_directory_ignores_an_unset_or_relative_xdg_config_home() {
    let home = Path::new("/home/example");

    assert_eq!(
        resolve_config_directory(None, home),
        home.join(".config"),
        "unset falls back to ~/.config"
    );
    assert_eq!(
        resolve_config_directory(Some(OsString::from("relative/config")), home),
        home.join(".config"),
        "a relative value is invalid under XDG and must not be resolved against the cwd"
    );
    assert_eq!(
        resolve_config_directory(Some(OsString::from("/elsewhere/config")), home),
        Path::new("/elsewhere/config"),
        "an absolute value wins"
    );
}
