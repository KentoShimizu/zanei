use std::{collections::BTreeSet, thread, time::Duration};

use zanei_collector::Capability;
use zanei_core::{CapabilityState, DaemonCapabilities};
use zanei_macos::permission::{
    PermissionChecker, PermissionError, PermissionStatus, capture_screen_state,
    request_accessibility, request_input_monitoring, request_screen_recording,
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

/// Requests the missing required permissions one dialog at a time and, when `screen_recording`
/// is set, then the optional Screen Recording permission. Runs only in the recorder, whose TCC
/// identity the requests belong to.
pub(crate) fn request_missing_permissions(
    required: &BTreeSet<Capability>,
    screen_recording: bool,
) -> Result<PermissionRequestOutcome, PermissionError> {
    let checker = PermissionChecker::new();
    request_missing_permissions_with(
        required,
        screen_recording.then_some(capture_screen_state),
        |capability| permission_request_status(&checker, capability),
        |capability| match capability {
            Capability::ReadAccessibilityTree => request_accessibility(),
            Capability::ObserveInput => {
                request_input_monitoring();
                Ok(())
            }
            Capability::AutomateBrowser | Capability::AutomateSafari => {
                unreachable!("browser automation is requested by use")
            }
            Capability::CaptureScreen => {
                request_screen_recording();
                Ok(())
            }
        },
        thread::sleep,
        PERMISSION_DECISION_POLL_INTERVAL,
        PERMISSION_DECISION_TIMEOUT,
    )
}

fn request_missing_permissions_with<E>(
    required: &BTreeSet<Capability>,
    screen_capture_state: Option<impl FnOnce() -> CapabilityState>,
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
    // Optional and last, so its dialog never covers a required one. A grant applies only after
    // a restart, so there is nothing to wait for; below macOS 14 there is nothing to ask for.
    if screen_capture_state.is_some_and(|state| state() == CapabilityState::ActionRequired) {
        request(&Capability::CaptureScreen)?;
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
    let capabilities = DaemonCapabilities::new(
        required.clone(),
        capability_state(accessibility),
        capability_state(input_monitoring),
        capability_state(automation),
    );
    Ok(if required.contains(&Capability::AutomateSafari) {
        capabilities
            .with_automate_safari(capability_state(status_for(&Capability::AutomateSafari)?))
    } else {
        capabilities
    })
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

    const NO_SCREEN_RECORDING: Option<fn() -> CapabilityState> = None;

    #[test]
    fn false_accessibility_status_does_not_advance_before_timeout() {
        let required =
            BTreeSet::from([Capability::ReadAccessibilityTree, Capability::ObserveInput]);
        let requested = RefCell::new(Vec::new());
        let accessibility_checks = Cell::new(0);
        let input_checks = Cell::new(0);

        let outcome = request_missing_permissions_with(
            &required,
            NO_SCREEN_RECORDING,
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
                    Capability::AutomateBrowser
                    | Capability::AutomateSafari
                    | Capability::CaptureScreen => {
                        panic!("automation must not be probed")
                    }
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
            NO_SCREEN_RECORDING,
            |permission| {
                let checks = match permission {
                    Capability::ReadAccessibilityTree => &accessibility_checks,
                    Capability::ObserveInput => &input_checks,
                    Capability::AutomateBrowser
                    | Capability::AutomateSafari
                    | Capability::CaptureScreen => {
                        panic!("automation must not be probed")
                    }
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
            NO_SCREEN_RECORDING,
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
                    Capability::AutomateBrowser
                    | Capability::AutomateSafari
                    | Capability::CaptureScreen => {
                        panic!("automation must not be probed")
                    }
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
            NO_SCREEN_RECORDING,
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
    fn screen_recording_is_requested_once_after_required_steps_settle_without_waiting() {
        // Catches its dialog colliding with a pending required one, the sequence blocking on a
        // grant that applies only after a restart, and a timed-out step skipping it.
        let required =
            BTreeSet::from([Capability::ReadAccessibilityTree, Capability::ObserveInput]);
        let requested = RefCell::new(Vec::new());
        let input_checks = Cell::new(0);

        let outcome = request_missing_permissions_with(
            &required,
            Some(|| CapabilityState::ActionRequired),
            |permission| {
                Ok::<_, ()>(match permission {
                    Capability::ReadAccessibilityTree => PermissionStatus::NotDetermined,
                    Capability::ObserveInput => {
                        input_checks.set(input_checks.get() + 1);
                        if input_checks.get() == 1 {
                            PermissionStatus::NotDetermined
                        } else {
                            PermissionStatus::Granted
                        }
                    }
                    Capability::AutomateBrowser
                    | Capability::AutomateSafari
                    | Capability::CaptureScreen => panic!("only required steps are probed"),
                })
            },
            |permission| {
                requested.borrow_mut().push(*permission);
                Ok::<_, ()>(())
            },
            |_| assert!(!requested.borrow().contains(&Capability::CaptureScreen)),
            Duration::from_secs(1),
            Duration::from_secs(3),
        )
        .expect("request permissions");

        assert_eq!(outcome, PermissionRequestOutcome::TimedOut);
        assert_eq!(
            requested.into_inner(),
            [
                Capability::ReadAccessibilityTree,
                Capability::ObserveInput,
                Capability::CaptureScreen,
            ]
        );
    }

    #[test]
    fn screen_recording_is_not_requested_when_granted_unavailable_or_snapshots_are_off() {
        // Catches a dialog for a permission already held, one below macOS 14 where it cannot be
        // used, and one for users who never turned content snapshots on.
        let required = BTreeSet::from([Capability::ReadAccessibilityTree]);
        let request = |screen_state: Option<CapabilityState>| {
            let requested = RefCell::new(Vec::new());
            request_missing_permissions_with(
                &required,
                screen_state.map(|state| move || state),
                |_| Ok::<_, ()>(PermissionStatus::Granted),
                |permission| {
                    requested.borrow_mut().push(*permission);
                    Ok(())
                },
                |_| panic!("granted permissions are not polled"),
                Duration::from_secs(1),
                Duration::from_secs(3),
            )
            .expect("request permissions");
            requested.into_inner()
        };

        assert_eq!(request(None), []);
        assert_eq!(request(Some(CapabilityState::Available)), []);
        assert_eq!(request(Some(CapabilityState::Unavailable)), []);
        assert_eq!(
            request(Some(CapabilityState::ActionRequired)),
            [Capability::CaptureScreen]
        );
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
            Capability::AutomateBrowser
            | Capability::AutomateSafari
            | Capability::CaptureScreen => Ok(PermissionStatus::NotDetermined),
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
            Capability::AutomateBrowser
            | Capability::AutomateSafari
            | Capability::CaptureScreen => Ok(PermissionStatus::NotDetermined),
        })
        .expect("probe snapshot");

        assert!(snapshot.ready());
    }

    #[test]
    fn probes_required_safari_without_requesting_it() {
        let required = BTreeSet::from([Capability::AutomateSafari]);
        let probed = RefCell::new(Vec::new());
        let snapshot = probe_permissions_with(&required, |permission| {
            probed.borrow_mut().push(*permission);
            Ok::<_, ()>(PermissionStatus::NotDetermined)
        })
        .expect("probe Safari capability");

        assert_eq!(
            *probed.borrow(),
            [
                Capability::ReadAccessibilityTree,
                Capability::ObserveInput,
                Capability::AutomateSafari,
            ]
        );
        assert_eq!(
            snapshot.state(Capability::AutomateSafari),
            CapabilityState::Deferred
        );
        assert!(snapshot.ready());
    }

    #[test]
    fn unrequired_safari_is_never_probed_or_requested() {
        let required = BTreeSet::from([Capability::ReadAccessibilityTree]);
        let probed = RefCell::new(Vec::new());
        let requested = RefCell::new(Vec::new());
        let snapshot = probe_permissions_with(&required, |permission| {
            probed.borrow_mut().push(*permission);
            Ok::<_, ()>(PermissionStatus::Granted)
        })
        .expect("probe shared capabilities");

        assert!(snapshot.state(Capability::AutomateSafari) == CapabilityState::Deferred);
        assert!(!probed.borrow().contains(&Capability::AutomateSafari));

        request_missing_permissions_with(
            &required,
            NO_SCREEN_RECORDING,
            |_| Ok::<_, ()>(PermissionStatus::Granted),
            |permission| {
                requested.borrow_mut().push(*permission);
                Ok(())
            },
            |_| panic!("granted permission must not be requested"),
            Duration::from_secs(1),
            Duration::from_secs(3),
        )
        .expect("request shared permissions");
        assert!(!requested.borrow().contains(&Capability::AutomateSafari));
    }

    #[test]
    fn probes_chrome_and_safari_automation_as_independent_targets() {
        let required = BTreeSet::from([Capability::AutomateBrowser, Capability::AutomateSafari]);
        let snapshot = probe_permissions_with(&required, |permission| match permission {
            Capability::ReadAccessibilityTree
            | Capability::ObserveInput
            | Capability::CaptureScreen => Ok::<_, ()>(PermissionStatus::Granted),
            Capability::AutomateBrowser => Ok(PermissionStatus::Denied),
            Capability::AutomateSafari => Ok(PermissionStatus::Granted),
        })
        .expect("probe browser capabilities");

        assert_eq!(
            snapshot.state(Capability::AutomateBrowser),
            CapabilityState::ActionRequired
        );
        assert_eq!(
            snapshot.state(Capability::AutomateSafari),
            CapabilityState::Available
        );
        assert!(!snapshot.ready());
    }
}
