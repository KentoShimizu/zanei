use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::thread;
use std::time::Instant;

use time::OffsetDateTime;
use zanei_core::config::{Config, parse_duration_expression};
use zanei_core::normalize::format_timestamp;
use zanei_core::store::DaemonMode;

use super::doctor::{StartPermissionState, require_recorder_for_start};
use super::{EXIT_MISSING_PERMISSIONS, EXIT_NO_DAEMON, EXIT_SUCCESS};
use crate::error::CliError;
use crate::paths::Paths;
use crate::store_access::{self, KeyAccess, KeyPrompt};

mod start_permissions;
mod text_content;

const BACKGROUND_START_CONFIRMATION: &str = concat!(
    "Zanei recording started\n",
    "Registered as a launchd background item. macOS notifications and an entry in Login Items & Extensions are expected.\n",
    "For the bundled distribution, Accessibility lists Zanei. Input Monitoring may omit its row even when permission is granted; the recorder-reported `zanei doctor` result is authoritative.\n",
    "To manage a missing Input Monitoring row in System Settings, add Zanei.app with +; the bundled entry persists.\n",
    "A raw executable may not keep a manually added row; use the bundled distribution for stable permission management."
);
const TEXT_CONTENT_OPT_IN_GUIDANCE: &str = "Typed and clipboard content is not recorded by default. To opt in: zanei config set capture.text_content true";
const PENDING_PERMISSION_SNAPSHOT_GUIDANCE: &str = "recorder is still waiting for macOS permission dialogs — dialogs may take a moment to appear; if none are visible, run `zanei doctor --fix`; respond to any dialogs, then check `zanei doctor`";
const RESTARTING_RECORDER: &str = "Restarting the recorder to apply text content capture...";

pub fn start(paths: &Paths, foreground: bool, quiet: bool, json: bool) -> Result<u8, CliError> {
    let config = Config::load(&paths.config)?;
    if foreground {
        crate::daemon::run_daemon(&paths.config, &paths.store, DaemonMode::Foreground)?;
        return Ok(EXIT_SUCCESS);
    }

    if let Some(owner) = crate::daemon::StoreOwnership::probe(&paths.store)? {
        return Err(crate::daemon::DaemonError::StoreOwned { pid: owner.pid }.into());
    }
    let executable = crate::executable::current().map_err(CliError::Input)?;
    let prompted_before = prompt_text_content(paths, quiet || json, || {
        start_permissions::before_bootstrap(&config, &paths.store).ok()
    })?
    .is_some();
    let config = Config::load(&paths.config)?;
    let background = start_background_with(
        quiet,
        config.capture.text_content,
        || {
            crate::daemon::start_launch_agent(&executable, &paths.config, &paths.store)
                .map_err(CliError::from)
        },
        || {
            start_permissions::after_bootstrap(
                quiet,
                || require_recorder_for_start(&config, &paths.store, &executable),
                Instant::now,
                thread::sleep,
                |message| eprintln!("{message}"),
            )
        },
        |message| println!("{message}"),
    )?;
    let permission_state = background.permission_state;
    complete_background_start_with(
        background,
        prompted_before,
        || prompt_text_content(paths, quiet || json, || Some(permission_state)),
        || restart_background(paths),
        |message| eprintln!("{message}"),
    )
}

fn prompt_text_content(
    paths: &Paths,
    output_suppressed: bool,
    permission_state: impl FnOnce() -> Option<StartPermissionState>,
) -> Result<Option<bool>, CliError> {
    let stdin_is_terminal = io::stdin().is_terminal();
    let stderr_is_terminal = io::stderr().is_terminal();
    let mut stderr = io::stderr().lock();
    text_content::maybe_prompt(
        &paths.config,
        output_suppressed,
        || stdin_is_terminal,
        || stderr_is_terminal,
        permission_state,
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

fn restart_background(paths: &Paths) -> Result<u8, CliError> {
    let stop_exit = stop(&paths.store, true)?;
    if stop_exit == EXIT_SUCCESS {
        start(paths, false, true, false)
    } else {
        Ok(stop_exit)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackgroundStartOutcome {
    exit_code: u8,
    permission_state: StartPermissionState,
}

fn start_background_with(
    quiet: bool,
    text_content_enabled: bool,
    start_launch_agent: impl FnOnce() -> Result<bool, CliError>,
    recorder_permission_state: impl FnOnce() -> Result<StartPermissionState, CliError>,
    mut print: impl FnMut(&str),
) -> Result<BackgroundStartOutcome, CliError> {
    let was_bootstrapped = start_launch_agent()?;
    let permission_state = recorder_permission_state()?;
    let exit_code = match permission_state {
        StartPermissionState::PendingSnapshot => {
            print(PENDING_PERMISSION_SNAPSHOT_GUIDANCE);
            EXIT_SUCCESS
        }
        StartPermissionState::Missing => EXIT_MISSING_PERMISSIONS,
        StartPermissionState::Ready => EXIT_SUCCESS,
    };
    if permission_state == StartPermissionState::Ready && !quiet {
        if was_bootstrapped {
            print("Zanei recording restarted");
        } else {
            print(&background_start_confirmation(text_content_enabled));
        }
    }
    Ok(BackgroundStartOutcome {
        exit_code,
        permission_state,
    })
}

fn complete_background_start_with(
    background: BackgroundStartOutcome,
    prompted_before: bool,
    prompt_after: impl FnOnce() -> Result<Option<bool>, CliError>,
    restart: impl FnOnce() -> Result<u8, CliError>,
    mut print: impl FnMut(&str),
) -> Result<u8, CliError> {
    if prompted_before || background.permission_state != StartPermissionState::Ready {
        return Ok(background.exit_code);
    }
    if prompt_after()? == Some(true) {
        print(RESTARTING_RECORDER);
        return restart();
    }
    Ok(background.exit_code)
}

fn background_start_confirmation(text_content_enabled: bool) -> String {
    if text_content_enabled {
        BACKGROUND_START_CONFIRMATION.to_owned()
    } else {
        format!("{BACKGROUND_START_CONFIRMATION}\n{TEXT_CONTENT_OPT_IN_GUIDANCE}")
    }
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
    let writer = store_access::open_writer(store_path, KeyAccess::Existing, KeyPrompt::Allowed)?;
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
    let writer = store_access::open_writer(store_path, KeyAccess::Existing, KeyPrompt::Allowed)?;
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
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    use zanei_core::store::DaemonMode;

    use super::start_permissions::{WAITING_FOR_PERMISSION_CHECK, after_bootstrap};
    use super::{
        BACKGROUND_START_CONFIRMATION, BackgroundStartOutcome, EXIT_MISSING_PERMISSIONS,
        EXIT_SUCCESS, PENDING_PERMISSION_SNAPSHOT_GUIDANCE, RESTARTING_RECORDER,
        StartPermissionState, TEXT_CONTENT_OPT_IN_GUIDANCE, background_start_confirmation,
        complete_background_start_with, start_background_with, wait_for_stop_completion,
    };

    #[test]
    fn pending_snapshots_then_ready_succeed_and_run_post_start_prompt() {
        let states = RefCell::new(VecDeque::from([
            StartPermissionState::PendingSnapshot,
            StartPermissionState::PendingSnapshot,
            StartPermissionState::Ready,
        ]));
        let sleeps = RefCell::new(Vec::new());
        let clock = Cell::new(Instant::now());
        let prompted = Cell::new(false);

        let background = start_background_with(
            false,
            false,
            || Ok(false),
            || {
                after_bootstrap(
                    false,
                    || Ok(states.borrow_mut().pop_front().expect("permission state")),
                    || clock.get(),
                    |duration| {
                        sleeps.borrow_mut().push(duration);
                        clock.set(clock.get() + duration);
                    },
                    |_| panic!("a two-second wait must not print progress"),
                )
            },
            |_| {},
        )
        .expect("background start");
        let exit = complete_background_start_with(
            background,
            false,
            || {
                prompted.set(true);
                Ok(Some(false))
            },
            || panic!("declining text capture must not restart"),
            |_| panic!("declining text capture must not print a restart notice"),
        )
        .expect("complete background start");

        assert_eq!(exit, EXIT_SUCCESS);
        assert_eq!(sleeps.into_inner(), [Duration::from_secs(1); 2]);
        assert!(prompted.get());
    }

    #[test]
    fn missing_permissions_leave_the_started_recorder_running() {
        let recorder_running = Cell::new(false);

        let background = start_background_with(
            true,
            false,
            || {
                recorder_running.set(true);
                Ok(false)
            },
            || {
                after_bootstrap(
                    true,
                    || Ok(StartPermissionState::Missing),
                    Instant::now,
                    |_| panic!("a ready permission snapshot must not sleep"),
                    |_| panic!("a ready permission snapshot must not print progress"),
                )
            },
            |_| {},
        )
        .expect("degraded background start");

        assert_eq!(background.exit_code, EXIT_MISSING_PERMISSIONS);
        assert_eq!(background.permission_state, StartPermissionState::Missing);
        assert!(
            recorder_running.get(),
            "the recorder must not be booted out"
        );
        let exit = complete_background_start_with(
            background,
            false,
            || panic!("missing permissions must not run the text content prompt"),
            || panic!("missing permissions must not restart the recorder"),
            |_| panic!("missing permissions must not print a restart notice"),
        )
        .expect("complete missing-permission start");
        assert_eq!(exit, EXIT_MISSING_PERMISSIONS);
    }

    #[test]
    fn pending_snapshot_timeout_keeps_start_successful_and_guides_user() {
        let checks = Cell::new(0);
        let sleeps = RefCell::new(Vec::new());
        let started_at = Instant::now();
        let clock = Cell::new(started_at);
        let output = RefCell::new(Vec::new());
        let progress = RefCell::new(Vec::new());

        let exit = start_background_with(
            false,
            false,
            || Ok(false),
            || {
                after_bootstrap(
                    false,
                    || {
                        checks.set(checks.get() + 1);
                        Ok(StartPermissionState::PendingSnapshot)
                    },
                    || clock.get(),
                    |duration| {
                        sleeps.borrow_mut().push(duration);
                        clock.set(clock.get() + duration);
                    },
                    |message| {
                        progress
                            .borrow_mut()
                            .push((clock.get().duration_since(started_at), message.to_owned()));
                    },
                )
            },
            |message| output.borrow_mut().push(message.to_owned()),
        )
        .expect("pending background start");

        assert_eq!(exit.exit_code, EXIT_SUCCESS);
        assert_eq!(exit.permission_state, StartPermissionState::PendingSnapshot);
        assert_eq!(checks.get(), 21, "initial read plus 20 one-second polls");
        assert_eq!(sleeps.borrow().as_slice(), [Duration::from_secs(1); 20]);
        assert_eq!(
            clock.get().duration_since(started_at),
            Duration::from_secs(20)
        );
        assert_eq!(
            output.into_inner(),
            [PENDING_PERMISSION_SNAPSHOT_GUIDANCE.to_owned()]
        );
        assert_eq!(
            progress.into_inner(),
            [(
                Duration::from_secs(5),
                WAITING_FOR_PERMISSION_CHECK.to_owned()
            )]
        );
        assert!(
            PENDING_PERMISSION_SNAPSHOT_GUIDANCE.contains("dialogs may take a moment to appear")
        );
        assert!(PENDING_PERMISSION_SNAPSHOT_GUIDANCE.contains("`zanei doctor --fix`"));
    }

    #[test]
    fn quiet_pending_snapshot_wait_suppresses_progress() {
        let clock = Cell::new(Instant::now());
        let exit = start_background_with(
            true,
            false,
            || Ok(false),
            || {
                after_bootstrap(
                    true,
                    || Ok(StartPermissionState::PendingSnapshot),
                    || clock.get(),
                    |duration| clock.set(clock.get() + duration),
                    |_| panic!("quiet start must not print progress"),
                )
            },
            |_| {},
        )
        .expect("quiet pending background start");

        assert_eq!(exit.exit_code, EXIT_SUCCESS);
        assert_eq!(exit.permission_state, StartPermissionState::PendingSnapshot);
    }

    #[test]
    fn post_start_yes_restarts_through_the_injected_canonical_path() {
        let prompted = Cell::new(false);
        let restarted = Cell::new(false);
        let output = RefCell::new(Vec::new());

        let exit = complete_background_start_with(
            BackgroundStartOutcome {
                exit_code: EXIT_SUCCESS,
                permission_state: StartPermissionState::Ready,
            },
            false,
            || {
                prompted.set(true);
                Ok(Some(true))
            },
            || {
                restarted.set(true);
                Ok(EXIT_SUCCESS)
            },
            |message| output.borrow_mut().push(message.to_owned()),
        )
        .expect("complete background start");

        assert_eq!(exit, EXIT_SUCCESS);
        assert!(prompted.get());
        assert!(restarted.get());
        assert_eq!(output.into_inner(), [RESTARTING_RECORDER.to_owned()]);
    }

    #[test]
    fn post_start_no_keeps_the_running_recorder() {
        let exit = complete_background_start_with(
            BackgroundStartOutcome {
                exit_code: EXIT_SUCCESS,
                permission_state: StartPermissionState::Ready,
            },
            false,
            || Ok(Some(false)),
            || panic!("a false decision must not restart the recorder"),
            |_| panic!("a false decision must not print a restart notice"),
        )
        .expect("complete background start");

        assert_eq!(exit, EXIT_SUCCESS);
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
        assert!(!BACKGROUND_START_CONFIRMATION.contains(TEXT_CONTENT_OPT_IN_GUIDANCE));
        assert!(background_start_confirmation(false).ends_with(TEXT_CONTENT_OPT_IN_GUIDANCE));
        assert!(!background_start_confirmation(true).contains(TEXT_CONTENT_OPT_IN_GUIDANCE));
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
