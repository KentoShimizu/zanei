//! LaunchAgent plist rendering and atomic installation.

use std::{
    env,
    ffi::CString,
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
};

use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, open};

use super::{DaemonError, LABEL, LAUNCHCTL, PLIST_FILE_NAME, command_succeeded, gui_domain};

const STANDARD_OUT_LOG_SUFFIX: &str = ".daemon.stdout.log";
const STANDARD_ERROR_LOG_SUFFIX: &str = ".daemon.stderr.log";
const LOG_FILE_MODE: Mode = Mode::RUSR.union(Mode::WUSR);

// Design limit: logs are unbounded; add rotation when either file exceeds 100 MiB.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct LaunchAgentPaths {
    pub(super) store: PathBuf,
    pub(super) standard_out: PathBuf,
    pub(super) standard_error: PathBuf,
}

pub fn launch_agent_path() -> Result<PathBuf, DaemonError> {
    let home = env::var_os("HOME").ok_or(DaemonError::MissingEnvironment { name: "HOME" })?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(PLIST_FILE_NAME))
}

/// Renders the launch agent, including an active file-key override because
/// launchd does not inherit the invoking shell's environment.
pub(super) fn render_launch_agent_plist(
    executable: &Path,
    config_path: &Path,
    store_key_file: Option<&Path>,
    paths: &LaunchAgentPaths,
) -> String {
    let executable = xml_escape(&executable.to_string_lossy());
    let config_path = xml_escape(&config_path.to_string_lossy());
    let store_path = xml_escape(&paths.store.to_string_lossy());
    let standard_out_path = xml_escape(&paths.standard_out.to_string_lossy());
    let standard_error_path = xml_escape(&paths.standard_error.to_string_lossy());
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
  <key>StandardOutPath</key>
  <string>{standard_out_path}</string>
  <key>StandardErrorPath</key>
  <string>{standard_error_path}</string>
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
    paths: &LaunchAgentPaths,
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
        crate::store_access::key_file_override().as_deref(),
        paths,
    ))
}

pub(super) fn launch_agent_paths(store_path: &Path) -> Result<LaunchAgentPaths, DaemonError> {
    let store = std::path::absolute(store_path).map_err(|source| DaemonError::File {
        operation: "resolve absolute path for",
        path: store_path.to_owned(),
        source,
    })?;
    Ok(LaunchAgentPaths {
        standard_out: with_file_name_suffix(&store, STANDARD_OUT_LOG_SUFFIX),
        standard_error: with_file_name_suffix(&store, STANDARD_ERROR_LOG_SUFFIX),
        store,
    })
}

fn with_file_name_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push(suffix);
    path.into()
}

pub(super) fn prepare_launch_agent_logs(paths: &LaunchAgentPaths) -> Result<(), DaemonError> {
    crate::daemon::runtime_support::ensure_store_parent(&paths.store)?;
    prepare_log_file(&paths.standard_out)?;
    prepare_log_file(&paths.standard_error)
}

fn prepare_log_file(path: &Path) -> Result<(), DaemonError> {
    let path_argument =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| DaemonError::File {
            operation: "create",
            path: path.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path contains a NUL byte",
            ),
        })?;
    let flags =
        OFlags::CREATE | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let file = open(path_argument.as_c_str(), flags, LOG_FILE_MODE).map_err(|source| {
        DaemonError::File {
            operation: "create",
            path: path.to_owned(),
            source: std::io::Error::from_raw_os_error(source.raw_os_error()),
        }
    })?;
    let metadata = fstat(&file).map_err(|source| DaemonError::File {
        operation: "inspect",
        path: path.to_owned(),
        source: std::io::Error::from_raw_os_error(source.raw_os_error()),
    })?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        return Err(DaemonError::File {
            operation: "use as a launchd log",
            path: path.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path is not a regular file",
            ),
        });
    }
    fchmod(&file, LOG_FILE_MODE).map_err(|source| DaemonError::File {
        operation: "restrict the permissions of",
        path: path.to_owned(),
        source: std::io::Error::from_raw_os_error(source.raw_os_error()),
    })?;
    Ok(())
}

pub fn bootstrap(
    executable: &Path,
    config_path: &Path,
    store_path: &Path,
) -> Result<(), DaemonError> {
    let paths = launch_agent_paths(store_path)?;
    let plist = build_launch_agent_plist(executable, config_path, &paths)?;
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
    prepare_launch_agent_logs(&paths)?;
    let temporary_path = plist_path.with_extension("plist.tmp");
    fs::write(&temporary_path, plist).map_err(|source| DaemonError::File {
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
