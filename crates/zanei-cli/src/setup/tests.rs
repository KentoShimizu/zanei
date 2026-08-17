use std::fs;

use serde_json::Value;
use tempfile::TempDir;

use super::assets::SKILL;
use super::{Agent, Scope, run_at};

#[test]
fn print_plan_does_not_write() {
    let project = TempDir::new().expect("project tempdir");
    let home = TempDir::new().expect("home tempdir");

    let report = run_at(
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

    let project_report = run_at(
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

    let user_report = run_at(
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

    let first_report = run_at(
        Agent::Codex,
        Scope::Project,
        false,
        project.path(),
        home.path(),
    )
    .expect("first setup");
    let first = fs::read_to_string(&skill).expect("first skill");
    let second_report = run_at(
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
fn opencode_is_manual_only_for_both_scopes_and_print_modes() {
    for scope in [Scope::Project, Scope::User] {
        for print_only in [false, true] {
            let project = TempDir::new().expect("project tempdir");
            let home = TempDir::new().expect("home tempdir");
            let project_agents = project.path().join("AGENTS.md");
            let project_config = project.path().join("opencode.json");
            let user_directory = home.path().join(".config/opencode");
            let user_agents = user_directory.join("AGENTS.md");
            let user_config = user_directory.join("opencode.json");
            fs::create_dir_all(&user_directory).expect("user opencode directory");
            fs::write(&project_agents, "# Project instructions\n").expect("project AGENTS");
            fs::write(&project_config, r#"{"theme":"project"}"#).expect("project config");
            fs::write(&user_agents, "# User instructions\n").expect("user AGENTS");
            fs::write(&user_config, r#"{"theme":"user"}"#).expect("user config");

            let report = run_at(
                Agent::Opencode,
                scope,
                print_only,
                project.path(),
                home.path(),
            )
            .expect("manual setup");

            assert!(!report.has_changes());
            assert_eq!(
                fs::read_to_string(project_agents).expect("unchanged project AGENTS"),
                "# Project instructions\n"
            );
            assert_eq!(
                fs::read_to_string(project_config).expect("unchanged project config"),
                r#"{"theme":"project"}"#
            );
            assert_eq!(
                fs::read_to_string(user_agents).expect("unchanged user AGENTS"),
                "# User instructions\n"
            );
            assert_eq!(
                fs::read_to_string(user_config).expect("unchanged user config"),
                r#"{"theme":"user"}"#
            );

            let output = report.to_string();
            assert!(output.contains("Paste these instructions anywhere in your AGENTS.md:"));
            assert!(output.contains("## Zanei activity context"));
            assert!(output.contains(canonical_body_without_heading()));
            assert!(output.contains("Add this mcp.zanei server entry to your opencode.json:"));
            assert!(output.contains(r#""mcp": {"#));
            assert!(output.contains(r#""command": ["zanei", "mcp"]"#));
            assert!(!output.contains("name: zanei"));
            assert!(!output.contains("description: Recover recent local activity context"));
        }
    }
}

#[test]
fn hermes_skill_and_mcp_command_remain_user_global() {
    let project = TempDir::new().expect("project tempdir");
    let home = TempDir::new().expect("home tempdir");

    let report = run_at(
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

    run_at(
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
fn pi_is_manual_only_for_both_scopes_and_print_modes() {
    for scope in [Scope::Project, Scope::User] {
        for print_only in [false, true] {
            let project = TempDir::new().expect("project tempdir");
            let home = TempDir::new().expect("home tempdir");
            let project_readme = project.path().join(".pi/skills/zanei/README.md");
            let user_readme = home.path().join(".pi/skills/zanei/README.md");
            fs::create_dir_all(project_readme.parent().expect("project README parent"))
                .expect("project pi directory");
            fs::create_dir_all(user_readme.parent().expect("user README parent"))
                .expect("user pi directory");
            fs::write(&project_readme, "# Project README\n").expect("project README");
            fs::write(&user_readme, "# User README\n").expect("user README");

            let report = run_at(Agent::Pi, scope, print_only, project.path(), home.path())
                .expect("manual setup");

            assert!(!report.has_changes());
            assert_eq!(
                fs::read_to_string(project_readme).expect("unchanged project README"),
                "# Project README\n"
            );
            assert_eq!(
                fs::read_to_string(user_readme).expect("unchanged user README"),
                "# User README\n"
            );
            let output = report.to_string();
            assert!(
                output.contains(
                    "Paste these instructions into a README or another file that pi reads:"
                )
            );
            assert!(output.contains("# Zanei activity context"));
            assert!(output.contains(canonical_body_without_heading()));
            assert!(
                output.contains("pi uses the CLI skill only and does not register an MCP server")
            );
            assert!(!output.contains("name: zanei"));
            assert!(!output.contains("description: Recover recent local activity context"));
            assert!(!output.contains("opencode.json"));
            assert!(!output.contains("[mcp command]"));
        }
    }
}

fn canonical_body_without_heading() -> &'static str {
    let (_, body) = SKILL
        .split_once("\n---\n")
        .expect("canonical skill frontmatter");
    let (_, instructions) = body
        .trim_start()
        .split_once('\n')
        .expect("canonical skill heading");
    instructions.trim()
}
