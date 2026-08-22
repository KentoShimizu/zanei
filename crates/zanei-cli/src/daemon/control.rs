use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

use zanei_core::store::StoreStatus;

use crate::store_access::KeyPrompt;

use super::{DaemonError, StoreOwner, StoreOwnership};

const LABEL: &str = "dev.zanei.agent";
const PLIST_FILE_NAME: &str = "dev.zanei.agent.plist";
const LAUNCHCTL: &str = "/bin/launchctl";
const ID: &str = "/usr/bin/id";
const KILL: &str = "/bin/kill";
pub(crate) const DAEMON_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const DAEMON_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub fn launch_agent_path() -> Result<PathBuf, DaemonError> {
    let home = env::var_os("HOME").ok_or(DaemonError::MissingEnvironment { name: "HOME" })?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(PLIST_FILE_NAME))
}

pub fn render_launch_agent_plist(
    executable: &Path,
    config_path: &Path,
    store_path: &Path,
) -> String {
    let executable = xml_escape(&executable.to_string_lossy());
    let config_path = xml_escape(&config_path.to_string_lossy());
    let store_path = xml_escape(&store_path.to_string_lossy());
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
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
</dict>
</plist>
"#
    )
}

fn build_launch_agent_plist(
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

pub fn start_launch_agent(
    executable: &Path,
    config_path: &Path,
    store_path: &Path,
) -> Result<bool, DaemonError> {
    let registered = is_bootstrapped()?;
    start_launch_agent_with(
        registered,
        DAEMON_CONTROL_TIMEOUT,
        bootout,
        is_bootstrapped,
        thread::sleep,
        || bootstrap(executable, config_path, store_path),
        || daemon_is_alive(store_path),
    )
}

pub fn bootout() -> Result<(), DaemonError> {
    let service = service_target()?;
    let output = Command::new(LAUNCHCTL)
        .args(["bootout", &service])
        .output()
        .map_err(|source| DaemonError::CommandLaunch {
            program: LAUNCHCTL,
            operation: "boot out the Zanei launch agent",
            source,
        })?;
    command_succeeded(LAUNCHCTL, "boot out the Zanei launch agent", output)?;

    let plist_path = launch_agent_path()?;
    match fs::remove_file(&plist_path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DaemonError::File {
            operation: "remove",
            path: plist_path,
            source,
        }),
    }
}

pub fn is_bootstrapped() -> Result<bool, DaemonError> {
    let service = service_target()?;
    let output = Command::new(LAUNCHCTL)
        .args(["print", &service])
        .output()
        .map_err(|source| DaemonError::CommandLaunch {
            program: LAUNCHCTL,
            operation: "inspect the Zanei launch agent",
            source,
        })?;
    Ok(output.status.success())
}

pub fn wait_for_launch_agent_removal() -> Result<(), DaemonError> {
    wait_for_launch_agent_removal_with(DAEMON_CONTROL_TIMEOUT, is_bootstrapped, thread::sleep)
}

fn start_launch_agent_with(
    registered: bool,
    timeout: Duration,
    mut bootout: impl FnMut() -> Result<(), DaemonError>,
    is_registered: impl FnMut() -> Result<bool, DaemonError>,
    mut sleep: impl FnMut(Duration),
    mut bootstrap: impl FnMut() -> Result<(), DaemonError>,
    is_daemon_alive: impl FnMut() -> Result<bool, DaemonError>,
) -> Result<bool, DaemonError> {
    if registered {
        bootout()?;
        wait_for_launch_agent_removal_with(timeout, is_registered, &mut sleep)?;
    }
    bootstrap()?;
    wait_for_daemon_start_with(timeout, is_daemon_alive, sleep)?;
    Ok(registered)
}

fn daemon_is_alive(store_path: &Path) -> Result<bool, DaemonError> {
    let Some(owner) = StoreOwnership::probe(store_path)? else {
        return Ok(false);
    };
    // Ownership is acquired before store creation and migration. Until the reader can observe a
    // complete heartbeat, a store read failure is a not-ready observation for this bounded wait.
    let Ok(status) = crate::store_access::open_reader(store_path, KeyPrompt::Suppressed)
        .and_then(|reader| reader.status())
    else {
        return Ok(false);
    };
    Ok(owner_has_fresh_heartbeat(&owner, &status))
}

fn owner_has_fresh_heartbeat(owner: &StoreOwner, status: &StoreStatus) -> bool {
    status.running && status.instance_id.as_deref() == Some(owner.instance_id.as_str())
}

fn wait_for_daemon_start_with(
    timeout: Duration,
    is_daemon_alive: impl FnMut() -> Result<bool, DaemonError>,
    sleep: impl FnMut(Duration),
) -> Result<(), DaemonError> {
    wait_until_with(timeout, is_daemon_alive, sleep, |timeout_seconds| {
        DaemonError::DaemonDidNotStart { timeout_seconds }
    })
}

fn wait_for_launch_agent_removal_with(
    timeout: Duration,
    mut is_registered: impl FnMut() -> Result<bool, DaemonError>,
    sleep: impl FnMut(Duration),
) -> Result<(), DaemonError> {
    wait_until_with(
        timeout,
        || is_registered().map(|registered| !registered),
        sleep,
        |timeout_seconds| DaemonError::LaunchAgentStillLoaded { timeout_seconds },
    )
}

fn wait_until_with(
    timeout: Duration,
    mut condition: impl FnMut() -> Result<bool, DaemonError>,
    mut sleep: impl FnMut(Duration),
    timeout_error: impl FnOnce(u64) -> DaemonError,
) -> Result<(), DaemonError> {
    let deadline = Instant::now() + timeout;
    let mut first_poll = true;
    while first_poll || Instant::now() < deadline {
        first_poll = false;
        if condition()? {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        sleep(DAEMON_CONTROL_POLL_INTERVAL.min(remaining));
    }
    Err(timeout_error(timeout.as_secs()))
}

pub fn terminate(pid: u32) -> Result<(), DaemonError> {
    if pid == 0 {
        return Err(DaemonError::InvalidRecorderPid { pid });
    }
    let output = Command::new(KILL)
        .args(["-TERM", &pid.to_string()])
        .output()
        .map_err(|source| DaemonError::CommandLaunch {
            program: KILL,
            operation: "terminate the foreground Zanei recorder",
            source,
        })?;
    command_succeeded(KILL, "terminate the foreground Zanei recorder", output)
}

fn gui_domain() -> Result<String, DaemonError> {
    Ok(format!("gui/{}", user_id()?))
}

fn service_target() -> Result<String, DaemonError> {
    Ok(format!("{}/{LABEL}", gui_domain()?))
}

fn user_id() -> Result<u32, DaemonError> {
    let output =
        Command::new(ID)
            .arg("-u")
            .output()
            .map_err(|source| DaemonError::CommandLaunch {
                program: ID,
                operation: "resolve the current user ID",
                source,
            })?;
    if !output.status.success() {
        return Err(DaemonError::CommandFailed {
            program: ID,
            operation: "resolve the current user ID",
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let output = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    output.parse().map_err(|_| DaemonError::InvalidUserId {
        program: ID,
        output,
    })
}

fn command_succeeded(
    program: &'static str,
    operation: &'static str,
    output: Output,
) -> Result<(), DaemonError> {
    if output.status.success() {
        return Ok(());
    }
    Err(DaemonError::CommandFailed {
        program,
        operation,
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
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
        cell::{Cell, RefCell},
        collections::VecDeque,
        fs,
        os::unix::fs::symlink,
        path::Path,
        time::Duration,
    };

    use tempfile::TempDir;
    use zanei_core::store::{DaemonMode, StoreStatus};

    use super::{
        DaemonError, StoreOwner, build_launch_agent_plist, owner_has_fresh_heartbeat,
        render_launch_agent_plist, start_launch_agent_with,
    };

    #[test]
    fn liveness_does_not_require_a_permission_snapshot() {
        let owner = StoreOwner {
            pid: 42,
            instance_id: "42@2026-08-17T10:00:00.000Z".to_owned(),
            mode: DaemonMode::Launchd,
            started_at: "2026-08-17T10:00:00.000Z".to_owned(),
        };
        let status = StoreStatus {
            running: true,
            instance_id: Some(owner.instance_id.clone()),
            permissions: None,
            ..StoreStatus::default()
        };

        assert!(owner_has_fresh_heartbeat(&owner, &status));
    }

    #[test]
    fn plist_uses_the_hidden_daemon_contract_and_escapes_paths() {
        let plist = render_launch_agent_plist(
            Path::new("/Applications/A&B/zanei"),
            Path::new("/tmp/<config>.toml"),
            Path::new("/tmp/store.sqlite"),
        );

        assert!(plist.contains("<string>__daemon</string>"));
        assert!(plist.contains("<string>--config</string>"));
        assert!(plist.contains("<string>--store</string>"));
        assert!(plist.contains("/Applications/A&amp;B/zanei"));
        assert!(plist.contains("/tmp/&lt;config&gt;.toml"));
        assert!(!plist.contains("A&B"));
    }

    #[test]
    fn installed_plist_uses_the_canonical_bundle_executable_and_current_label() {
        let directory = TempDir::new().expect("launch agent fixture");
        let executable = directory.path().join("Zanei.app/Contents/MacOS/zanei");
        fs::create_dir_all(executable.parent().expect("executable parent"))
            .expect("create app bundle fixture");
        fs::write(&executable, b"fixture").expect("write executable fixture");
        let canonical_executable =
            fs::canonicalize(&executable).expect("canonical executable fixture");
        let symlink_path = directory.path().join("bin/zanei");
        fs::create_dir_all(symlink_path.parent().expect("symlink parent"))
            .expect("create symlink directory");
        symlink(&executable, &symlink_path).expect("create executable symlink");

        let plist = build_launch_agent_plist(
            &symlink_path,
            Path::new("/tmp/config.toml"),
            Path::new("/tmp/store.sqlite"),
        )
        .expect("build launch agent plist");

        assert!(plist.contains("<string>dev.zanei.agent</string>"));
        assert!(plist.contains(&canonical_executable.to_string_lossy().into_owned()));
        assert!(!plist.contains(&symlink_path.to_string_lossy().into_owned()));
    }

    #[test]
    fn registered_agent_is_removed_before_it_is_bootstrapped_again() {
        let events = RefCell::new(Vec::new());
        let registered = RefCell::new(VecDeque::from([true, true, false]));

        let restarted = start_launch_agent_with(
            true,
            Duration::from_secs(1),
            || {
                events.borrow_mut().push("bootout");
                Ok(())
            },
            || {
                events.borrow_mut().push("print");
                Ok(registered
                    .borrow_mut()
                    .pop_front()
                    .expect("registered state"))
            },
            |_| {},
            || {
                events.borrow_mut().push("bootstrap");
                Ok(())
            },
            || {
                events.borrow_mut().push("alive");
                Ok(true)
            },
        )
        .expect("restart launch agent");

        assert!(restarted);
        assert_eq!(
            events.into_inner(),
            ["bootout", "print", "print", "print", "bootstrap", "alive"]
        );
    }

    #[test]
    fn registered_agent_timeout_does_not_attempt_bootstrap() {
        let bootstrap_called = Cell::new(false);

        let error = start_launch_agent_with(
            true,
            Duration::ZERO,
            || Ok(()),
            || Ok(true),
            |_| panic!("timeout must not sleep after the deadline"),
            || {
                bootstrap_called.set(true);
                Ok(())
            },
            || panic!("readiness must not be checked before bootstrap"),
        )
        .expect_err("registered launch agent must time out");

        assert!(error.to_string().contains("`zanei stop`"));
        assert!(error.to_string().contains("`zanei start`"));
        assert!(matches!(
            error,
            DaemonError::LaunchAgentStillLoaded { timeout_seconds: 0 }
        ));
        assert!(!bootstrap_called.get());
    }

    #[test]
    fn daemon_start_succeeds_after_repeated_not_ready_probes() {
        let events = RefCell::new(Vec::new());
        let readiness = RefCell::new(VecDeque::from([false, false, true]));
        let sleeps = RefCell::new(Vec::new());

        let restarted = start_launch_agent_with(
            false,
            Duration::from_secs(1),
            || panic!("unregistered launch agent must not be booted out"),
            || panic!("unregistered launch agent must not be polled"),
            |duration| sleeps.borrow_mut().push(duration),
            || {
                events.borrow_mut().push("bootstrap");
                Ok(())
            },
            || {
                events.borrow_mut().push("probe");
                Ok(readiness.borrow_mut().pop_front().expect("readiness state"))
            },
        )
        .expect("start unregistered launch agent");

        assert!(!restarted);
        assert_eq!(
            events.into_inner(),
            ["bootstrap", "probe", "probe", "probe"]
        );
        assert_eq!(
            sleeps.into_inner(),
            [Duration::from_millis(100), Duration::from_millis(100)]
        );
    }

    #[test]
    fn daemon_start_timeout_keeps_the_bootstrapped_agent_registered() {
        let bootstrap_called = Cell::new(false);
        let bootout_called = Cell::new(false);

        let error = start_launch_agent_with(
            false,
            Duration::ZERO,
            || {
                bootout_called.set(true);
                Ok(())
            },
            || panic!("unregistered launch agent must not be polled"),
            |_| panic!("timeout must not sleep after the deadline"),
            || {
                bootstrap_called.set(true);
                Ok(())
            },
            || Ok(false),
        )
        .expect_err("daemon readiness must time out");

        let message = error.to_string();
        assert!(message.contains("`zanei status`"));
        assert!(message.contains("last exit code"));
        assert!(message.contains("launchctl print gui/$(id -u)/dev.zanei.agent"));
        assert!(message.contains("`zanei start --foreground`"));
        assert!(matches!(
            &error,
            DaemonError::DaemonDidNotStart { timeout_seconds: 0 }
        ));
        assert_eq!(crate::error::CliError::from(error).exit_code(), 1);
        assert!(bootstrap_called.get());
        assert!(!bootout_called.get());
    }
}
