//! Bounded launchd removal and recorder-readiness waits.

use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

use zanei_core::store::{StoreError, StoreFormat, StoreKey, StoreStatus};

use crate::store_access::KeyPrompt;

use super::{
    DAEMON_CONTROL_POLL_INTERVAL, DAEMON_CONTROL_TIMEOUT, DaemonError, StoreOwner, StoreOwnership,
    is_bootstrapped,
};

pub fn wait_for_launch_agent_removal() -> Result<(), DaemonError> {
    wait_for_launch_agent_removal_with(DAEMON_CONTROL_TIMEOUT, is_bootstrapped, thread::sleep)
}

pub(super) fn start_launch_agent_with(
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
    if let Err(error) = wait_for_daemon_start_with(timeout, is_daemon_alive, sleep) {
        if matches!(error, DaemonError::Store(StoreError::Locked(_))) {
            let _ = bootout();
        }
        return Err(error);
    }
    Ok(registered)
}

pub(super) fn daemon_is_alive_with(
    store_path: &Path,
    probe_key: impl FnOnce() -> Result<Option<StoreKey>, StoreError>,
) -> Result<bool, DaemonError> {
    if matches!(
        StoreFormat::probe(store_path)?,
        StoreFormat::Missing | StoreFormat::Plaintext
    ) {
        probe_key().or_else(|error| match error {
            StoreError::Locked(_) => Err(DaemonError::Store(error)),
            _ => Ok(None),
        })?;
    }
    let status = match crate::store_access::open_reader(store_path, KeyPrompt::Suppressed)
        .and_then(|reader| reader.status())
    {
        Ok(status) => status,
        Err(error @ StoreError::Locked(_)) => return Err(DaemonError::Store(error)),
        Err(_) => return Ok(false),
    };
    let Some(owner) = StoreOwnership::probe(store_path)? else {
        return Ok(false);
    };
    Ok(owner_has_fresh_heartbeat(&owner, &status))
}

pub(super) fn owner_has_fresh_heartbeat(owner: &StoreOwner, status: &StoreStatus) -> bool {
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
