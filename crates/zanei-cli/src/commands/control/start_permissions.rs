use std::path::Path;

use zanei_core::config::Config;
use zanei_core::store::{DaemonPermissions, StoreReader, StoreStatus};

use super::super::doctor::StartPermissionState;
use crate::error::CliError;
use crate::permissions::{permission_snapshot_ready, probe_permissions};

pub(super) fn before_bootstrap(
    config: &Config,
    store_path: &Path,
) -> Result<StartPermissionState, CliError> {
    let status = if store_path
        .try_exists()
        .map_err(|source| CliError::io(store_path, source))?
    {
        Some(StoreReader::open(store_path)?.status()?)
    } else {
        None
    };
    permission_state_with(config, status.as_ref(), || {
        let required = crate::daemon::required_permissions_for(config);
        probe_permissions(&required).map_err(CliError::from)
    })
}

fn permission_state_with<E>(
    config: &Config,
    status: Option<&StoreStatus>,
    local_probe: impl FnOnce() -> Result<DaemonPermissions, E>,
) -> Result<StartPermissionState, E> {
    let required = crate::daemon::required_permissions_for(config);
    let snapshot = match status {
        Some(status) if status.heartbeat_at.is_some() => {
            let Some(snapshot) = status.last_reported_permissions() else {
                return Ok(StartPermissionState::PendingSnapshot);
            };
            snapshot.clone()
        }
        Some(_) | None => local_probe()?,
    };
    Ok(match permission_snapshot_ready(&required, &snapshot) {
        Some(true) => StartPermissionState::Ready,
        Some(false) => StartPermissionState::Missing,
        None => StartPermissionState::PendingSnapshot,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;

    use zanei_core::normalize::format_timestamp;
    use zanei_core::store::PermissionState;

    use super::*;

    #[test]
    fn prior_heartbeat_snapshot_is_used_without_a_local_probe() {
        let probed = Cell::new(false);
        let status = StoreStatus {
            heartbeat_at: Some("2020-01-01T00:00:00Z".to_owned()),
            permissions: Some(granted_permissions()),
            ..StoreStatus::default()
        };

        let state = permission_state_with(&Config::default(), Some(&status), || {
            probed.set(true);
            Ok::<_, ()>(denied_permissions())
        })
        .expect("permission state");

        assert_eq!(state, StartPermissionState::Ready);
        assert!(!probed.get());
    }

    #[test]
    fn missing_heartbeat_uses_the_local_probe() {
        let probed = Cell::new(false);
        let status = StoreStatus {
            permissions: Some(denied_permissions()),
            ..StoreStatus::default()
        };

        let state = permission_state_with(&Config::default(), Some(&status), || {
            probed.set(true);
            Ok::<_, ()>(granted_permissions())
        })
        .expect("permission state");

        assert_eq!(state, StartPermissionState::Ready);
        assert!(probed.get());
    }

    #[test]
    fn heartbeat_without_a_snapshot_is_unknown_and_does_not_probe() {
        let probed = Cell::new(false);
        let status = StoreStatus {
            heartbeat_at: Some(format_timestamp(time::OffsetDateTime::now_utc())),
            ..StoreStatus::default()
        };

        let state = permission_state_with(&Config::default(), Some(&status), || {
            probed.set(true);
            Ok::<_, ()>(granted_permissions())
        })
        .expect("permission state");

        assert_eq!(state, StartPermissionState::PendingSnapshot);
        assert!(!probed.get());
    }

    #[test]
    fn denied_prior_snapshot_is_missing_without_probe_fallback() {
        let probed = Cell::new(false);
        let status = StoreStatus {
            heartbeat_at: Some(format_timestamp(time::OffsetDateTime::now_utc())),
            permissions: Some(denied_permissions()),
            ..StoreStatus::default()
        };

        let state = permission_state_with(&Config::default(), Some(&status), || {
            probed.set(true);
            Ok::<_, ()>(granted_permissions())
        })
        .expect("permission state");

        assert_eq!(state, StartPermissionState::Missing);
        assert!(!probed.get());
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
