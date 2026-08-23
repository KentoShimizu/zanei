//! launchctl service lifecycle and process termination.

use std::{
    fs,
    process::{Command, Output},
};

use super::{DaemonError, ID, KILL, LABEL, LAUNCHCTL, launch_agent_path};

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

pub(super) fn gui_domain() -> Result<String, DaemonError> {
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

pub(super) fn command_succeeded(
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
