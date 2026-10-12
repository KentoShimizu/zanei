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

const STILL_OPENING_STORE: &str = "Waiting for the recorder to finish opening the store; the first start after an upgrade can take a while...";
// How long `start` waits, from bootstrap, for a recorder that holds the store
// to report alive. Opening the store can include a schema migration that
// rewrites every event: one of 1.08 million events took 8 to 16 seconds, and
// the time grows with the store. Three minutes covers a store about ten times
// that size on that machine, with margin for slower disks, while a recorder
// stuck holding the store still fails in bounded time.
const STORE_OPEN_TIMEOUT: Duration = Duration::from_secs(180);

/// What `start` sees of the recorder it just bootstrapped.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum DaemonReadiness {
    /// No recorder holds the store: it has not started yet, or it exited.
    NotStarted,
    /// A recorder holds the store but has not written its first heartbeat:
    /// it is still opening the store.
    Starting { instance_id: String },
    /// The recorder holding the store has written its first heartbeat.
    Alive { instance_id: String },
}

pub(super) fn start_launch_agent_with(
    registered: bool,
    timeout: Duration,
    mut bootout: impl FnMut() -> Result<(), DaemonError>,
    is_registered: impl FnMut() -> Result<bool, DaemonError>,
    mut sleep: impl FnMut(Duration),
    mut bootstrap: impl FnMut() -> Result<(), DaemonError>,
    wait_for_daemon_start: impl FnOnce() -> Result<(), DaemonError>,
) -> Result<bool, DaemonError> {
    if registered {
        bootout()?;
        wait_for_launch_agent_removal_with(timeout, is_registered, &mut sleep)?;
    }
    bootstrap()?;
    if let Err(error) = wait_for_daemon_start() {
        if matches!(error, DaemonError::Store(StoreError::Locked(_))) {
            let _ = bootout();
        }
        return Err(error);
    }
    Ok(registered)
}

pub(super) fn daemon_readiness_with(
    store_path: &Path,
    probe_key: impl FnOnce() -> Result<Option<StoreKey>, StoreError>,
) -> Result<DaemonReadiness, DaemonError> {
    if matches!(
        StoreFormat::probe(store_path)?,
        StoreFormat::Missing | StoreFormat::Plaintext
    ) {
        probe_key().or_else(|error| match error {
            StoreError::Locked(_) => Err(DaemonError::Store(error)),
            _ => Ok(None),
        })?;
    }
    // A store the recorder is still creating or migrating may not be readable
    // yet; that is not a failure while the recorder holds it.
    let status = match crate::store_access::open_reader(store_path, KeyPrompt::Suppressed)
        .and_then(|reader| reader.status())
    {
        Ok(status) => Some(status),
        Err(error @ StoreError::Locked(_)) => return Err(DaemonError::Store(error)),
        Err(_) => None,
    };
    let Some(owner) = StoreOwnership::probe(store_path)? else {
        return Ok(DaemonReadiness::NotStarted);
    };
    if status.is_some_and(|status| owner_has_fresh_heartbeat(&owner, &status)) {
        return Ok(DaemonReadiness::Alive {
            instance_id: owner.instance_id,
        });
    }
    Ok(DaemonReadiness::Starting {
        instance_id: owner.instance_id,
    })
}

pub(super) fn owner_has_fresh_heartbeat(owner: &StoreOwner, status: &StoreStatus) -> bool {
    status.running && status.instance_id.as_deref() == Some(owner.instance_id.as_str())
}

/// Waits until the recorder reports that it is alive. A recorder that has
/// not taken the store within `timeout` fails the wait; one that holds it gets
/// until [`STORE_OPEN_TIMEOUT`] after bootstrap to finish opening it. Once
/// seen, the recorder exiting before it reports alive fails the wait at once,
/// even if launchd relaunches it.
pub(super) fn wait_for_daemon_start_with(
    timeout: Duration,
    mut readiness: impl FnMut() -> Result<DaemonReadiness, DaemonError>,
    mut now: impl FnMut() -> Instant,
    mut sleep: impl FnMut(Duration),
    mut notice: impl FnMut(&str),
) -> Result<(), DaemonError> {
    let started_at = now();
    let mut deadline = started_at + timeout;
    let mut starting: Option<String> = None;
    let mut noticed = false;
    loop {
        match (readiness()?, &starting) {
            (DaemonReadiness::Alive { .. }, None) => return Ok(()),
            (DaemonReadiness::Alive { instance_id }, Some(seen)) if instance_id == *seen => {
                return Ok(());
            }
            (DaemonReadiness::NotStarted, None) => {}
            (DaemonReadiness::Starting { instance_id }, None) => {
                deadline = deadline.max(started_at + STORE_OPEN_TIMEOUT);
                starting = Some(instance_id);
            }
            (DaemonReadiness::Starting { instance_id }, Some(seen)) if instance_id == *seen => {}
            // The recorder seen holding the store exited, and launchd may
            // already have relaunched another one, even one that is alive.
            (_, Some(_)) => return Err(DaemonError::DaemonDidNotStart),
        }
        let current = now();
        if current >= deadline {
            return Err(DaemonError::DaemonDidNotStart);
        }
        if !noticed && current >= started_at + timeout {
            notice(STILL_OPENING_STORE);
            noticed = true;
        }
        sleep(DAEMON_CONTROL_POLL_INTERVAL.min(deadline - current));
    }
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

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        time::{Duration, Instant},
    };

    use super::{
        DaemonError, DaemonReadiness, STILL_OPENING_STORE, STORE_OPEN_TIMEOUT,
        wait_for_daemon_start_with,
    };

    const TIMEOUT: Duration = Duration::from_secs(10);

    fn starting(instance_id: &str) -> DaemonReadiness {
        DaemonReadiness::Starting {
            instance_id: instance_id.to_owned(),
        }
    }

    fn alive(instance_id: &str) -> DaemonReadiness {
        DaemonReadiness::Alive {
            instance_id: instance_id.to_owned(),
        }
    }

    /// Runs the wait on a simulated clock; `recorder` sees the time since
    /// bootstrap. Returns the result, when it came, and the notices printed.
    fn wait_with(
        mut recorder: impl FnMut(Duration) -> DaemonReadiness,
    ) -> (Result<(), DaemonError>, Duration, Vec<String>) {
        let bootstrapped = Instant::now();
        let clock = Cell::new(bootstrapped);
        let notices = RefCell::new(Vec::new());
        let result = wait_for_daemon_start_with(
            TIMEOUT,
            || Ok(recorder(clock.get() - bootstrapped)),
            || clock.get(),
            |duration| clock.set(clock.get() + duration),
            |notice| notices.borrow_mut().push(notice.to_owned()),
        );
        (result, clock.get() - bootstrapped, notices.into_inner())
    }

    #[test]
    fn a_recorder_migrating_a_large_store_is_not_reported_as_failed() {
        let (result, elapsed, notices) = wait_with(|elapsed| match elapsed.as_secs() {
            0..150 => starting("1@start"),
            _ => alive("1@start"),
        });

        result.expect("a recorder holding the store keeps the wait going");
        assert_eq!(elapsed, Duration::from_secs(150));
        assert_eq!(notices, [STILL_OPENING_STORE], "noticed once, past 10 s");
    }

    #[test]
    fn a_recorder_holding_the_store_past_the_bound_fails() {
        let (result, elapsed, notices) = wait_with(|_| starting("1@start"));

        assert!(matches!(result, Err(DaemonError::DaemonDidNotStart)));
        assert_eq!(elapsed, STORE_OPEN_TIMEOUT);
        assert_eq!(notices, [STILL_OPENING_STORE]);
    }

    #[test]
    fn a_recorder_that_exits_while_starting_fails_the_wait_at_once() {
        // Crashes at 5 s; by the next poll launchd may have relaunched it, and
        // the relaunched recorder may even have reported alive already.
        for after_exit in [
            DaemonReadiness::NotStarted,
            starting("2@restart"),
            alive("2@restart"),
        ] {
            let mut after_exit = Some(after_exit);
            let (result, elapsed, _) = wait_with(|elapsed| match elapsed.as_secs() {
                0..5 => starting("1@start"),
                _ => after_exit.take().expect("the wait stops at the exit"),
            });

            assert!(matches!(result, Err(DaemonError::DaemonDidNotStart)));
            assert_eq!(elapsed, Duration::from_secs(5));
        }
    }

    #[test]
    fn a_recorder_that_never_takes_the_store_fails_after_the_timeout_without_a_notice() {
        let (result, elapsed, notices) = wait_with(|_| DaemonReadiness::NotStarted);

        assert!(matches!(result, Err(DaemonError::DaemonDidNotStart)));
        assert_eq!(elapsed, TIMEOUT);
        assert!(notices.is_empty());
    }
}
