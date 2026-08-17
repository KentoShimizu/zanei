use std::collections::{BTreeMap, BTreeSet};

use zanei_collector::Permission;
use zanei_core::store::{DaemonPermissions, PermissionState};
use zanei_macos::permission::{
    PermissionChecker, PermissionError, PermissionStatus, request_accessibility,
    request_input_monitoring,
};

pub(crate) fn request_missing_permissions(
    required: &BTreeSet<Permission>,
) -> Result<(), PermissionError> {
    let checker = PermissionChecker::new();
    request_missing_permissions_with(
        required,
        |permission| checker.permission_status(permission),
        |permission| match permission {
            Permission::Accessibility => request_accessibility(),
            Permission::InputMonitoring => {
                request_input_monitoring();
                Ok(())
            }
            Permission::Automation { .. } => unreachable!("automation is requested by use"),
        },
    )
}

fn request_missing_permissions_with<E>(
    required: &BTreeSet<Permission>,
    mut status_for: impl FnMut(&Permission) -> Result<PermissionStatus, E>,
    mut request: impl FnMut(&Permission) -> Result<(), E>,
) -> Result<(), E> {
    for permission in required {
        if matches!(permission, Permission::Automation { .. }) {
            continue;
        }
        if status_for(permission)? != PermissionStatus::Granted {
            request(permission)?;
        }
    }
    Ok(())
}

pub(crate) fn probe_permissions(
    required: &BTreeSet<Permission>,
) -> Result<DaemonPermissions, PermissionError> {
    let checker = PermissionChecker::new();
    probe_permissions_with(required, |permission| checker.permission_status(permission))
}

fn probe_permissions_with<E>(
    required: &BTreeSet<Permission>,
    mut status_for: impl FnMut(&Permission) -> Result<PermissionStatus, E>,
) -> Result<DaemonPermissions, E> {
    let accessibility = status_for(&Permission::Accessibility)?;
    let input_monitoring = status_for(&Permission::InputMonitoring)?;
    let mut automation = BTreeMap::new();
    for permission in required {
        if let Permission::Automation { bundle_id } = permission {
            automation.insert(bundle_id.clone(), status_for(permission)?);
        }
    }
    let permissions_ok = required.iter().all(|permission| {
        let status = match permission {
            Permission::Accessibility => Some(accessibility),
            Permission::InputMonitoring => Some(input_monitoring),
            Permission::Automation { bundle_id } => automation.get(bundle_id).copied(),
        };
        status.is_some_and(|status| permission_is_ready(permission, status))
    });

    Ok(DaemonPermissions {
        permissions_ok,
        accessibility: permission_state(accessibility),
        input_monitoring: permission_state(input_monitoring),
        automation: automation
            .into_iter()
            .map(|(bundle_id, status)| (bundle_id, permission_state(status)))
            .collect(),
    })
}

fn permission_is_ready(permission: &Permission, status: PermissionStatus) -> bool {
    status == PermissionStatus::Granted
        || matches!(
            (permission, status),
            (
                Permission::Automation { .. },
                PermissionStatus::NotDetermined
            )
        )
}

const fn permission_state(status: PermissionStatus) -> PermissionState {
    match status {
        PermissionStatus::Granted => PermissionState::Granted,
        PermissionStatus::Denied => PermissionState::Denied,
        PermissionStatus::NotDetermined => PermissionState::NotDetermined,
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeSet};

    use zanei_collector::Permission;
    use zanei_core::store::PermissionState;
    use zanei_macos::permission::PermissionStatus;

    use super::{probe_permissions_with, request_missing_permissions_with};

    #[test]
    fn requests_each_missing_daemon_permission_once_and_never_requests_automation() {
        let required = BTreeSet::from([
            Permission::Accessibility,
            Permission::InputMonitoring,
            Permission::Automation {
                bundle_id: "com.google.Chrome".to_owned(),
            },
        ]);
        let probed = RefCell::new(Vec::new());
        let requested = RefCell::new(Vec::new());

        request_missing_permissions_with(
            &required,
            |permission| {
                probed.borrow_mut().push(permission.clone());
                Ok::<_, ()>(match permission {
                    Permission::Accessibility => PermissionStatus::Denied,
                    Permission::InputMonitoring => PermissionStatus::NotDetermined,
                    Permission::Automation { .. } => panic!("automation must not be probed"),
                })
            },
            |permission| {
                requested.borrow_mut().push(permission.clone());
                Ok::<_, ()>(())
            },
        )
        .expect("request missing permissions");

        assert_eq!(
            probed.into_inner(),
            [Permission::Accessibility, Permission::InputMonitoring]
        );
        assert_eq!(
            requested.into_inner(),
            [Permission::Accessibility, Permission::InputMonitoring]
        );
    }

    #[test]
    fn granted_permissions_are_not_requested() {
        let required = BTreeSet::from([Permission::Accessibility, Permission::InputMonitoring]);
        let requested = RefCell::new(Vec::new());

        request_missing_permissions_with(
            &required,
            |_| Ok::<_, ()>(PermissionStatus::Granted),
            |permission| {
                requested.borrow_mut().push(permission.clone());
                Ok::<_, ()>(())
            },
        )
        .expect("skip granted permissions");

        assert!(requested.into_inner().is_empty());
    }

    #[test]
    fn derives_typed_snapshot_for_every_required_permission() {
        let required = BTreeSet::from([
            Permission::Accessibility,
            Permission::InputMonitoring,
            Permission::Automation {
                bundle_id: "com.google.Chrome".to_owned(),
            },
        ]);

        let snapshot = probe_permissions_with(&required, |permission| match permission {
            Permission::Accessibility => Ok::<_, ()>(PermissionStatus::Granted),
            Permission::InputMonitoring => Ok(PermissionStatus::Denied),
            Permission::Automation { .. } => Ok(PermissionStatus::NotDetermined),
        })
        .expect("probe snapshot");

        assert!(!snapshot.permissions_ok);
        assert_eq!(snapshot.accessibility, PermissionState::Granted);
        assert_eq!(snapshot.input_monitoring, PermissionState::Denied);
        assert_eq!(
            snapshot.automation["com.google.Chrome"],
            PermissionState::NotDetermined
        );
    }

    #[test]
    fn pending_automation_is_ready_until_the_target_app_can_be_probed() {
        let required = BTreeSet::from([Permission::Automation {
            bundle_id: "com.google.Chrome".to_owned(),
        }]);

        let snapshot = probe_permissions_with(&required, |permission| match permission {
            Permission::Accessibility | Permission::InputMonitoring => {
                Ok::<_, ()>(PermissionStatus::Denied)
            }
            Permission::Automation { .. } => Ok(PermissionStatus::NotDetermined),
        })
        .expect("probe snapshot");

        assert!(snapshot.permissions_ok);
    }
}
