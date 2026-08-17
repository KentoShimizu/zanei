use std::path::Path;
use std::thread;
use std::time::Instant;

use time::OffsetDateTime;
use zanei_core::config::{Config, parse_duration_expression};
use zanei_core::normalize::format_timestamp;
use zanei_core::store::{DaemonMode, StoreWriter};

use super::doctor::{StartPermissionState, require_recorder_for_start};
use super::{EXIT_MISSING_PERMISSIONS, EXIT_NO_DAEMON, EXIT_SUCCESS};
use crate::error::CliError;
use crate::paths::Paths;

const BACKGROUND_START_CONFIRMATION: &str = concat!(
    "Zanei recording started\n",
    "Registered as a launchd background item. macOS notifications and an entry in Login Items & Extensions are expected.\n",
    "For the bundled distribution, Accessibility lists Zanei. Input Monitoring may omit its row even when permission is granted; the recorder-reported `zanei doctor` result is authoritative.\n",
    "To manage a missing Input Monitoring row in System Settings, add Zanei.app with +; the bundled entry persists.\n",
    "A raw executable may not keep a manually added row; use the bundled distribution for stable permission management.\n",
    "Typed and clipboard content is not recorded by default. To opt in: zanei config set capture.text_content true"
);
const PENDING_PERMISSION_SNAPSHOT_GUIDANCE: &str = "recorder is still waiting for macOS permission dialogs — respond to them, then check `zanei doctor`";

pub fn start(paths: &Paths, foreground: bool, quiet: bool) -> Result<u8, CliError> {
    let config = Config::load(&paths.config)?;
    if foreground {
        crate::daemon::run_daemon(&paths.config, &paths.store, DaemonMode::Foreground)?;
        return Ok(EXIT_SUCCESS);
    }

    if let Some(owner) = crate::daemon::StoreOwnership::probe(&paths.store)? {
        return Err(crate::daemon::DaemonError::StoreOwned { pid: owner.pid }.into());
    }
    let executable = crate::executable::current().map_err(CliError::Input)?;
    start_background_with(
        quiet,
        || {
            crate::daemon::start_launch_agent(&executable, &paths.config, &paths.store)
                .map_err(CliError::from)
        },
        || require_recorder_for_start(&config, &paths.store, &executable),
        |message| println!("{message}"),
    )
}

fn start_background_with(
    quiet: bool,
    start_launch_agent: impl FnOnce() -> Result<bool, CliError>,
    recorder_permission_state: impl FnOnce() -> Result<StartPermissionState, CliError>,
    mut print: impl FnMut(&str),
) -> Result<u8, CliError> {
    let was_bootstrapped = start_launch_agent()?;
    match recorder_permission_state()? {
        StartPermissionState::PendingSnapshot => {
            print(PENDING_PERMISSION_SNAPSHOT_GUIDANCE);
            return Ok(EXIT_SUCCESS);
        }
        StartPermissionState::Missing => return Ok(EXIT_MISSING_PERMISSIONS),
        StartPermissionState::Ready => {}
    }
    if !quiet {
        if was_bootstrapped {
            print("Zanei recording restarted");
        } else {
            print(BACKGROUND_START_CONFIRMATION);
        }
    }
    Ok(EXIT_SUCCESS)
}

pub fn stop(store_path: &Path, quiet: bool) -> Result<u8, CliError> {
    let Some(target) = crate::daemon::StoreOwnership::probe(store_path)? else {
        if !quiet {
            eprintln!("Zanei daemon is not running");
        }
        return Ok(EXIT_NO_DAEMON);
    };
    confirm_target(store_path, &target)?;
    match target.mode {
        DaemonMode::Foreground => crate::daemon::terminate(target.pid)?,
        DaemonMode::Launchd => {
            if !crate::daemon::is_bootstrapped()? {
                return Err(crate::daemon::DaemonError::LaunchdRecorderNotRegistered {
                    instance_id: target.instance_id,
                }
                .into());
            }
            crate::daemon::bootout()?;
        }
    }
    wait_for_stop_completion(
        &target.mode,
        || wait_for_daemon_exit(store_path, &target.instance_id),
        || crate::daemon::wait_for_launch_agent_removal().map_err(CliError::from),
    )?;
    if !quiet {
        println!("Zanei recording stopped; stored data was kept");
    }
    Ok(EXIT_SUCCESS)
}

fn wait_for_stop_completion(
    mode: &DaemonMode,
    wait_for_daemon_exit: impl FnOnce() -> Result<(), CliError>,
    wait_for_launch_agent_removal: impl FnOnce() -> Result<(), CliError>,
) -> Result<(), CliError> {
    wait_for_daemon_exit()?;
    if matches!(mode, DaemonMode::Launchd) {
        wait_for_launch_agent_removal()?;
    }
    Ok(())
}

fn confirm_target(store_path: &Path, target: &crate::daemon::StoreOwner) -> Result<(), CliError> {
    let current = crate::daemon::StoreOwnership::probe(store_path)?;
    if current
        .as_ref()
        .is_some_and(|owner| owner.instance_id == target.instance_id)
    {
        return Ok(());
    }
    Err(crate::daemon::DaemonError::RecorderInstanceChanged {
        instance_id: target.instance_id.clone(),
    }
    .into())
}

fn wait_for_daemon_exit(store_path: &Path, instance_id: &str) -> Result<(), CliError> {
    let deadline = Instant::now() + crate::daemon::DAEMON_CONTROL_TIMEOUT;
    while Instant::now() < deadline {
        let owner = crate::daemon::StoreOwnership::probe(store_path)?;
        if owner
            .as_ref()
            .is_none_or(|owner| owner.instance_id != instance_id)
        {
            return Ok(());
        }
        thread::sleep(crate::daemon::DAEMON_CONTROL_POLL_INTERVAL);
    }
    Err(crate::daemon::DaemonError::RecorderStopTimeout {
        instance_id: instance_id.to_owned(),
    }
    .into())
}

pub fn pause(store_path: &Path, duration: Option<&str>, quiet: bool) -> Result<u8, CliError> {
    let paused_until = duration
        .map(|value| {
            let duration = parse_duration_expression(value)?;
            OffsetDateTime::now_utc()
                .checked_add(duration)
                .map(format_timestamp)
                .ok_or_else(|| {
                    CliError::InvalidValue(format!("pause duration is outside range: {value}"))
                })
        })
        .transpose()?
        .unwrap_or_else(|| "infinity".to_owned());
    if !daemon_running(store_path)? {
        if !quiet {
            eprintln!("Zanei daemon is not running");
        }
        return Ok(EXIT_NO_DAEMON);
    }
    let writer = StoreWriter::open(store_path)?;
    writer.set_paused_until(Some(&paused_until))?;
    if !quiet {
        println!("Zanei recording paused");
    }
    Ok(EXIT_SUCCESS)
}

pub fn resume(store_path: &Path, quiet: bool) -> Result<u8, CliError> {
    if !daemon_running(store_path)? {
        if !quiet {
            eprintln!("Zanei daemon is not running");
        }
        return Ok(EXIT_NO_DAEMON);
    }
    let writer = StoreWriter::open(store_path)?;
    writer.set_paused_until(None)?;
    if !quiet {
        println!("Zanei recording resumed");
    }
    Ok(EXIT_SUCCESS)
}

pub(super) fn daemon_running(store_path: &Path) -> Result<bool, CliError> {
    if crate::daemon::StoreOwnership::probe(store_path)?.is_none() {
        return Ok(false);
    }
    store_path
        .try_exists()
        .map_err(|source| CliError::io(store_path, source))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use zanei_core::store::DaemonMode;

    use super::{
        BACKGROUND_START_CONFIRMATION, EXIT_MISSING_PERMISSIONS, EXIT_SUCCESS,
        PENDING_PERMISSION_SNAPSHOT_GUIDANCE, StartPermissionState, start_background_with,
        wait_for_stop_completion,
    };

    #[test]
    fn background_start_succeeds_when_recorder_reports_permissions_ready() {
        let permission_check_called = Cell::new(false);

        let exit = start_background_with(
            true,
            || Ok(false),
            || {
                permission_check_called.set(true);
                Ok(StartPermissionState::Ready)
            },
            |_| {},
        )
        .expect("background start");

        assert_eq!(exit, EXIT_SUCCESS);
        assert!(permission_check_called.get());
    }

    #[test]
    fn missing_permissions_leave_the_started_recorder_running() {
        let recorder_running = Cell::new(false);

        let exit = start_background_with(
            true,
            || {
                recorder_running.set(true);
                Ok(false)
            },
            || Ok(StartPermissionState::Missing),
            |_| {},
        )
        .expect("degraded background start");

        assert_eq!(exit, EXIT_MISSING_PERMISSIONS);
        assert!(
            recorder_running.get(),
            "the recorder must not be booted out"
        );
    }

    #[test]
    fn pending_permission_snapshot_keeps_start_successful_and_guides_user() {
        let output = RefCell::new(Vec::new());

        let exit = start_background_with(
            true,
            || Ok(false),
            || Ok(StartPermissionState::PendingSnapshot),
            |message| output.borrow_mut().push(message.to_owned()),
        )
        .expect("pending background start");

        assert_eq!(exit, EXIT_SUCCESS);
        assert_eq!(
            output.into_inner(),
            [PENDING_PERMISSION_SNAPSHOT_GUIDANCE.to_owned()]
        );
    }

    #[test]
    fn background_start_confirmation_explains_launchd_registration() {
        assert!(BACKGROUND_START_CONFIRMATION.contains("launchd background item"));
        assert!(BACKGROUND_START_CONFIRMATION.contains("Login Items & Extensions"));
        assert!(BACKGROUND_START_CONFIRMATION.contains("Accessibility lists Zanei"));
        assert!(BACKGROUND_START_CONFIRMATION.contains("Input Monitoring may omit its row"));
        assert!(BACKGROUND_START_CONFIRMATION.contains("`zanei doctor` result is authoritative"));
        assert!(BACKGROUND_START_CONFIRMATION.contains("add Zanei.app with +"));
        assert!(BACKGROUND_START_CONFIRMATION.contains("bundled entry persists"));
        assert!(BACKGROUND_START_CONFIRMATION.contains("raw executable may not keep"));
        assert!(BACKGROUND_START_CONFIRMATION.ends_with(
            "Typed and clipboard content is not recorded by default. To opt in: zanei config set capture.text_content true"
        ));
    }

    #[test]
    fn launchd_stop_waits_for_owner_exit_before_launch_agent_removal() {
        let events = RefCell::new(Vec::new());

        wait_for_stop_completion(
            &DaemonMode::Launchd,
            || {
                events.borrow_mut().push("owner exited");
                Ok(())
            },
            || {
                events.borrow_mut().push("launch agent removed");
                Ok(())
            },
        )
        .expect("wait for launchd recorder stop");

        assert_eq!(
            events.into_inner(),
            ["owner exited", "launch agent removed"]
        );
    }

    #[test]
    fn foreground_stop_does_not_wait_for_launch_agent_removal() {
        wait_for_stop_completion(
            &DaemonMode::Foreground,
            || Ok(()),
            || panic!("foreground recorder has no launch agent"),
        )
        .expect("wait for foreground recorder stop");
    }
}
