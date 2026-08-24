//! LaunchAgent plist rendering and atomic installation.

use std::{
    env,
    ffi::CString,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::{self, Command},
    sync::atomic::{AtomicU64, Ordering},
};

use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, open};

use super::{DaemonError, ID, LABEL, LAUNCHCTL, PLIST_FILE_NAME, command_succeeded, gui_domain};

const STANDARD_OUT_LOG_SUFFIX: &str = ".daemon.stdout.log";
const STANDARD_ERROR_LOG_SUFFIX: &str = ".daemon.stderr.log";
const LOG_FILE_MODE: Mode = Mode::RUSR.union(Mode::WUSR);
const TEMPORARY_PLIST_MODE: u32 = 0o600;
const GROUP_OR_WORLD_WRITE_MODE: u32 = 0o022;
const STICKY_MODE: u32 = 0o1000;
const ROOT_USER_ID: u32 = 0;
static TEMPORARY_PLIST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

// Design limit: logs are unbounded; add rotation when either file exceeds 100 MiB.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct LaunchAgentPaths {
    pub(super) store: String,
    pub(super) standard_out: String,
    pub(super) standard_error: String,
}

impl LaunchAgentPaths {
    fn new(store: String) -> Self {
        Self {
            standard_out: format!("{store}{STANDARD_OUT_LOG_SUFFIX}"),
            standard_error: format!("{store}{STANDARD_ERROR_LOG_SUFFIX}"),
            store,
        }
    }

    pub(super) fn store_path(&self) -> &Path {
        Path::new(&self.store)
    }
}

pub(super) struct PreparedLaunchAgent {
    plist_path: PathBuf,
    temporary_path: Option<PathBuf>,
    domain: String,
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
    let store_path = xml_escape(&paths.store);
    let standard_out_path = xml_escape(&paths.standard_out);
    let standard_error_path = xml_escape(&paths.standard_error);
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
    Ok(LaunchAgentPaths::new(validated_store_path(store)?))
}

fn validated_store_path(path: PathBuf) -> Result<String, DaemonError> {
    path.into_os_string()
        .into_string()
        .map_err(|path| DaemonError::File {
            operation: "use as a launchd store path",
            path: path.into(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path is not valid UTF-8; choose a store path containing only valid UTF-8",
            ),
        })
}

#[cfg(test)]
pub(super) fn prepare_launch_agent_logs(
    paths: &LaunchAgentPaths,
) -> Result<LaunchAgentPaths, DaemonError> {
    let domain = gui_domain()?;
    prepare_launch_agent_logs_for_user(paths, user_id_from_domain(&domain)?)
}

fn prepare_launch_agent_logs_for_user(
    paths: &LaunchAgentPaths,
    user_id: u32,
) -> Result<LaunchAgentPaths, DaemonError> {
    crate::daemon::runtime_support::ensure_store_parent(paths.store_path())?;
    let store_path = paths.store_path();
    let parent = store_path.parent().ok_or_else(|| DaemonError::File {
        operation: "resolve the launchd log directory for",
        path: store_path.to_owned(),
        source: std::io::Error::other("store path has no parent directory"),
    })?;
    let canonical_parent = fs::canonicalize(parent).map_err(|source| DaemonError::File {
        operation: "canonicalize the launchd log directory",
        path: parent.to_owned(),
        source,
    })?;
    validate_log_directory(&canonical_parent, user_id)?;
    let file_name = store_path.file_name().ok_or_else(|| DaemonError::File {
        operation: "use as a launchd store path",
        path: store_path.to_owned(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "store path must end with a file name",
        ),
    })?;
    let canonical = LaunchAgentPaths::new(validated_store_path(canonical_parent.join(file_name))?);
    prepare_log_file(Path::new(&canonical.standard_out))?;
    prepare_log_file(Path::new(&canonical.standard_error))?;
    Ok(canonical)
}

fn user_id_from_domain(domain: &str) -> Result<u32, DaemonError> {
    domain
        .strip_prefix("gui/")
        .and_then(|user_id| user_id.parse().ok())
        .ok_or_else(|| DaemonError::InvalidUserId {
            program: ID,
            output: domain.to_owned(),
        })
}

fn validate_log_directory(path: &Path, user_id: u32) -> Result<(), DaemonError> {
    let mut directories: Vec<_> = path.ancestors().collect();
    directories.reverse();
    for directory in directories {
        let metadata = fs::symlink_metadata(directory).map_err(|source| DaemonError::File {
            operation: "inspect the launchd log directory",
            path: directory.to_owned(),
            source,
        })?;
        if !metadata.file_type().is_dir() {
            return Err(log_directory_error(
                directory,
                std::io::ErrorKind::InvalidInput,
                "path component is not a directory; choose an owner-only store directory",
            ));
        }
        let owner = metadata.uid();
        let is_log_directory = directory == path;
        if is_log_directory && owner != user_id {
            return Err(log_directory_error(
                directory,
                std::io::ErrorKind::PermissionDenied,
                &format!(
                    "directory is owned by uid {owner}, but the launchd log directory must be owned by current uid {user_id}; change its owner or choose an owner-only store directory"
                ),
            ));
        }
        if !is_log_directory && owner != ROOT_USER_ID && owner != user_id {
            return Err(log_directory_error(
                directory,
                std::io::ErrorKind::PermissionDenied,
                &format!(
                    "ancestor is owned by untrusted uid {owner}; move the store below a directory owned by root or current uid {user_id}"
                ),
            ));
        }
        let mode = metadata.mode() & 0o7777;
        let writable_by_others = mode & GROUP_OR_WORLD_WRITE_MODE != 0;
        if is_log_directory && writable_by_others {
            return Err(log_directory_error(
                directory,
                std::io::ErrorKind::PermissionDenied,
                &format!(
                    "directory mode {mode:#06o} is group/world-writable; run `chmod go-w` on it or choose an owner-only store directory"
                ),
            ));
        }
        if !is_log_directory && writable_by_others && mode & STICKY_MODE == 0 {
            return Err(log_directory_error(
                directory,
                std::io::ErrorKind::PermissionDenied,
                &format!(
                    "ancestor mode {mode:#06o} lets other users replace path components because the sticky bit is not set; run `chmod go-w` on it or choose an owner-only store directory"
                ),
            ));
        }
    }
    Ok(())
}

fn log_directory_error(path: &Path, kind: std::io::ErrorKind, reason: &str) -> DaemonError {
    DaemonError::File {
        operation: "use as a launchd log directory",
        path: path.to_owned(),
        source: std::io::Error::new(kind, reason),
    }
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
        return Err(non_regular_log(path));
    }
    fchmod(&file, LOG_FILE_MODE).map_err(|source| DaemonError::File {
        operation: "restrict the permissions of",
        path: path.to_owned(),
        source: std::io::Error::from_raw_os_error(source.raw_os_error()),
    })?;
    Ok(())
}

fn non_regular_log(path: &Path) -> DaemonError {
    DaemonError::File {
        operation: "use as a launchd log",
        path: path.to_owned(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path is not a regular file; remove it before starting Zanei",
        ),
    }
}

pub(super) fn prepare_launch_agent(
    executable: &Path,
    config_path: &Path,
    store_path: &Path,
) -> Result<PreparedLaunchAgent, DaemonError> {
    let paths = launch_agent_paths(store_path)?;
    let domain = gui_domain()?;
    let paths = prepare_launch_agent_logs_for_user(&paths, user_id_from_domain(&domain)?)?;
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
    let temporary_path = parent.join(format!(
        ".{PLIST_FILE_NAME}.{}.{}.tmp",
        process::id(),
        TEMPORARY_PLIST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut temporary = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(TEMPORARY_PLIST_MODE)
        .open(&temporary_path)
        .map_err(|source| DaemonError::File {
            operation: "create prepared launch agent plist",
            path: temporary_path.clone(),
            source,
        })?;
    let prepared = PreparedLaunchAgent {
        plist_path,
        temporary_path: Some(temporary_path.clone()),
        domain,
    };
    temporary
        .write_all(plist.as_bytes())
        .map_err(|source| DaemonError::File {
            operation: "write prepared launch agent plist",
            path: temporary_path.clone(),
            source,
        })?;
    drop(temporary);
    Ok(prepared)
}

impl PreparedLaunchAgent {
    pub(super) fn install_and_bootstrap(&mut self) -> Result<(), DaemonError> {
        let temporary_path = self
            .temporary_path
            .take()
            .ok_or_else(|| DaemonError::File {
                operation: "install prepared launch agent plist at",
                path: self.plist_path.clone(),
                source: std::io::Error::other("prepared plist was already installed"),
            })?;
        if let Err(source) = fs::rename(&temporary_path, &self.plist_path) {
            self.temporary_path = Some(temporary_path);
            return Err(DaemonError::File {
                operation: "install",
                path: self.plist_path.clone(),
                source,
            });
        }

        let result = Command::new(LAUNCHCTL)
            .args(["bootstrap", &self.domain])
            .arg(&self.plist_path)
            .output()
            .map_err(|source| DaemonError::CommandLaunch {
                program: LAUNCHCTL,
                operation: "bootstrap the Zanei launch agent",
                source,
            })
            .and_then(|output| {
                command_succeeded(LAUNCHCTL, "bootstrap the Zanei launch agent", output)
            });
        if result.is_err() {
            let _ = fs::remove_file(&self.plist_path);
        }
        result
    }
}

impl Drop for PreparedLaunchAgent {
    fn drop(&mut self) {
        if let Some(path) = self.temporary_path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[allow(dead_code)]
pub fn bootstrap(
    executable: &Path,
    config_path: &Path,
    store_path: &Path,
) -> Result<(), DaemonError> {
    prepare_launch_agent(executable, config_path, store_path)?.install_and_bootstrap()
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs::{self, OpenOptions},
        os::unix::{
            ffi::OsStringExt,
            fs::{PermissionsExt, symlink},
        },
        path::Path,
        process::Command,
    };

    use tempfile::TempDir;

    use super::*;

    fn assert_file_error(error: DaemonError, operation: &str, path: &Path, reason: &str) {
        let (actual, actual_path, source) = match error {
            DaemonError::File {
                operation,
                path,
                source,
            } => (operation, path, source),
            error => panic!("expected file error, got {error}"),
        };
        assert_eq!(actual, operation);
        assert_eq!(actual_path, path);
        assert!(source.to_string().contains(reason));
    }

    fn canonical_sibling(path: &Path) -> PathBuf {
        fs::canonicalize(path.parent().expect("path parent"))
            .expect("canonical parent")
            .join(path.file_name().expect("file name"))
    }

    #[test]
    fn logs_reject_symbolic_links_and_non_regular_files() {
        let directory = TempDir::new().expect("log type fixture");
        let paths = launch_agent_paths(&directory.path().join("store/store.sqlite"))
            .expect("launch agent paths");
        fs::create_dir_all(Path::new(&paths.standard_out).parent().expect("log parent"))
            .expect("create log directory");
        let target = directory.path().join("unrelated.txt");
        fs::write(&target, b"must remain unchanged").expect("write symlink target");
        symlink(&target, &paths.standard_out).expect("create log symlink");

        let error = prepare_launch_agent_logs(&paths).expect_err("log symlink must fail");
        assert_file_error(
            error,
            "create",
            &canonical_sibling(Path::new(&paths.standard_out)),
            "",
        );
        assert_eq!(
            fs::read(&target).expect("read target"),
            b"must remain unchanged"
        );

        fs::remove_file(&paths.standard_out).expect("remove symlink");
        assert!(
            Command::new("/usr/bin/mkfifo")
                .arg(&paths.standard_out)
                .status()
                .expect("run mkfifo")
                .success()
        );
        let _reader = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&paths.standard_out)
            .expect("open FIFO reader");
        let error = prepare_launch_agent_logs(&paths).expect_err("FIFO must fail");
        assert_file_error(
            error,
            "use as a launchd log",
            &canonical_sibling(Path::new(&paths.standard_out)),
            "not a regular file",
        );
    }

    #[test]
    fn paths_reject_nul_and_non_utf8_store_paths() {
        let directory = TempDir::new().expect("invalid path fixture");
        let non_utf8 = directory
            .path()
            .join(OsString::from_vec(b"store-\xff.sqlite".to_vec()));
        let error = launch_agent_paths(&non_utf8).expect_err("non-UTF-8 must fail");
        assert_file_error(
            error,
            "use as a launchd store path",
            &non_utf8,
            "not valid UTF-8",
        );

        let nul = directory
            .path()
            .join(OsString::from_vec(b"store\0.sqlite".to_vec()));
        let paths = launch_agent_paths(&nul).expect("NUL remains until file open");
        let error = prepare_launch_agent_logs(&paths).expect_err("NUL must fail");
        assert_file_error(
            error,
            "create",
            &canonical_sibling(Path::new(&paths.standard_out)),
            "path contains a NUL byte",
        );
    }

    #[test]
    fn logs_reject_unsafe_parent_and_ancestor_directories() {
        for mode in [0o720, 0o702] {
            let directory = TempDir::new().expect("unsafe parent fixture");
            let parent = directory.path().join("logs");
            fs::create_dir(&parent).expect("create log parent");
            fs::set_permissions(&parent, fs::Permissions::from_mode(mode)).expect("set mode");
            let paths = launch_agent_paths(&parent.join("store.sqlite")).expect("log paths");
            let error = prepare_launch_agent_logs(&paths).expect_err("unsafe parent must fail");
            assert_file_error(
                error,
                "use as a launchd log directory",
                &fs::canonicalize(&parent).expect("canonical parent"),
                "chmod go-w",
            );
        }

        let directory = TempDir::new().expect("unsafe ancestor fixture");
        let ancestor = directory.path().join("shared");
        let parent = ancestor.join("private");
        fs::create_dir_all(&parent).expect("create nested log parent");
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o777)).expect("set ancestor");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).expect("set parent");
        let paths = launch_agent_paths(&parent.join("store.sqlite")).expect("log paths");
        let error = prepare_launch_agent_logs(&paths).expect_err("unsafe ancestor must fail");
        assert_file_error(
            error,
            "use as a launchd log directory",
            &fs::canonicalize(&ancestor).expect("canonical ancestor"),
            "sticky bit",
        );
    }

    #[test]
    fn logs_and_plist_share_the_canonical_parent() {
        let directory = TempDir::new().expect("symlinked parent fixture");
        let target = directory.path().join("target");
        fs::create_dir(&target).expect("create target");
        let alias = directory.path().join("alias");
        symlink(&target, &alias).expect("create alias");
        let raw = launch_agent_paths(&alias.join("store.sqlite")).expect("raw paths");
        let paths = prepare_launch_agent_logs(&raw).expect("canonical paths");
        let expected = fs::canonicalize(target)
            .expect("canonical target")
            .join("store.sqlite");
        assert_eq!(paths.store_path(), expected);
        let plist = render_launch_agent_plist(
            Path::new("/Applications/Zanei.app/Contents/MacOS/zanei"),
            Path::new("/tmp/config.toml"),
            None,
            &paths,
        );
        assert!(plist.contains(expected.to_str().expect("UTF-8 target")));
        assert!(!plist.contains(alias.to_str().expect("UTF-8 alias")));
    }
}
