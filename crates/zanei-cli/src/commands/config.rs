use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use zanei_core::config::{CaptureBoolKey, Config, apply_scalar_edit, save, save_capture_bool};

use super::EXIT_SUCCESS;
use crate::cli::{ConfigArgs, ConfigCommand};
use crate::error::CliError;

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
    config_path: &Path,
    store_path: &Path,
    args: ConfigArgs,
    quiet: bool,
) -> Result<u8, CliError> {
    match args.command {
        ConfigCommand::Init => init(config_path)?,
        ConfigCommand::Path => println!("{}", config_path.display()),
        ConfigCommand::Show => {
            let config = Config::load(config_path)?;
            print!("{}", toml::to_string_pretty(&config)?);
        }
        ConfigCommand::Edit => edit(config_path)?,
        ConfigCommand::Set { dotted_key, value } => {
            set(config_path, store_path, &dotted_key, &value, quiet)?;
        }
    }
    Ok(EXIT_SUCCESS)
}

fn set(
    config_path: &Path,
    store_path: &Path,
    dotted_key: &str,
    value: &str,
    quiet: bool,
) -> Result<(), CliError> {
    let (result, file_changed) = persist_scalar(config_path, dotted_key, value)?;

    if quiet {
        return Ok(());
    }
    if file_changed {
        println!("Updated {}", config_path.display());
    } else {
        println!("No change: {dotted_key}");
    }
    if result.changed && result.restart_recording && super::control::daemon_running(store_path)? {
        println!("Restart recording with `zanei stop && zanei start` for this to take effect.");
    }
    Ok(())
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
    Ok((result, file_changed))
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
    use zanei_core::config::Config;

    use super::{CONFIG_OPTION_COMMENTS, render_template};

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
}
