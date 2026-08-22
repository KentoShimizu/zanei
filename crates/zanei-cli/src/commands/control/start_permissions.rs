use std::path::Path;
use std::time::{Duration, Instant};

use zanei_core::config::Config;
use zanei_core::store::StoreStatus;

use super::super::doctor::StartPermissionState;
use crate::error::CliError;
use crate::permissions::permission_snapshot_ready;
use crate::store_access::{self, KeyPrompt};

pub(super) const WAITING_FOR_PERMISSION_CHECK: &str =
    "Waiting for the recorder's permission check...";
const PERMISSION_SNAPSHOT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const PERMISSION_SNAPSHOT_PROGRESS_AFTER: Duration = Duration::from_secs(5);
// The Automation probe can take 2 seconds and permission snapshots are published on a 5-second
// heartbeat. A 20-second budget covers both plus scheduling margin without hiding an unanswered
// macOS permission dialog indefinitely.
const PERMISSION_SNAPSHOT_WAIT_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) fn before_bootstrap(
    config: &Config,
    store_path: &Path,
) -> Result<StartPermissionState, CliError> {
    let status = if store_path
        .try_exists()
        .map_err(|source| CliError::io(store_path, source))?
    {
        Some(store_access::open_reader(store_path, KeyPrompt::Allowed)?.status()?)
    } else {
        None
    };
    Ok(permission_state(config, status.as_ref()))
}

pub(super) fn after_bootstrap(
    quiet: bool,
    mut permission_state: impl FnMut() -> Result<StartPermissionState, CliError>,
    mut now: impl FnMut() -> Instant,
    mut sleep: impl FnMut(Duration),
    mut print_progress: impl FnMut(&str),
) -> Result<StartPermissionState, CliError> {
    let started_at = now();
    let deadline = started_at + PERMISSION_SNAPSHOT_WAIT_TIMEOUT;
    let progress_at = started_at + PERMISSION_SNAPSHOT_PROGRESS_AFTER;
    let mut progress_printed = false;
    let mut state = permission_state()?;
    while state == StartPermissionState::PendingSnapshot && now() < deadline {
        let remaining = deadline.saturating_duration_since(now());
        sleep(PERMISSION_SNAPSHOT_POLL_INTERVAL.min(remaining));
        state = permission_state()?;
        if !quiet
            && !progress_printed
            && state == StartPermissionState::PendingSnapshot
            && now() >= progress_at
        {
            print_progress(WAITING_FOR_PERMISSION_CHECK);
            progress_printed = true;
        }
    }
    Ok(state)
}

fn permission_state(config: &Config, status: Option<&StoreStatus>) -> StartPermissionState {
    let required = crate::daemon::required_permissions_for(config);
    // Never fall back to a CLI-local probe here: the CLI inherits the terminal's TCC identity,
    // not the recorder's, so its result cannot authorize a recorder-specific opt-in prompt.
    let Some(snapshot) = status.and_then(StoreStatus::last_reported_permissions) else {
        return StartPermissionState::PendingSnapshot;
    };
    match permission_snapshot_ready(&required, snapshot) {
        Some(true) => StartPermissionState::Ready,
        Some(false) => StartPermissionState::Missing,
        None => StartPermissionState::PendingSnapshot,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::TempDir;
    use zanei_core::store::{
        DaemonMode, DaemonPermissions, DaemonState, PermissionState, StoreWriter,
    };

    use super::*;

    #[test]
    fn persisted_last_report_is_used_before_bootstrap() {
        let status = StoreStatus {
            last_known_permissions: Some(granted_permissions()),
            ..StoreStatus::default()
        };

        let state = permission_state(&Config::default(), Some(&status));

        assert_eq!(state, StartPermissionState::Ready);
    }

    #[test]
    fn stopped_store_last_report_is_ready_before_bootstrap() {
        let directory = TempDir::new().expect("temporary directory");
        let store = directory.path().join("events.sqlite");
        let writer = StoreWriter::open(&store).expect("open fake store");
        let started_at = "2026-08-17T10:00:00Z";
        writer
            .write_daemon_state(&DaemonState {
                pid: Some(42),
                started_at: Some(started_at.to_owned()),
                instance_id: Some(format!("42@{started_at}")),
                mode: Some(DaemonMode::Launchd),
                heartbeat_at: Some("2026-08-17T10:00:01Z".to_owned()),
                retention_hours: Some(48),
                permissions: Some(granted_permissions()),
                ..DaemonState::default()
            })
            .expect("write recorder report");
        writer
            .write_daemon_state(&DaemonState::default())
            .expect("clear fake recorder heartbeat");

        assert_eq!(
            before_bootstrap(&Config::default(), &store).expect("bootstrap permission state"),
            StartPermissionState::Ready
        );
    }

    #[test]
    fn terminal_tcc_grants_cannot_replace_an_absent_recorder_report() {
        let terminal_tcc_snapshot = granted_permissions();

        let state = permission_state(&Config::default(), None);

        assert!(terminal_tcc_snapshot.permissions_ok);
        assert_eq!(state, StartPermissionState::PendingSnapshot);
    }

    #[test]
    fn incomplete_recorder_report_is_pending() {
        let mut incomplete = granted_permissions();
        incomplete.automation.clear();
        let status = StoreStatus {
            last_known_permissions: Some(incomplete),
            ..StoreStatus::default()
        };

        let state = permission_state(&Config::default(), Some(&status));

        assert_eq!(state, StartPermissionState::PendingSnapshot);
    }

    #[test]
    fn denied_persisted_report_is_missing() {
        let status = StoreStatus {
            last_known_permissions: Some(denied_permissions()),
            ..StoreStatus::default()
        };

        let state = permission_state(&Config::default(), Some(&status));

        assert_eq!(state, StartPermissionState::Missing);
    }

    fn granted_permissions() -> DaemonPermissions {
        DaemonPermissions {
            permissions_ok: true,
            accessibility: PermissionState::Granted,
            input_monitoring: PermissionState::Granted,
            automation: BTreeMap::from([(
                "com.google.Chrome".to_owned(),
                PermissionState::Granted,
            )]),
        }
    }

    fn denied_permissions() -> DaemonPermissions {
        DaemonPermissions {
            permissions_ok: false,
            accessibility: PermissionState::Denied,
            input_monitoring: PermissionState::Granted,
            automation: BTreeMap::from([(
                "com.google.Chrome".to_owned(),
                PermissionState::Granted,
            )]),
        }
    }
}
