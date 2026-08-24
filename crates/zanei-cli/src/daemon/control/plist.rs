//! LaunchAgent plist rendering and atomic installation.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use super::{DaemonError, LABEL, LAUNCHCTL, PLIST_FILE_NAME, command_succeeded, gui_domain};

pub fn launch_agent_path() -> Result<PathBuf, DaemonError> {
    let home = env::var_os("HOME").ok_or(DaemonError::MissingEnvironment { name: "HOME" })?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(PLIST_FILE_NAME))
}

/// Renders the launch agent, including an active file-key override because
/// launchd does not inherit the invoking shell's environment.
pub fn render_launch_agent_plist(
    executable: &Path,
    config_path: &Path,
    store_path: &Path,
    store_key_file: Option<&Path>,
) -> String {
    let executable = xml_escape(&executable.to_string_lossy());
    let config_path = xml_escape(&config_path.to_string_lossy());
    let store_path = xml_escape(&store_path.to_string_lossy());
    let environment = store_key_file.map_or_else(String::new, |path| {
        format!(
            "  <key>EnvironmentVariables</key>\n  <dict>\n    <key>{}</key>\n    <string>{}</string>\n  </dict>\n",
            crate::store_access::STORE_KEY_FILE_ENV,
            xml_escape(&path.to_string_lossy())
        )
    });
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{executable}</string>
    <string>__daemon</string>
    <string>--config</string>
    <string>{config_path}</string>
    <string>--store</string>
    <string>{store_path}</string>
  </array>
{environment}  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
</dict>
</plist>
"#
    )
}

pub(super) fn build_launch_agent_plist(
    executable: &Path,
    config_path: &Path,
    store_path: &Path,
) -> Result<String, DaemonError> {
    let executable =
        crate::executable::canonicalize(executable).map_err(|source| DaemonError::File {
            operation: "canonicalize",
            path: executable.to_path_buf(),
            source,
        })?;
    Ok(render_launch_agent_plist(
        &executable,
        config_path,
        store_path,
        crate::store_access::key_file_override().as_deref(),
    ))
}

pub fn bootstrap(
    executable: &Path,
    config_path: &Path,
    store_path: &Path,
) -> Result<(), DaemonError> {
    let plist_path = launch_agent_path()?;
    let parent = plist_path.parent().ok_or_else(|| DaemonError::File {
        operation: "resolve parent directory for",
        path: plist_path.clone(),
        source: std::io::Error::other("launch agent path has no parent"),
    })?;
    fs::create_dir_all(parent).map_err(|source| DaemonError::File {
        operation: "create directory for",
        path: plist_path.clone(),
        source,
    })?;
    let temporary_path = plist_path.with_extension("plist.tmp");
    fs::write(
        &temporary_path,
        build_launch_agent_plist(executable, config_path, store_path)?,
    )
    .map_err(|source| DaemonError::File {
        operation: "write",
        path: temporary_path.clone(),
        source,
    })?;
    fs::rename(&temporary_path, &plist_path).map_err(|source| DaemonError::File {
        operation: "install",
        path: plist_path.clone(),
        source,
    })?;

    let domain = gui_domain()?;
    let output = Command::new(LAUNCHCTL)
        .args(["bootstrap", &domain])
        .arg(&plist_path)
        .output()
        .map_err(|source| DaemonError::CommandLaunch {
            program: LAUNCHCTL,
            operation: "bootstrap the Zanei launch agent",
            source,
        })?;
    let result = command_succeeded(LAUNCHCTL, "bootstrap the Zanei launch agent", output);
    if result.is_err() {
        let _ = fs::remove_file(plist_path);
    }
    result
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
