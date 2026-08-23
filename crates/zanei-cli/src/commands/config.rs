use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

use zanei_collector::AppDirectory;
use zanei_core::config::{
    CaptureBoolKey, Config, FilterScope, ScalarEditResult, apply_scalar_edit, save,
    save_capture_bool,
};

use super::apps;
use super::filter::ScopeSummary;
use super::{EXIT_SUCCESS, EXIT_USAGE};
use crate::cli::{ConfigArgs, ConfigCommand};
use crate::error::CliError;
use crate::paths::Paths;

const CONTENT_SNAPSHOT_WARNING_PREFIX: &str = concat!(
    "Content snapshots record the text shown in the frontmost window, including messages and\n",
    "documents written by other people and text you typed that is on screen. Password fields and\n",
    "Chrome Incognito windows are never captured; stored text is redacted and deleted after "
);
const CONTENT_SNAPSHOT_WARNING_SUFFIX: &str =
    " hours.\n\nCurrent scope (change it first if this is not what you want):\n";
const CONTENT_SNAPSHOT_ACTIONS: &str = concat!(
    "  zanei filter content-snapshot only-app add <APP>      record only these apps\n",
    "  zanei filter content-snapshot exclude-app add <APP>   everything except these\n",
    "  zanei apps                                            list apps to choose from\n\n",
    "Enable content snapshots with this scope? [y/N]\n"
);

const CONFIG_OPTION_COMMENTS: [ConfigOptionComment; 18] = [
    ConfigOptionComment {
        section: "capture",
        key: "sources",
        description: "Event families to capture: app, window, ui, input, and browser.",
    },
    ConfigOptionComment {
        section: "capture",
        key: "text_content",
        description: "Capture typed, field, and clipboard content (explicit opt-in).",
    },
    ConfigOptionComment {
        section: "capture",
        key: "content_snapshot",
        description: "Capture frontmost-window Accessibility text (explicit opt-in).",
    },
    ConfigOptionComment {
        section: "filter",
        key: "exclude_apps",
        description: "Apps denied before storage; bundle identifiers are recommended.",
    },
    ConfigOptionComment {
        section: "filter",
        key: "include_only_apps",
        description: "When non-empty, capture only these apps.",
    },
    ConfigOptionComment {
        section: "filter",
        key: "exclude_websites",
        description: "Website hosts denied for Chrome URL events and text-content bodies.",
    },
    ConfigOptionComment {
        section: "filter",
        key: "include_only_websites",
        description: "When non-empty, capture Chrome URL events and text-content bodies only for these hosts.",
    },
    ConfigOptionComment {
        section: "filter",
        key: "redactors",
        description: "Redactors applied to captured values: email, credit_card, and token.",
    },
    ConfigOptionComment {
        section: "filter.text_content",
        key: "exclude_apps",
        description: "Apps whose typed and clipboard bodies stay null.",
    },
    ConfigOptionComment {
        section: "filter.text_content",
        key: "include_only_apps",
        description: "When non-empty, retain typed bodies only for these apps.",
    },
    ConfigOptionComment {
        section: "filter.text_content",
        key: "exclude_websites",
        description: "Chrome hosts whose typed and clipboard bodies stay null.",
    },
    ConfigOptionComment {
        section: "filter.text_content",
        key: "include_only_websites",
        description: "When non-empty, retain typed bodies only for these Chrome hosts.",
    },
    ConfigOptionComment {
        section: "filter.content_snapshot",
        key: "exclude_apps",
        description: "Apps where content snapshots are not created.",
    },
    ConfigOptionComment {
        section: "filter.content_snapshot",
        key: "include_only_apps",
        description: "When non-empty, create content snapshots only for these apps.",
    },
    ConfigOptionComment {
        section: "filter.content_snapshot",
        key: "exclude_websites",
        description: "Chrome hosts where content snapshots are not created.",
    },
    ConfigOptionComment {
        section: "filter.content_snapshot",
        key: "include_only_websites",
        description: "When non-empty, create snapshots only for these Chrome hosts.",
    },
    ConfigOptionComment {
        section: "output",
        key: "batch_interval_s",
        description: "Flush interval in seconds; must be greater than zero.",
    },
    ConfigOptionComment {
        section: "output",
        key: "retention_hours",
        description: "Delete events older than this many hours; must be greater than zero.",
    },
];

#[derive(Clone, Copy)]
struct ConfigOptionComment {
    section: &'static str,
    key: &'static str,
    description: &'static str,
}

pub fn run(
    paths: &Paths,
    app_directory: &dyn AppDirectory,
    args: ConfigArgs,
    quiet: bool,
) -> Result<u8, CliError> {
    match args.command {
        ConfigCommand::Init => init(&paths.config)?,
        ConfigCommand::Path => println!("{}", paths.config.display()),
        ConfigCommand::Show => {
            let config = Config::load(&paths.config)?;
            print!("{}", toml::to_string_pretty(&config)?);
        }
        ConfigCommand::Edit => edit(&paths.config)?,
        ConfigCommand::Set { dotted_key, value } => {
            return set(paths, app_directory, &dotted_key, &value, quiet);
        }
    }
    Ok(EXIT_SUCCESS)
}

fn set(
    paths: &Paths,
    app_directory: &dyn AppDirectory,
    dotted_key: &str,
    value: &str,
    quiet: bool,
) -> Result<u8, CliError> {
    let config = Config::load(&paths.config)?;
    let result = apply_scalar_edit(&config, dotted_key, value)?;
    if dotted_key == "capture.content_snapshot"
        && result.changed
        && result.config.capture.content_snapshot
        && !quiet
    {
        let candidates = apps::collect(paths, app_directory)?.apps;
        let summary = ScopeSummary::for_scope(&config, FilterScope::ContentSnapshot, &candidates);
        match confirm_content_snapshot(&summary, config.output.retention_hours, false)? {
            EnableDecision::Persist => {}
            EnableDecision::Cancel => return Ok(EXIT_SUCCESS),
            EnableDecision::UsageError => return Ok(EXIT_USAGE),
        }
    }
    let file_changed = persist_scalar_result(&paths.config, dotted_key, &result)?;

    if quiet {
        return Ok(EXIT_SUCCESS);
    }
    if file_changed {
        println!("Updated {}", paths.config.display());
    } else {
        println!("No change: {dotted_key}");
    }
    if result.changed && result.restart_recording && super::control::daemon_running(&paths.store)? {
        println!("Restart recording with `zanei stop && zanei start` for this to take effect.");
    }
    Ok(EXIT_SUCCESS)
}

pub(super) fn persist_capture_text_content(
    config_path: &Path,
    enabled: bool,
) -> Result<(), CliError> {
    persist_scalar(
        config_path,
        "capture.text_content",
        if enabled { "true" } else { "false" },
    )
    .map(|_| ())
}

fn persist_scalar(
    config_path: &Path,
    dotted_key: &str,
    value: &str,
) -> Result<(zanei_core::config::ScalarEditResult, bool), CliError> {
    let config = Config::load(config_path)?;
    let result = apply_scalar_edit(&config, dotted_key, value)?;
    let file_changed = persist_scalar_result(config_path, dotted_key, &result)?;
    Ok((result, file_changed))
}

fn persist_scalar_result(
    config_path: &Path,
    dotted_key: &str,
    result: &ScalarEditResult,
) -> Result<bool, CliError> {
    let capture_bool = match dotted_key {
        "capture.text_content" => Some(CaptureBoolKey::TextContent),
        "capture.content_snapshot" => Some(CaptureBoolKey::ContentSnapshot),
        _ => None,
    };
    let file_changed = if let Some(key) = capture_bool {
        save_capture_bool(&result.config, config_path, key)?
    } else if result.changed {
        save(&result.config, config_path)?;
        true
    } else {
        false
    };
    Ok(file_changed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnableDecision {
    Persist,
    Cancel,
    UsageError,
}

fn decide_enable(quiet: bool, terminal: bool, answer: Option<&str>) -> EnableDecision {
    if quiet {
        EnableDecision::Persist
    } else if !terminal {
        EnableDecision::UsageError
    } else if answer.is_some_and(|answer| matches!(answer.trim(), "y" | "Y")) {
        EnableDecision::Persist
    } else {
        EnableDecision::Cancel
    }
}

fn confirm_content_snapshot(
    summary: &ScopeSummary,
    retention_hours: u64,
    quiet: bool,
) -> Result<EnableDecision, CliError> {
    let stdin_is_terminal = io::stdin().is_terminal();
    let stderr_is_terminal = io::stderr().is_terminal();
    let mut stderr = io::stderr().lock();
    confirm_content_snapshot_with(
        summary,
        retention_hours,
        quiet,
        stdin_is_terminal,
        stderr_is_terminal,
        || {
            let mut answer = String::new();
            io::stdin().read_line(&mut answer).map(|_| answer)
        },
        |message| {
            stderr.write_all(message.as_bytes())?;
            stderr.flush()
        },
    )
}

fn confirm_content_snapshot_with(
    summary: &ScopeSummary,
    retention_hours: u64,
    quiet: bool,
    stdin_is_terminal: bool,
    stderr_is_terminal: bool,
    read_answer: impl FnOnce() -> io::Result<String>,
    mut write_stderr: impl FnMut(&str) -> io::Result<()>,
) -> Result<EnableDecision, CliError> {
    if quiet {
        return Ok(EnableDecision::Persist);
    }
    write_stderr(&render_content_snapshot_prompt(summary, retention_hours))
        .map_err(CliError::PromptOutput)?;
    let terminal = stdin_is_terminal && stderr_is_terminal;
    if !terminal {
        return Ok(EnableDecision::UsageError);
    }
    let answer = read_answer().map_err(CliError::Input)?;
    Ok(decide_enable(false, true, Some(&answer)))
}

fn render_content_snapshot_prompt(summary: &ScopeSummary, retention_hours: u64) -> String {
    format!(
        "{CONTENT_SNAPSHOT_WARNING_PREFIX}{retention_hours}{CONTENT_SNAPSHOT_WARNING_SUFFIX}  Apps:  {}\n  Sites: {}\n{CONTENT_SNAPSHOT_ACTIONS}",
        summary.prompt_apps(),
        summary.prompt_sites(),
    )
}

fn init(config_path: &Path) -> Result<(), CliError> {
    let template = render_template()?;
    let parent = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| CliError::io(parent, source))?;

    let mut file = match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(config_path)
    {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(CliError::ConfigAlreadyExists(config_path.to_path_buf()));
        }
        Err(source) => return Err(CliError::io(config_path, source)),
    };

    if let Err(source) = file
        .write_all(template.as_bytes())
        .and_then(|()| file.sync_all())
    {
        drop(file);
        return Err(remove_partial_config(config_path, source));
    }

    println!("Created configuration at {}", config_path.display());
    Ok(())
}

fn remove_partial_config(config_path: &Path, source: std::io::Error) -> CliError {
    match fs::remove_file(config_path) {
        Ok(()) => CliError::io(config_path, source),
        Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => {
            CliError::io(config_path, source)
        }
        Err(cleanup) => CliError::ConfigInitializationCleanup {
            path: config_path.to_path_buf(),
            source,
            cleanup,
        },
    }
}

fn render_template() -> Result<String, CliError> {
    let serialized = toml::to_string_pretty(&Config::default())?;
    let mut rendered = String::with_capacity(serialized.len());
    let mut used = [false; CONFIG_OPTION_COMMENTS.len()];
    let mut section = "";

    for line in serialized.lines() {
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            section = name;
        }
        if let Some((key, _)) = line.split_once(" = ") {
            let Some((index, option)) = CONFIG_OPTION_COMMENTS
                .iter()
                .enumerate()
                .find(|(_, option)| option.section == section && option.key == key)
            else {
                return Err(CliError::ConfigTemplateOutOfSync(format!(
                    "{section}.{key}"
                )));
            };
            rendered.push_str("# ");
            rendered.push_str(option.description);
            rendered.push('\n');
            used[index] = true;
        }
        rendered.push_str(line);
        rendered.push('\n');
    }

    if let Some((_, option)) = used
        .iter()
        .zip(CONFIG_OPTION_COMMENTS)
        .find(|(used, _)| !**used)
    {
        return Err(CliError::ConfigTemplateOutOfSync(format!(
            "{}.{}",
            option.section, option.key
        )));
    }

    Ok(rendered)
}

fn edit(config_path: &Path) -> Result<(), CliError> {
    if !config_path.exists() {
        save(&Config::default(), config_path)?;
    }
    let editor = std::env::var_os("EDITOR").ok_or(CliError::MissingEnvironment("EDITOR"))?;
    let status = Command::new(editor)
        .arg(config_path)
        .status()
        .map_err(CliError::Input)?;
    if !status.success() {
        return Err(CliError::EditorFailed(status));
    }
    Config::load(config_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;

    use tempfile::TempDir;
    use zanei_core::config::Config;

    use super::*;

    #[test]
    fn template_contains_every_option_comment_and_current_defaults() {
        let template = render_template().expect("configuration template should render");

        for option in CONFIG_OPTION_COMMENTS {
            assert!(template.contains(&format!("# {}\n{} =", option.description, option.key)));
        }
        assert_eq!(
            Config::from_toml(&template).expect("template should parse"),
            Config::default()
        );
        assert!(template.contains(
            "# Website hosts denied for Chrome URL events and text-content bodies.\nexclude_websites ="
        ));
        assert!(template.contains(
            "# When non-empty, capture Chrome URL events and text-content bodies only for these hosts.\ninclude_only_websites ="
        ));
    }

    #[test]
    fn content_snapshot_prompt_matches_the_documented_default_scope() {
        let prompt = render_content_snapshot_prompt(&default_content_summary(), 72);

        assert_eq!(
            prompt,
            concat!(
                "Content snapshots record the text shown in the frontmost window, including messages and\n",
                "documents written by other people and text you typed that is on screen. Password fields and\n",
                "Chrome Incognito windows are never captured; stored text is redacted and deleted after 72 hours.\n\n",
                "Current scope (change it first if this is not what you want):\n",
                "  Apps:  every app except 6 excluded (Safari, Firefox, Brave, Edge, Vivaldi, Arc)\n",
                "  Sites: every site\n",
                "  zanei filter content-snapshot only-app add <APP>      record only these apps\n",
                "  zanei filter content-snapshot exclude-app add <APP>   everything except these\n",
                "  zanei apps                                            list apps to choose from\n\n",
                "Enable content snapshots with this scope? [y/N]\n"
            )
        );
    }

    #[test]
    fn content_snapshot_prompt_decision_covers_tty_answers_non_tty_and_quiet() {
        for (answer, expected) in [
            ("y\n", EnableDecision::Persist),
            ("Y\n", EnableDecision::Persist),
            ("n\n", EnableDecision::Cancel),
            ("\n", EnableDecision::Cancel),
            ("", EnableDecision::Cancel),
        ] {
            let output = RefCell::new(String::new());
            let decision = confirm_content_snapshot_with(
                &default_content_summary(),
                48,
                false,
                true,
                true,
                || Ok(answer.to_owned()),
                |message| {
                    output.borrow_mut().push_str(message);
                    Ok(())
                },
            )
            .expect("TTY prompt decision");
            assert_eq!(decision, expected);
            assert!(output.into_inner().contains("Enable content snapshots"));
        }

        let read = Cell::new(false);
        let output = RefCell::new(String::new());
        let non_tty = confirm_content_snapshot_with(
            &default_content_summary(),
            48,
            false,
            false,
            true,
            || {
                read.set(true);
                Ok("y\n".to_owned())
            },
            |message| {
                output.borrow_mut().push_str(message);
                Ok(())
            },
        )
        .expect("non-TTY decision");
        assert_eq!(non_tty, EnableDecision::UsageError);
        assert!(!read.get());
        assert!(output.into_inner().contains("Current scope"));

        let wrote = Cell::new(false);
        let quiet = confirm_content_snapshot_with(
            &default_content_summary(),
            48,
            true,
            false,
            false,
            || Ok("n\n".to_owned()),
            |_| {
                wrote.set(true);
                Ok(())
            },
        )
        .expect("quiet decision");
        assert_eq!(quiet, EnableDecision::Persist);
        assert!(!wrote.get());
    }

    #[test]
    fn negative_enter_and_eof_leave_config_bytes_unchanged() {
        for answer in ["n\n", "\n", ""] {
            let directory = TempDir::new().expect("temporary config directory");
            let path = directory.path().join("config.toml");
            fs::write(
                &path,
                "# retained comment\n[capture]\ncontent_snapshot = false\n",
            )
            .expect("prompt config fixture");
            let before = fs::read(&path).expect("config before prompt");
            let config = Config::load(&path).expect("load prompt config");
            let result = apply_scalar_edit(&config, "capture.content_snapshot", "true")
                .expect("prepare content snapshot edit");
            let decision = confirm_content_snapshot_with(
                &default_content_summary(),
                48,
                false,
                true,
                true,
                || Ok(answer.to_owned()),
                |_| Ok(()),
            )
            .expect("injected prompt");

            if decision == EnableDecision::Persist {
                persist_scalar_result(&path, "capture.content_snapshot", &result)
                    .expect("persist accepted edit");
            }
            assert_eq!(
                fs::read(&path).expect("config after prompt"),
                before,
                "answer {answer:?} must not write"
            );
        }
    }

    fn default_content_summary() -> ScopeSummary {
        ScopeSummary::for_scope(&Config::default(), FilterScope::ContentSnapshot, &[])
    }
}
