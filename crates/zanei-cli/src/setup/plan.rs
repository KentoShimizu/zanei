use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde_json::{Map, Value, json};

use super::assets::SKILL;
use super::error::SetupError;

const OPENCODE_MCP_CONFIG: &str = r#"{
  "mcp": {
    "zanei": {
      "type": "local",
      "command": ["zanei", "mcp"]
    }
  }
}"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Agent {
    Claude,
    Codex,
    Opencode,
    Hermes,
    Pi,
    ClaudeDesktop,
}

impl fmt::Display for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Hermes => "hermes",
            Self::Pi => "pi",
            Self::ClaudeDesktop => "claude-desktop",
        })
    }
}

impl FromStr for Agent {
    type Err = SetupError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::Opencode),
            "hermes" => Ok(Self::Hermes),
            "pi" => Ok(Self::Pi),
            "claude-desktop" => Ok(Self::ClaudeDesktop),
            _ => Err(SetupError::UnsupportedAgent {
                value: value.to_owned(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Scope {
    Project,
    User,
}

impl fmt::Display for Scope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Project => "project",
            Self::User => "user",
        })
    }
}

impl FromStr for Scope {
    type Err = SetupError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "project" => Ok(Self::Project),
            "user" => Ok(Self::User),
            _ => Err(SetupError::UnsupportedScope {
                value: value.to_owned(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Change {
    Create,
    Update,
    Unchanged,
}

impl fmt::Display for Change {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Unchanged => "unchanged",
        })
    }
}

#[derive(Debug)]
struct PlannedFile {
    path: PathBuf,
    content: String,
    preview: String,
    change: Change,
}

impl PlannedFile {
    fn exact(path: PathBuf, content: &str) -> Result<Self, SetupError> {
        let current = read_optional(&path)?;
        let content = ensure_trailing_newline(content);
        let change = classify(current.as_deref(), &content);
        Ok(Self {
            path,
            preview: content.clone(),
            content,
            change,
        })
    }

    fn json(path: PathBuf, root_field: &'static str, server: Value) -> Result<Self, SetupError> {
        let current = read_optional(&path)?;
        let preview = format!(
            "{root_field}.zanei = {}\n",
            serde_json::to_string_pretty(&server).map_err(|source| {
                SetupError::SerializeJson {
                    path: path.clone(),
                    source,
                }
            })?
        );
        let content = merge_json(current.as_deref(), &path, root_field, server)?;
        let change = classify(current.as_deref(), &content);
        Ok(Self {
            path,
            content,
            preview,
            change,
        })
    }

    fn apply(&self) -> Result<(), SetupError> {
        if self.change == Change::Unchanged {
            return Ok(());
        }
        atomic_write(&self.path, self.content.as_bytes())
    }
}

#[derive(Debug)]
struct ManualStep {
    guidance: &'static str,
    content: String,
}

#[derive(Debug)]
pub(crate) struct Installation {
    agent: Agent,
    scope: Scope,
    files: Vec<PlannedFile>,
    manual_steps: Vec<ManualStep>,
    commands: Vec<&'static str>,
    notes: Vec<&'static str>,
}

impl Installation {
    pub fn build(
        agent: Agent,
        scope: Scope,
        project_dir: &Path,
        home_dir: &Path,
        config_dir: &Path,
    ) -> Result<Self, SetupError> {
        let mut installation = Self {
            agent,
            scope,
            files: Vec::new(),
            manual_steps: Vec::new(),
            commands: Vec::new(),
            notes: Vec::new(),
        };

        match agent {
            Agent::Claude => installation.add_claude(project_dir, home_dir)?,
            Agent::Codex => installation.add_codex(home_dir)?,
            Agent::Opencode => installation.add_opencode(project_dir, config_dir)?,
            Agent::Hermes => installation.add_hermes(home_dir)?,
            Agent::Pi => installation.add_pi(project_dir, home_dir)?,
            Agent::ClaudeDesktop => installation.add_claude_desktop(home_dir)?,
        }
        Ok(installation)
    }

    pub fn apply(&self) -> Result<(), SetupError> {
        for file in &self.files {
            file.apply()?;
        }
        Ok(())
    }

    pub fn report(self, print_only: bool) -> SetupReport {
        SetupReport {
            agent: self.agent,
            scope: self.scope,
            print_only,
            files: self
                .files
                .into_iter()
                .map(|file| FileReport {
                    path: file.path,
                    preview: file.preview,
                    change: file.change,
                })
                .collect(),
            manual_steps: self.manual_steps,
            commands: self.commands,
            notes: self.notes,
        }
    }

    fn add_claude(&mut self, project: &Path, home: &Path) -> Result<(), SetupError> {
        let base = match self.scope {
            Scope::Project => project.join(".claude"),
            Scope::User => home.join(".claude"),
        };
        self.files.push(PlannedFile::exact(
            base.join("skills/zanei/SKILL.md"),
            SKILL,
        )?);
        self.commands.push(match self.scope {
            Scope::Project => "claude mcp add --scope project zanei -- zanei mcp",
            Scope::User => "claude mcp add --scope user zanei -- zanei mcp",
        });
        Ok(())
    }

    fn add_codex(&mut self, home: &Path) -> Result<(), SetupError> {
        self.files.push(PlannedFile::exact(
            home.join(".codex/skills/zanei/SKILL.md"),
            SKILL,
        )?);
        self.commands.push("codex mcp add zanei -- zanei mcp");
        self.notes
            .push("Codex skill and MCP registration are always user-global; the requested scope is ignored.");
        Ok(())
    }

    fn add_opencode(&mut self, project: &Path, config: &Path) -> Result<(), SetupError> {
        let base = match self.scope {
            Scope::Project => project.join(".opencode/skills"),
            Scope::User => config.join("opencode/skills"),
        };
        self.files
            .push(PlannedFile::exact(base.join("zanei/SKILL.md"), SKILL)?);
        // opencode's `mcp add` is an interactive wizard with no non-interactive flags,
        // so the server entry is printed for the user to place instead of run as a command.
        self.manual_steps.push(ManualStep {
            guidance: "Add this mcp.zanei server entry to your opencode.json:",
            content: OPENCODE_MCP_CONFIG.to_owned(),
        });
        Ok(())
    }

    fn add_hermes(&mut self, home: &Path) -> Result<(), SetupError> {
        self.files.push(PlannedFile::exact(
            home.join(".hermes/skills/zanei/SKILL.md"),
            SKILL,
        )?);
        self.commands
            .push("hermes mcp add zanei --command zanei --args mcp");
        self.notes
            .push("Hermes setup is always user-global; the requested scope is ignored.");
        Ok(())
    }

    fn add_pi(&mut self, project: &Path, home: &Path) -> Result<(), SetupError> {
        let base = match self.scope {
            Scope::Project => project.join(".pi/skills"),
            Scope::User => home.join(".pi/agent/skills"),
        };
        self.files
            .push(PlannedFile::exact(base.join("zanei/SKILL.md"), SKILL)?);
        self.notes
            .push("pi does not support MCP, so the skill is the only surface.");
        Ok(())
    }

    fn add_claude_desktop(&mut self, home: &Path) -> Result<(), SetupError> {
        self.files.push(PlannedFile::json(
            home.join("Library/Application Support/Claude/claude_desktop_config.json"),
            "mcpServers",
            json!({ "command": "zanei", "args": ["mcp"] }),
        )?);
        self.notes.push(
            "Claude Desktop configuration is always user-global; the requested scope is ignored.",
        );
        Ok(())
    }
}

#[derive(Debug)]
struct FileReport {
    path: PathBuf,
    preview: String,
    change: Change,
}

#[derive(Debug)]
pub struct SetupReport {
    agent: Agent,
    scope: Scope,
    print_only: bool,
    files: Vec<FileReport>,
    manual_steps: Vec<ManualStep>,
    commands: Vec<&'static str>,
    notes: Vec<&'static str>,
}

impl SetupReport {
    #[cfg(test)]
    pub fn has_changes(&self) -> bool {
        self.files
            .iter()
            .any(|file| file.change != Change::Unchanged)
    }
}

impl fmt::Display for SetupReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "zanei setup {} ({}, scope={})",
            if self.print_only {
                "preview"
            } else {
                "complete"
            },
            self.agent,
            self.scope
        )?;
        for file in &self.files {
            writeln!(formatter, "\n[{}] {}", file.change, file.path.display())?;
            if self.print_only {
                writeln!(formatter, "{}", file.preview.trim_end())?;
            }
        }
        for step in &self.manual_steps {
            writeln!(formatter, "\n[manual setup] {}", step.guidance)?;
            writeln!(formatter, "{}", step.content.trim_end())?;
        }
        for command in &self.commands {
            writeln!(formatter, "\n[mcp command] {command}")?;
        }
        for note in &self.notes {
            writeln!(formatter, "\n[note] {note}")?;
        }
        Ok(())
    }
}

fn read_optional(path: &Path) -> Result<Option<String>, SetupError> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SetupError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn classify(current: Option<&str>, desired: &str) -> Change {
    match current {
        None => Change::Create,
        Some(content) if content == desired => Change::Unchanged,
        Some(_) => Change::Update,
    }
}

fn ensure_trailing_newline(content: &str) -> String {
    format!("{}\n", content.trim_end())
}

fn merge_json(
    current: Option<&str>,
    path: &Path,
    root_field: &'static str,
    server: Value,
) -> Result<String, SetupError> {
    let mut root = match current.filter(|content| !content.trim().is_empty()) {
        Some(content) => {
            serde_json::from_str::<Value>(content).map_err(|source| SetupError::InvalidJson {
                path: path.to_path_buf(),
                source,
            })?
        }
        None => Value::Object(Map::new()),
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| SetupError::JsonRootNotObject {
            path: path.to_path_buf(),
        })?;
    let servers = object
        .entry(root_field)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| SetupError::JsonFieldNotObject {
            path: path.to_path_buf(),
            field: root_field,
        })?;
    servers.insert("zanei".to_owned(), server);
    let mut rendered =
        serde_json::to_string_pretty(&root).map_err(|source| SetupError::SerializeJson {
            path: path.to_path_buf(),
            source,
        })?;
    rendered.push('\n');
    Ok(rendered)
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<(), SetupError> {
    let parent = path.parent().ok_or_else(|| SetupError::MissingParent {
        path: path.to_path_buf(),
    })?;
    fs::create_dir_all(parent).map_err(|source| SetupError::CreateDirectory {
        path: parent.to_path_buf(),
        source,
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SetupError::MissingParent {
            path: path.to_path_buf(),
        })?;
    let temporary = parent.join(format!(".{file_name}.zanei-{}.tmp", std::process::id()));
    let result = write_temporary(&temporary, content)
        .and_then(|()| preserve_permissions(path, &temporary))
        .and_then(|()| {
            fs::rename(&temporary, path).map_err(|source| SetupError::Replace {
                path: path.to_path_buf(),
                source,
            })
        });
    match result {
        Ok(()) => Ok(()),
        Err(original) => match fs::remove_file(&temporary) {
            Ok(()) => Err(original),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Err(original),
            Err(source) => Err(SetupError::Cleanup {
                path: temporary,
                original: Box::new(original),
                source,
            }),
        },
    }
}

fn write_temporary(path: &Path, content: &[u8]) -> Result<(), SetupError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(|source| SetupError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(content)
        .and_then(|()| file.sync_all())
        .map_err(|source| SetupError::Write {
            path: path.to_path_buf(),
            source,
        })
}

fn preserve_permissions(existing: &Path, temporary: &Path) -> Result<(), SetupError> {
    let metadata = match fs::metadata(existing) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(SetupError::Read {
                path: existing.to_path_buf(),
                source,
            });
        }
    };
    fs::set_permissions(temporary, metadata.permissions()).map_err(|source| SetupError::Write {
        path: temporary.to_path_buf(),
        source,
    })
}
