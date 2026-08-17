use std::{
    collections::{BTreeMap, BTreeSet},
    thread,
    time::Duration,
};

use zanei_collector::Permission;
use zanei_core::store::{DaemonPermissions, PermissionState};
use zanei_macos::permission::{
    PermissionChecker, PermissionError, PermissionStatus, request_accessibility,
    request_input_monitoring,
};

const PERMISSION_DECISION_POLL_INTERVAL: Duration = Duration::from_secs(1);
// Permission dialogs are user-paced. Two minutes allows a deliberate response while bounding the
// detached startup worker so an abandoned dialog is retried on the next daemon start.
const PERMISSION_DECISION_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PermissionRequestOutcome {
    Completed,
    TimedOut,
}

pub(crate) fn request_missing_permissions(
    required: &BTreeSet<Permission>,
) -> Result<PermissionRequestOutcome, PermissionError> {
    let checker = PermissionChecker::new();
    request_missing_permissions_with(
        required,
        |permission| permission_request_status(&checker, permission),
        |permission| match permission {
            Permission::Accessibility => request_accessibility(),
            Permission::InputMonitoring => {
                request_input_monitoring();
                Ok(())
            }
            Permission::Automation { .. } => unreachable!("automation is requested by use"),
        },
        thread::sleep,
        PERMISSION_DECISION_POLL_INTERVAL,
        PERMISSION_DECISION_TIMEOUT,
    )
}

fn request_missing_permissions_with<E>(
    required: &BTreeSet<Permission>,
    mut status_for: impl FnMut(&Permission) -> Result<PermissionStatus, E>,
    mut request: impl FnMut(&Permission) -> Result<(), E>,
    mut sleep: impl FnMut(Duration),
    poll_interval: Duration,
    timeout: Duration,
) -> Result<PermissionRequestOutcome, E> {
    for permission in [Permission::Accessibility, Permission::InputMonitoring] {
        if !required.contains(&permission) {
            continue;
        }
        if status_for(&permission)? != PermissionStatus::NotDetermined {
            continue;
        }
        request(&permission)?;
        if !wait_for_permission_decision(
            &permission,
            &mut status_for,
            &mut sleep,
            poll_interval,
            timeout,
        )? {
            return Ok(PermissionRequestOutcome::TimedOut);
        }
    }
    Ok(PermissionRequestOutcome::Completed)
}

fn wait_for_permission_decision<E>(
    permission: &Permission,
    status_for: &mut impl FnMut(&Permission) -> Result<PermissionStatus, E>,
    sleep: &mut impl FnMut(Duration),
    poll_interval: Duration,
    timeout: Duration,
) -> Result<bool, E> {
    let mut elapsed = Duration::ZERO;
    while elapsed < timeout {
        sleep(poll_interval);
        elapsed = elapsed.saturating_add(poll_interval);
        if status_for(permission)? != PermissionStatus::NotDetermined {
            return Ok(true);
        }
    }
    Ok(false)
}

fn permission_request_status(
    checker: &PermissionChecker,
    permission: &Permission,
) -> Result<PermissionStatus, PermissionError> {
    let status = checker.permission_status(permission)?;
    if *permission == Permission::Accessibility && status == PermissionStatus::Denied {
        // AXIsProcessTrusted exposes only a Boolean. During prompt coordination, false therefore
        // remains pending-or-denied; only true proves that the request has resolved as granted.
        return Ok(PermissionStatus::NotDetermined);
    }
    Ok(status)
}

pub(crate) fn probe_permissions(
    required: &BTreeSet<Permission>,
) -> Result<DaemonPermissions, PermissionError> {
    let checker = PermissionChecker::new();
    probe_permissions_with(required, |permission| checker.permission_status(permission))
}

pub(crate) fn permission_snapshot_ready(
    required: &BTreeSet<Permission>,
    snapshot: &DaemonPermissions,
) -> Option<bool> {
    required
        .iter()
        .map(|permission| match permission {
            Permission::Accessibility => Some(snapshot.accessibility == PermissionState::Granted),
            Permission::InputMonitoring => {
                Some(snapshot.input_monitoring == PermissionState::Granted)
            }
            Permission::Automation { bundle_id } => {
                snapshot.automation.get(bundle_id).map(|state| {
                    matches!(
                        state,
                        PermissionState::Granted | PermissionState::NotDetermined
                    )
                })
            }
        })
        .try_fold(true, |ready, permission_ready| {
            permission_ready.map(|permission_ready| ready && permission_ready)
        })
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
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeSet,
        time::Duration,
    };

    use zanei_collector::Permission;
    use zanei_core::store::PermissionState;
    use zanei_macos::permission::PermissionStatus;

    use super::{
        PermissionRequestOutcome, probe_permissions_with, request_missing_permissions_with,
    };

    #[test]
    fn accessibility_timeout_stops_before_requesting_input_monitoring() {
        let required = BTreeSet::from([
            Permission::Accessibility,
            Permission::InputMonitoring,
            Permission::Automation {
                bundle_id: "com.google.Chrome".to_owned(),
            },
        ]);
        let requested = RefCell::new(Vec::new());
        let accessibility_checks = Cell::new(0);

        let outcome = request_missing_permissions_with(
            &required,
            |permission| {
                Ok::<_, ()>(match permission {
                    Permission::Accessibility => {
                        accessibility_checks.set(accessibility_checks.get() + 1);
                        PermissionStatus::NotDetermined
                    }
                    Permission::InputMonitoring => {
                        panic!("input monitoring must not be checked before Accessibility resolves")
                    }
                    Permission::Automation { .. } => panic!("automation must not be probed"),
                })
            },
            |permission| {
                requested.borrow_mut().push(permission.clone());
                Ok::<_, ()>(())
            },
            |_| assert_eq!(*requested.borrow(), [Permission::Accessibility]),
            Duration::from_secs(1),
            Duration::from_secs(3),
        )
        .expect("request missing permissions");

        assert_eq!(outcome, PermissionRequestOutcome::TimedOut);
        assert_eq!(accessibility_checks.get(), 4);
        assert_eq!(requested.into_inner(), [Permission::Accessibility]);
    }

    #[test]
    fn requests_input_monitoring_after_accessibility_is_decided() {
        let required = BTreeSet::from([Permission::Accessibility, Permission::InputMonitoring]);
        let requested = RefCell::new(Vec::new());
        let accessibility_checks = Cell::new(0);
        let input_checks = Cell::new(0);

        let outcome = request_missing_permissions_with(
            &required,
            |permission| {
                let checks = match permission {
                    Permission::Accessibility => &accessibility_checks,
                    Permission::InputMonitoring => &input_checks,
                    Permission::Automation { .. } => panic!("automation must not be probed"),
                };
                checks.set(checks.get() + 1);
                Ok::<_, ()>(if checks.get() == 1 {
                    PermissionStatus::NotDetermined
                } else {
                    PermissionStatus::Granted
                })
            },
            |permission| {
                requested.borrow_mut().push(permission.clone());
                Ok::<_, ()>(())
            },
            |_| {},
            Duration::from_secs(1),
            Duration::from_secs(3),
        )
        .expect("request missing permissions");

        assert_eq!(outcome, PermissionRequestOutcome::Completed);
        assert_eq!(
            requested.into_inner(),
            [Permission::Accessibility, Permission::InputMonitoring]
        );
    }

    #[test]
    fn denied_permission_does_not_block_the_next_request() {
        let required = BTreeSet::from([Permission::Accessibility, Permission::InputMonitoring]);
        let requested = RefCell::new(Vec::new());
        let accessibility_checks = Cell::new(0);
        let input_checks = Cell::new(0);

        let outcome = request_missing_permissions_with(
            &required,
            |permission| {
                let (checks, decided) = match permission {
                    Permission::Accessibility => (&accessibility_checks, PermissionStatus::Denied),
                    Permission::InputMonitoring => (&input_checks, PermissionStatus::Granted),
                    Permission::Automation { .. } => panic!("automation must not be probed"),
                };
                checks.set(checks.get() + 1);
                Ok::<_, ()>(if checks.get() == 1 {
                    PermissionStatus::NotDetermined
                } else {
                    decided
                })
            },
            |permission| {
                requested.borrow_mut().push(permission.clone());
                Ok::<_, ()>(())
            },
            |_| {},
            Duration::from_secs(1),
            Duration::from_secs(3),
        )
        .expect("request missing permissions");

        assert_eq!(outcome, PermissionRequestOutcome::Completed);
        assert_eq!(
            requested.into_inner(),
            [Permission::Accessibility, Permission::InputMonitoring]
        );
    }

    #[test]
    fn granted_permissions_are_skipped_and_automation_remains_lazy() {
        let required = BTreeSet::from([
            Permission::Accessibility,
            Permission::InputMonitoring,
            Permission::Automation {
                bundle_id: "com.google.Chrome".to_owned(),
            },
        ]);
        let probed = RefCell::new(Vec::new());
        let requested = RefCell::new(Vec::new());

        let outcome = request_missing_permissions_with(
            &required,
            |permission| {
                probed.borrow_mut().push(permission.clone());
                Ok::<_, ()>(PermissionStatus::Granted)
            },
            |permission| {
                requested.borrow_mut().push(permission.clone());
                Ok::<_, ()>(())
            },
            |_| panic!("granted permissions must not be polled"),
            Duration::from_secs(1),
            Duration::from_secs(3),
        )
        .expect("skip granted permissions");

        assert_eq!(outcome, PermissionRequestOutcome::Completed);
        assert_eq!(
            probed.into_inner(),
            [Permission::Accessibility, Permission::InputMonitoring]
        );
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
