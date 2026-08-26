//! macOS TCC permission diagnostics and System Settings navigation.

use std::{
    collections::HashSet,
    io,
    process::Command,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        mpsc::{RecvTimeoutError, sync_channel},
    },
    thread,
    time::Duration,
};

use thiserror::Error;
use zanei_collector::Capability;
use zanei_core::CapabilityState;
use zanei_core::privacy::CHROME_BUNDLE_ID;

use crate::ffi::permission::{
    AutomationTarget, AutomationTargetError, accessibility_is_trusted, input_monitoring_status,
    request_accessibility as request_accessibility_ffi,
    request_input_monitoring as request_input_monitoring_ffi,
};

const IO_HID_ACCESS_GRANTED: i32 = 0;
const IO_HID_ACCESS_DENIED: i32 = 1;
const IO_HID_ACCESS_UNKNOWN: i32 = 2;

const AE_PERMISSION_GRANTED: i32 = 0;
const AE_PERMISSION_DENIED: i32 = -1_743;
const AE_PERMISSION_NOT_DETERMINED: i32 = -1_744;
const AE_TARGET_NOT_RUNNING: i32 = -600;

// macOS permission dialogs are user-paced and can stall TCC indefinitely. Two seconds leaves
// ample time for normal local IPC while keeping the probe below the CLI's 10-second liveness wait.
const AUTOMATION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

const ACCESSIBILITY_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
const INPUT_MONITORING_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent";
const AUTOMATION_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation";
const OPEN_EXECUTABLE: &str = "/usr/bin/open";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionStatus {
    Granted,
    Denied,
    NotDetermined,
}

impl PermissionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::NotDetermined => "not_determined",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MacOsPermission {
    Accessibility,
    InputMonitoring,
    Automation,
}

impl MacOsPermission {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accessibility => "accessibility",
            Self::InputMonitoring => "input_monitoring",
            Self::Automation => "automation",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MacOsCapabilityDetail {
    pub platform: &'static str,
    pub permission: MacOsPermission,
    pub status: PermissionStatus,
    pub settings_url: &'static str,
    pub target_bundle_id: Option<&'static str>,
}

pub const fn capability_detail(
    capability: Capability,
    state: CapabilityState,
) -> MacOsCapabilityDetail {
    let (permission, settings_url, target_bundle_id) = match capability {
        Capability::ReadAccessibilityTree => (
            MacOsPermission::Accessibility,
            ACCESSIBILITY_SETTINGS_URL,
            None,
        ),
        Capability::ObserveInput => (
            MacOsPermission::InputMonitoring,
            INPUT_MONITORING_SETTINGS_URL,
            None,
        ),
        Capability::AutomateBrowser => (
            MacOsPermission::Automation,
            AUTOMATION_SETTINGS_URL,
            Some(CHROME_BUNDLE_ID),
        ),
    };
    MacOsCapabilityDetail {
        platform: "macos",
        permission,
        status: match state {
            CapabilityState::Available => PermissionStatus::Granted,
            CapabilityState::ActionRequired => PermissionStatus::Denied,
            CapabilityState::Deferred => PermissionStatus::NotDetermined,
        },
        settings_url,
        target_bundle_id,
    }
}

#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("failed to create Accessibility permission request options")]
    AccessibilityRequestOptionsCreation,
    #[error("input monitoring returned unknown IOHID access status {status}")]
    UnexpectedInputMonitoringStatus { status: i32 },
    #[error("automation bundle ID is too long: {byte_count} bytes")]
    AutomationBundleIdTooLong { byte_count: usize },
    #[error("failed to create an Apple Event target descriptor for {bundle_id}: OSStatus {status}")]
    AutomationTargetCreation { bundle_id: String, status: i16 },
    #[error("automation permission check for {bundle_id} returned OSStatus {status}")]
    UnexpectedAutomationStatus { bundle_id: String, status: i32 },
    #[error("failed to start automation permission probe for {bundle_id}: {source}")]
    AutomationProbeThreadSpawn {
        bundle_id: String,
        #[source]
        source: io::Error,
    },
    #[error("automation permission probe for {bundle_id} stopped without a result")]
    AutomationProbeWorkerStopped { bundle_id: String },
    #[error("failed to launch System Settings for {settings_url}: {source}")]
    SettingsLaunch {
        settings_url: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("System Settings opener failed for {settings_url} with status {status}")]
    SettingsOpen {
        settings_url: &'static str,
        status: std::process::ExitStatus,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PermissionChecker;

impl PermissionChecker {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    pub fn permission_status(
        &self,
        capability: &Capability,
    ) -> Result<PermissionStatus, PermissionError> {
        permission_status_with(&NativePermissionProbe, capability)
    }

    pub fn open_settings(&self, capability: &Capability) -> Result<(), PermissionError> {
        open_settings_with(&ProcessSettingsOpener, capability)
    }
}

pub fn permission_status(capability: &Capability) -> Result<PermissionStatus, PermissionError> {
    PermissionChecker::new().permission_status(capability)
}

pub fn open_settings(capability: &Capability) -> Result<(), PermissionError> {
    PermissionChecker::new().open_settings(capability)
}

pub fn request_accessibility() -> Result<(), PermissionError> {
    request_accessibility_ffi()
        .ok_or(PermissionError::AccessibilityRequestOptionsCreation)
        .map(|_| ())
}

pub fn request_input_monitoring() {
    let _ = request_input_monitoring_ffi();
}

trait PermissionProbe {
    fn accessibility_is_trusted(&self) -> bool;
    fn input_monitoring_status(&self) -> i32;
    fn automation_status(&self, bundle_id: &str) -> Result<i32, PermissionError>;
}

struct NativePermissionProbe;

impl PermissionProbe for NativePermissionProbe {
    fn accessibility_is_trusted(&self) -> bool {
        accessibility_is_trusted()
    }

    fn input_monitoring_status(&self) -> i32 {
        input_monitoring_status()
    }

    fn automation_status(&self, bundle_id: &str) -> Result<i32, PermissionError> {
        let worker_bundle_id = bundle_id.to_owned();
        automation_status_with_timeout(
            native_automation_probe_state(),
            bundle_id,
            AUTOMATION_PROBE_TIMEOUT,
            move || {
                AutomationTarget::new(&worker_bundle_id).map(|target| target.permission_status())
            },
        )
    }
}

#[derive(Clone, Default)]
struct AutomationProbeState {
    in_flight: Arc<Mutex<HashSet<String>>>,
}

impl AutomationProbeState {
    fn begin(&self, bundle_id: &str) -> Option<AutomationProbeGuard> {
        let mut in_flight = self.lock_in_flight();
        in_flight
            .insert(bundle_id.to_owned())
            .then(|| AutomationProbeGuard {
                state: self.clone(),
                bundle_id: bundle_id.to_owned(),
            })
    }

    fn finish(&self, bundle_id: &str) {
        self.lock_in_flight().remove(bundle_id);
    }

    fn lock_in_flight(&self) -> MutexGuard<'_, HashSet<String>> {
        // The native probe never runs while this lock is held. If a membership operation panics,
        // the safe HashSet remains usable, so recovering the guard prevents a bundle from being
        // stranded in-flight for the rest of the process.
        self.in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    fn is_in_flight(&self, bundle_id: &str) -> bool {
        self.lock_in_flight().contains(bundle_id)
    }
}

struct AutomationProbeGuard {
    state: AutomationProbeState,
    bundle_id: String,
}

impl Drop for AutomationProbeGuard {
    fn drop(&mut self) {
        self.state.finish(&self.bundle_id);
    }
}

fn native_automation_probe_state() -> &'static AutomationProbeState {
    static STATE: OnceLock<AutomationProbeState> = OnceLock::new();
    STATE.get_or_init(AutomationProbeState::default)
}

fn automation_status_with_timeout(
    state: &AutomationProbeState,
    bundle_id: &str,
    timeout: Duration,
    probe: impl FnOnce() -> Result<i32, AutomationTargetError> + Send + 'static,
) -> Result<i32, PermissionError> {
    let Some(in_flight) = state.begin(bundle_id) else {
        return Ok(AE_PERMISSION_NOT_DETERMINED);
    };
    let (result_sender, result_receiver) = sync_channel(1);

    drop(
        thread::Builder::new()
            .name("zanei-automation-permission".to_owned())
            .spawn(move || {
                let result = probe();
                drop(in_flight);
                let _ = result_sender.send(result);
            })
            .map_err(|source| PermissionError::AutomationProbeThreadSpawn {
                bundle_id: bundle_id.to_owned(),
                source,
            })?,
    );

    match result_receiver.recv_timeout(timeout) {
        Ok(result) => result.map_err(|error| automation_target_error(bundle_id, error)),
        Err(RecvTimeoutError::Timeout) => Ok(AE_PERMISSION_NOT_DETERMINED),
        Err(RecvTimeoutError::Disconnected) => Err(PermissionError::AutomationProbeWorkerStopped {
            bundle_id: bundle_id.to_owned(),
        }),
    }
}

fn permission_status_with(
    probe: &impl PermissionProbe,
    capability: &Capability,
) -> Result<PermissionStatus, PermissionError> {
    match capability {
        Capability::ReadAccessibilityTree => Ok(if probe.accessibility_is_trusted() {
            PermissionStatus::Granted
        } else {
            // AXIsProcessTrusted exposes only a Boolean and cannot distinguish a first request
            // from an explicit denial.
            PermissionStatus::Denied
        }),
        Capability::ObserveInput => match probe.input_monitoring_status() {
            IO_HID_ACCESS_GRANTED => Ok(PermissionStatus::Granted),
            IO_HID_ACCESS_DENIED => Ok(PermissionStatus::Denied),
            IO_HID_ACCESS_UNKNOWN => Ok(PermissionStatus::NotDetermined),
            status => Err(PermissionError::UnexpectedInputMonitoringStatus { status }),
        },
        Capability::AutomateBrowser => {
            let status = probe.automation_status(CHROME_BUNDLE_ID)?;
            match status {
                AE_PERMISSION_GRANTED => Ok(PermissionStatus::Granted),
                AE_PERMISSION_DENIED => Ok(PermissionStatus::Denied),
                // AEDeterminePermissionToAutomateTarget cannot inspect TCC while the target is
                // not running. Report the conservative non-granted state and recheck on launch.
                AE_PERMISSION_NOT_DETERMINED | AE_TARGET_NOT_RUNNING => {
                    Ok(PermissionStatus::NotDetermined)
                }
                status => Err(PermissionError::UnexpectedAutomationStatus {
                    bundle_id: CHROME_BUNDLE_ID.to_owned(),
                    status,
                }),
            }
        }
    }
}

fn automation_target_error(bundle_id: &str, error: AutomationTargetError) -> PermissionError {
    match error {
        AutomationTargetError::BundleIdTooLong { byte_count } => {
            PermissionError::AutomationBundleIdTooLong { byte_count }
        }
        AutomationTargetError::CreateFailed { status } => {
            PermissionError::AutomationTargetCreation {
                bundle_id: bundle_id.to_owned(),
                status,
            }
        }
    }
}

trait SettingsOpener {
    fn open(&self, settings_url: &'static str) -> Result<(), PermissionError>;
}

struct ProcessSettingsOpener;

impl SettingsOpener for ProcessSettingsOpener {
    fn open(&self, settings_url: &'static str) -> Result<(), PermissionError> {
        let status = Command::new(OPEN_EXECUTABLE)
            .arg(settings_url)
            .status()
            .map_err(|source| PermissionError::SettingsLaunch {
                settings_url,
                source,
            })?;
        if !status.success() {
            return Err(PermissionError::SettingsOpen {
                settings_url,
                status,
            });
        }
        Ok(())
    }
}

fn open_settings_with(
    opener: &impl SettingsOpener,
    capability: &Capability,
) -> Result<(), PermissionError> {
    opener.open(capability_detail(*capability, CapabilityState::Available).settings_url)
}

#[cfg(test)]
mod tests {
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
        PermissionProbe, PermissionStatus, SettingsOpener, automation_status_with_timeout,
        automation_target_error, capability_detail, open_settings_with, permission_status_with,
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
            assert_eq!(bundle_id, zanei_core::privacy::CHROME_BUNDLE_ID);
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
        use {Capability::{AutomateBrowser, ObserveInput, ReadAccessibilityTree}, PermissionStatus::{Denied, Granted, NotDetermined}};
        assert_status(probe_with(true, 0, Ok(0)), ReadAccessibilityTree, Granted);
        assert_status(probe_with(false, 0, Ok(0)), ReadAccessibilityTree, Denied);
        for (raw, expected) in [(0, Granted), (1, Denied), (2, NotDetermined)] {
            assert_status(probe_with(false, raw, Ok(0)), ObserveInput, expected);
        }
        for (raw, expected) in [(0, Granted), (-1_743, Denied), (-1_744, NotDetermined), (-600, NotDetermined)] {
            assert_status(probe_with(false, 0, Ok(raw)), AutomateBrowser, expected);
        }
        assert!(matches!(
            permission_status_with(&probe_with(false, 99, Ok(0)), &ObserveInput),
            Err(PermissionError::UnexpectedInputMonitoringStatus { status: 99 })
        ));
        let descriptor_failure = probe_with(false, 0, Err(AutomationTargetError::CreateFailed { status: -1_708 }));
        assert!(matches!(
            permission_status_with(&descriptor_failure, &AutomateBrowser),
            Err(PermissionError::AutomationTargetCreation { bundle_id, status: -1_708 })
                if bundle_id == "com.google.Chrome"
        ));
    }

    #[test]
    #[rustfmt::skip]
    fn describes_each_capability_and_state_for_macos() {
        use {Capability::{AutomateBrowser, ObserveInput, ReadAccessibilityTree}, CapabilityState::{ActionRequired, Available, Deferred}, PermissionStatus::{Denied, Granted, NotDetermined}};
        for capability in [ReadAccessibilityTree, ObserveInput, AutomateBrowser] {
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
        assert_eq!(MacOsPermission::InputMonitoring.as_str(), "input_monitoring");
        assert_eq!(NotDetermined.as_str(), "not_determined");
    }

    #[test]
    fn timed_out_automation_probe_is_pending_without_duplicate_workers() {
        const BUNDLE_ID: &str = "com.google.Chrome";
        let state = AutomationProbeState::default();
        let (started_sender, started_receiver) = sync_channel(1);
        let (release_sender, release_receiver) = sync_channel(1);

        let status = automation_status_with_timeout(
            &state,
            BUNDLE_ID,
            Duration::from_millis(10),
            move || {
                started_sender.send(()).expect("test should be listening");
                release_receiver.recv().expect("test should release probe");
                Ok(AE_PERMISSION_GRANTED)
            },
        )
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

        assert_eq!(
            *opener.borrow(),
            [
                ACCESSIBILITY_SETTINGS_URL,
                INPUT_MONITORING_SETTINGS_URL,
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
}
