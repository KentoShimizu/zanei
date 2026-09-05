use std::{
    cell::RefCell,
    io,
    sync::mpsc::sync_channel,
    thread,
    time::{Duration, Instant},
};

use super::{
    ACCESSIBILITY_SETTINGS_URL, AE_PERMISSION_GRANTED, AE_PERMISSION_NOT_DETERMINED,
    AUTOMATION_SETTINGS_URL, AutomationProbeState, AutomationTargetError, Capability,
    CapabilityState, INPUT_MONITORING_SETTINGS_URL, MacOsPermission, PermissionError,
    PermissionProbe, PermissionStatus, SAFARI_BUNDLE_ID, SettingsOpener,
    automation_status_with_timeout, automation_target_error, capability_detail, open_settings_with,
    permission_status_with,
};

struct StubPermissionProbe {
    accessibility_trusted: bool,
    input_status: i32,
    automation_status: Result<i32, AutomationTargetError>,
}

impl PermissionProbe for StubPermissionProbe {
    fn accessibility_is_trusted(&self) -> bool {
        self.accessibility_trusted
    }

    fn input_monitoring_status(&self) -> i32 {
        self.input_status
    }

    fn automation_status(&self, bundle_id: &str) -> Result<i32, PermissionError> {
        assert!(matches!(
            bundle_id,
            zanei_core::privacy::CHROME_BUNDLE_ID | SAFARI_BUNDLE_ID
        ));
        self.automation_status
            .map_err(|error| automation_target_error(bundle_id, error))
    }
}

impl SettingsOpener for RefCell<Vec<&'static str>> {
    fn open(&self, settings_url: &'static str) -> Result<(), PermissionError> {
        self.borrow_mut().push(settings_url);
        Ok(())
    }
}

fn probe_with(
    accessibility_trusted: bool,
    input_status: i32,
    automation_status: Result<i32, AutomationTargetError>,
) -> StubPermissionProbe {
    StubPermissionProbe {
        accessibility_trusted,
        input_status,
        automation_status,
    }
}

fn assert_status(probe: StubPermissionProbe, cap: Capability, want: PermissionStatus) {
    assert_eq!(permission_status_with(&probe, &cap).unwrap(), want);
}

#[test]
#[rustfmt::skip]
fn maps_all_native_permission_statuses() {
    use {Capability::{AutomateBrowser, AutomateSafari, ObserveInput, ReadAccessibilityTree}, PermissionStatus::{Denied, Granted, NotDetermined}};
    assert_status(probe_with(true, 0, Ok(0)), ReadAccessibilityTree, Granted);
    assert_status(probe_with(false, 0, Ok(0)), ReadAccessibilityTree, Denied);
    for (raw, expected) in [(0, Granted), (1, Denied), (2, NotDetermined)] {
        assert_status(probe_with(false, raw, Ok(0)), ObserveInput, expected);
    }
    for capability in [AutomateBrowser, AutomateSafari] {
        for (raw, expected) in [(0, Granted), (-1_743, Denied), (-1_744, NotDetermined), (-600, NotDetermined)] {
            assert_status(probe_with(false, 0, Ok(raw)), capability, expected);
        }
    }
    assert!(matches!(
        permission_status_with(&probe_with(false, 99, Ok(0)), &ObserveInput),
        Err(PermissionError::UnexpectedInputMonitoringStatus { status: 99 })
    ));
    let descriptor_failure = probe_with(false, 0, Err(AutomationTargetError::CreateFailed { status: -1_708 }));
    assert!(matches!(
        permission_status_with(&descriptor_failure, &AutomateSafari),
        Err(PermissionError::AutomationTargetCreation { bundle_id, status: -1_708 })
            if bundle_id == SAFARI_BUNDLE_ID
    ));
}

#[test]
#[rustfmt::skip]
fn describes_each_capability_and_state_for_macos() {
    use {Capability::{AutomateBrowser, AutomateSafari, ObserveInput, ReadAccessibilityTree}, CapabilityState::{ActionRequired, Available, Deferred}, PermissionStatus::{Denied, Granted, NotDetermined}};
    for capability in [ReadAccessibilityTree, ObserveInput, AutomateBrowser, AutomateSafari] {
        for (state, status) in [(Available, Granted), (ActionRequired, Denied), (Deferred, NotDetermined)] {
            assert_eq!(capability_detail(capability, state).status, status);
        }
    }
    let accessibility = capability_detail(ReadAccessibilityTree, Available);
    assert_eq!(accessibility.platform, "macos");
    assert_eq!(accessibility.permission, MacOsPermission::Accessibility);
    assert_eq!(accessibility.settings_url, ACCESSIBILITY_SETTINGS_URL);
    assert_eq!(accessibility.target_bundle_id, None);
    let input = capability_detail(ObserveInput, Available);
    assert_eq!(input.permission, MacOsPermission::InputMonitoring);
    assert_eq!(input.settings_url, INPUT_MONITORING_SETTINGS_URL);
    assert_eq!(input.target_bundle_id, None);
    let automation = capability_detail(AutomateBrowser, Available);
    assert_eq!(automation.permission, MacOsPermission::Automation);
    assert_eq!(automation.settings_url, AUTOMATION_SETTINGS_URL);
    assert_eq!(automation.target_bundle_id, Some(zanei_core::privacy::CHROME_BUNDLE_ID));
    let safari = capability_detail(AutomateSafari, Available);
    assert_eq!(safari.target_bundle_id, Some(SAFARI_BUNDLE_ID));
    assert_eq!(MacOsPermission::InputMonitoring.as_str(), "input_monitoring");
    assert_eq!(NotDetermined.as_str(), "not_determined");
}

#[test]
fn timed_out_automation_probe_is_pending_without_duplicate_workers() {
    const BUNDLE_ID: &str = "com.google.Chrome";
    let state = AutomationProbeState::default();
    let (started_sender, started_receiver) = sync_channel(1);
    let (release_sender, release_receiver) = sync_channel(1);

    let status =
        automation_status_with_timeout(&state, BUNDLE_ID, Duration::from_millis(10), move || {
            started_sender.send(()).expect("test should be listening");
            release_receiver.recv().expect("test should release probe");
            Ok(AE_PERMISSION_GRANTED)
        })
        .unwrap();

    assert_eq!(status, AE_PERMISSION_NOT_DETERMINED);
    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("probe worker should start");
    assert_eq!(
        automation_status_with_timeout(&state, BUNDLE_ID, Duration::from_secs(1), || panic!(
            "an in-flight bundle must not start another worker"
        ),)
        .unwrap(),
        AE_PERMISSION_NOT_DETERMINED
    );

    release_sender
        .send(())
        .expect("probe should still be running");
    let release_deadline = Instant::now() + Duration::from_secs(1);
    while state.is_in_flight(BUNDLE_ID) {
        assert!(
            Instant::now() < release_deadline,
            "completed worker should release its bundle"
        );
        thread::yield_now();
    }
    assert_eq!(
        automation_status_with_timeout(&state, BUNDLE_ID, Duration::from_secs(1), || Ok(
            AE_PERMISSION_GRANTED
        ),)
        .unwrap(),
        AE_PERMISSION_GRANTED
    );
}

#[test]
fn opens_the_permission_specific_settings_pane() {
    let opener = RefCell::default();

    open_settings_with(&opener, &Capability::ReadAccessibilityTree).unwrap();
    open_settings_with(&opener, &Capability::ObserveInput).unwrap();
    open_settings_with(&opener, &Capability::AutomateBrowser).unwrap();
    open_settings_with(&opener, &Capability::AutomateSafari).unwrap();

    assert_eq!(
        *opener.borrow(),
        [
            ACCESSIBILITY_SETTINGS_URL,
            INPUT_MONITORING_SETTINGS_URL,
            AUTOMATION_SETTINGS_URL,
            AUTOMATION_SETTINGS_URL,
        ]
    );
}

#[test]
fn settings_opener_errors_are_not_hidden() {
    struct FailingOpener;

    impl SettingsOpener for FailingOpener {
        fn open(&self, settings_url: &'static str) -> Result<(), PermissionError> {
            Err(PermissionError::SettingsLaunch {
                settings_url,
                source: io::Error::other("test failure"),
            })
        }
    }

    assert!(matches!(
        open_settings_with(&FailingOpener, &Capability::ReadAccessibilityTree),
        Err(PermissionError::SettingsLaunch {
            settings_url: ACCESSIBILITY_SETTINGS_URL,
            ..
        })
    ));
}
