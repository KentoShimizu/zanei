use std::{collections::BTreeSet, thread, time::Duration};

use zanei_collector::Capability;
use zanei_core::{CapabilityState, DaemonCapabilities};
use zanei_macos::permission::{
    PermissionChecker, PermissionError, PermissionStatus, request_accessibility,
    request_input_monitoring,
};

const PERMISSION_DECISION_POLL_INTERVAL: Duration = Duration::from_secs(1);
// AXIsProcessTrusted cannot distinguish pending from denied, so wait for a grant or this cap.
// Two minutes is acceptable because the detached worker leaves the daemon responsive while the
// user responds. At the cap, continue so a denied Accessibility request does not prevent asking
// for Input Monitoring.
const PERMISSION_DECISION_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PermissionRequestOutcome {
    Completed,
    TimedOut,
}

pub(crate) fn request_missing_permissions(
    required: &BTreeSet<Capability>,
) -> Result<PermissionRequestOutcome, PermissionError> {
    let checker = PermissionChecker::new();
    request_missing_permissions_with(
        required,
        |capability| permission_request_status(&checker, capability),
        |capability| match capability {
            Capability::ReadAccessibilityTree => request_accessibility(),
            Capability::ObserveInput => {
                request_input_monitoring();
                Ok(())
            }
            Capability::AutomateBrowser => unreachable!("automation is requested by use"),
        },
        thread::sleep,
        PERMISSION_DECISION_POLL_INTERVAL,
        PERMISSION_DECISION_TIMEOUT,
    )
}

fn request_missing_permissions_with<E>(
    required: &BTreeSet<Capability>,
    mut status_for: impl FnMut(&Capability) -> Result<PermissionStatus, E>,
    mut request: impl FnMut(&Capability) -> Result<(), E>,
    mut sleep: impl FnMut(Duration),
    poll_interval: Duration,
    timeout: Duration,
) -> Result<PermissionRequestOutcome, E> {
    let mut outcome = PermissionRequestOutcome::Completed;
    for capability in [Capability::ReadAccessibilityTree, Capability::ObserveInput] {
        if !required.contains(&capability) {
            continue;
        }
        if status_for(&capability)? != PermissionStatus::NotDetermined {
            continue;
        }
        request(&capability)?;
        if !wait_for_permission_grant(
            &capability,
            &mut status_for,
            &mut sleep,
            poll_interval,
            timeout,
        )? {
            outcome = PermissionRequestOutcome::TimedOut;
        }
    }
    Ok(outcome)
}

fn wait_for_permission_grant<E>(
    capability: &Capability,
    status_for: &mut impl FnMut(&Capability) -> Result<PermissionStatus, E>,
    sleep: &mut impl FnMut(Duration),
    poll_interval: Duration,
    timeout: Duration,
) -> Result<bool, E> {
    let mut elapsed = Duration::ZERO;
    while elapsed < timeout {
        sleep(poll_interval);
        elapsed = elapsed.saturating_add(poll_interval);
        if status_for(capability)? == PermissionStatus::Granted {
            return Ok(true);
        }
    }
    Ok(false)
}

fn permission_request_status(
    checker: &PermissionChecker,
    capability: &Capability,
) -> Result<PermissionStatus, PermissionError> {
    let status = checker.permission_status(capability)?;
    if *capability == Capability::ReadAccessibilityTree && status == PermissionStatus::Denied {
        // AXIsProcessTrusted exposes only a Boolean. During prompt coordination, false therefore
        // remains pending-or-denied; only true proves that the request has resolved as granted.
        return Ok(PermissionStatus::NotDetermined);
    }
    Ok(status)
}

pub(crate) fn probe_permissions(
    required: &BTreeSet<Capability>,
) -> Result<DaemonCapabilities, PermissionError> {
    let checker = PermissionChecker::new();
    probe_permissions_with(required, |capability| checker.permission_status(capability))
}

fn probe_permissions_with<E>(
    required: &BTreeSet<Capability>,
    mut status_for: impl FnMut(&Capability) -> Result<PermissionStatus, E>,
) -> Result<DaemonCapabilities, E> {
    let accessibility = status_for(&Capability::ReadAccessibilityTree)?;
    let input_monitoring = status_for(&Capability::ObserveInput)?;
    let automation = if required.contains(&Capability::AutomateBrowser) {
        status_for(&Capability::AutomateBrowser)?
    } else {
        PermissionStatus::NotDetermined
    };
    Ok(DaemonCapabilities::new(
        required.clone(),
        capability_state(accessibility),
        capability_state(input_monitoring),
        capability_state(automation),
    ))
}

const fn capability_state(status: PermissionStatus) -> CapabilityState {
    match status {
        PermissionStatus::Granted => CapabilityState::Available,
        PermissionStatus::Denied => CapabilityState::ActionRequired,
        PermissionStatus::NotDetermined => CapabilityState::Deferred,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeSet,
        time::Duration,
    };

    use zanei_collector::Capability;
    use zanei_core::{CapabilityState, DaemonCapabilities};
    use zanei_macos::permission::PermissionStatus;

    use super::{
        PermissionRequestOutcome, probe_permissions_with, request_missing_permissions_with,
    };

    #[test]
    fn false_accessibility_status_does_not_advance_before_timeout() {
        let required =
            BTreeSet::from([Capability::ReadAccessibilityTree, Capability::ObserveInput]);
        let requested = RefCell::new(Vec::new());
        let accessibility_checks = Cell::new(0);
        let input_checks = Cell::new(0);

        let outcome = request_missing_permissions_with(
            &required,
            |permission| {
                Ok::<_, ()>(match permission {
                    Capability::ReadAccessibilityTree => {
                        accessibility_checks.set(accessibility_checks.get() + 1);
                        if accessibility_checks.get() == 1 {
                            PermissionStatus::NotDetermined
                        } else {
                            PermissionStatus::Denied
                        }
                    }
                    Capability::ObserveInput => {
                        assert_eq!(accessibility_checks.get(), 4);
                        input_checks.set(input_checks.get() + 1);
                        if input_checks.get() == 1 {
                            PermissionStatus::NotDetermined
                        } else {
                            PermissionStatus::Granted
                        }
                    }
                    Capability::AutomateBrowser => panic!("automation must not be probed"),
                })
            },
            |permission| {
                requested.borrow_mut().push(*permission);
                Ok::<_, ()>(())
            },
            |_| {
                if input_checks.get() == 0 {
                    assert_eq!(*requested.borrow(), [Capability::ReadAccessibilityTree]);
                }
            },
            Duration::from_secs(1),
            Duration::from_secs(3),
        )
        .expect("request missing permissions");

        assert_eq!(outcome, PermissionRequestOutcome::TimedOut);
        assert_eq!(accessibility_checks.get(), 4);
        assert_eq!(
            requested.into_inner(),
            [Capability::ReadAccessibilityTree, Capability::ObserveInput]
        );
    }

    #[test]
    fn granted_transition_requests_the_next_permission() {
        let required =
            BTreeSet::from([Capability::ReadAccessibilityTree, Capability::ObserveInput]);
        let requested = RefCell::new(Vec::new());
        let accessibility_checks = Cell::new(0);
        let input_checks = Cell::new(0);

        let outcome = request_missing_permissions_with(
            &required,
            |permission| {
                let checks = match permission {
                    Capability::ReadAccessibilityTree => &accessibility_checks,
                    Capability::ObserveInput => &input_checks,
                    Capability::AutomateBrowser => panic!("automation must not be probed"),
                };
                checks.set(checks.get() + 1);
                Ok::<_, ()>(match checks.get() {
                    1 => PermissionStatus::NotDetermined,
                    2 => PermissionStatus::Denied,
                    _ => PermissionStatus::Granted,
                })
            },
            |permission| {
                requested.borrow_mut().push(*permission);
                Ok::<_, ()>(())
            },
            |_| {
                if input_checks.get() == 0 {
                    assert_eq!(*requested.borrow(), [Capability::ReadAccessibilityTree]);
                }
            },
            Duration::from_secs(1),
            Duration::from_secs(3),
        )
        .expect("request missing permissions");

        assert_eq!(outcome, PermissionRequestOutcome::Completed);
        assert_eq!(
            requested.into_inner(),
            [Capability::ReadAccessibilityTree, Capability::ObserveInput]
        );
    }

    #[test]
    fn accessibility_timeout_requests_input_monitoring_after_120_seconds() {
        let required =
            BTreeSet::from([Capability::ReadAccessibilityTree, Capability::ObserveInput]);
        let requested = RefCell::new(Vec::new());
        let accessibility_checks = Cell::new(0);
        let input_checks = Cell::new(0);
        let elapsed = Cell::new(Duration::ZERO);

        let outcome = request_missing_permissions_with(
            &required,
            |permission| {
                Ok::<_, ()>(match permission {
                    Capability::ReadAccessibilityTree => {
                        accessibility_checks.set(accessibility_checks.get() + 1);
                        if accessibility_checks.get() == 1 {
                            PermissionStatus::NotDetermined
                        } else {
                            PermissionStatus::Denied
                        }
                    }
                    Capability::ObserveInput => {
                        assert_eq!(elapsed.get(), Duration::from_secs(120));
                        input_checks.set(input_checks.get() + 1);
                        if input_checks.get() == 1 {
                            PermissionStatus::NotDetermined
                        } else {
                            PermissionStatus::Granted
                        }
                    }
                    Capability::AutomateBrowser => panic!("automation must not be probed"),
                })
            },
            |permission| {
                requested.borrow_mut().push(*permission);
                Ok::<_, ()>(())
            },
            |duration| {
                if input_checks.get() == 0 {
                    assert_eq!(*requested.borrow(), [Capability::ReadAccessibilityTree]);
                    elapsed.set(elapsed.get() + duration);
                }
            },
            Duration::from_secs(1),
            Duration::from_secs(120),
        )
        .expect("request missing permissions");

        assert_eq!(outcome, PermissionRequestOutcome::TimedOut);
        assert_eq!(accessibility_checks.get(), 121);
        assert_eq!(elapsed.get(), Duration::from_secs(120));
        assert_eq!(
            requested.into_inner(),
            [Capability::ReadAccessibilityTree, Capability::ObserveInput]
        );
    }

    #[test]
    fn granted_permissions_are_skipped_and_automation_remains_lazy() {
        let required = BTreeSet::from([
            Capability::ReadAccessibilityTree,
            Capability::ObserveInput,
            Capability::AutomateBrowser,
        ]);
        let probed = RefCell::new(Vec::new());
        let requested = RefCell::new(Vec::new());

        let outcome = request_missing_permissions_with(
            &required,
            |permission| {
                probed.borrow_mut().push(*permission);
                Ok::<_, ()>(PermissionStatus::Granted)
            },
            |permission| {
                requested.borrow_mut().push(*permission);
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
            [Capability::ReadAccessibilityTree, Capability::ObserveInput]
        );
        assert!(requested.into_inner().is_empty());
    }

    #[test]
    fn derives_typed_snapshot_for_every_required_permission() {
        let required = BTreeSet::from([
            Capability::ReadAccessibilityTree,
            Capability::ObserveInput,
            Capability::AutomateBrowser,
        ]);

        let snapshot = probe_permissions_with(&required, |permission| match permission {
            Capability::ReadAccessibilityTree => Ok::<_, ()>(PermissionStatus::Granted),
            Capability::ObserveInput => Ok(PermissionStatus::Denied),
            Capability::AutomateBrowser => Ok(PermissionStatus::NotDetermined),
        })
        .expect("probe snapshot");

        assert_eq!(
            snapshot,
            DaemonCapabilities::new(
                required,
                CapabilityState::Available,
                CapabilityState::ActionRequired,
                CapabilityState::Deferred,
            )
        );
    }

    #[test]
    fn pending_automation_is_ready_until_the_target_app_can_be_probed() {
        let required = BTreeSet::from([Capability::AutomateBrowser]);

        let snapshot = probe_permissions_with(&required, |permission| match permission {
            Capability::ReadAccessibilityTree | Capability::ObserveInput => {
                Ok::<_, ()>(PermissionStatus::Denied)
            }
            Capability::AutomateBrowser => Ok(PermissionStatus::NotDetermined),
        })
        .expect("probe snapshot");

        assert!(snapshot.ready());
    }
}
