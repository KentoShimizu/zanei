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

pub const SAFARI_BUNDLE_ID: &str = "com.apple.Safari";

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
        Capability::AutomateSafari => (
            MacOsPermission::Automation,
            AUTOMATION_SETTINGS_URL,
            Some(SAFARI_BUNDLE_ID),
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
        Capability::AutomateBrowser | Capability::AutomateSafari => {
            let bundle_id = match capability {
                Capability::AutomateBrowser => CHROME_BUNDLE_ID,
                Capability::AutomateSafari => SAFARI_BUNDLE_ID,
                Capability::ReadAccessibilityTree | Capability::ObserveInput => unreachable!(),
            };
            let status = probe.automation_status(bundle_id)?;
            match status {
                AE_PERMISSION_GRANTED => Ok(PermissionStatus::Granted),
                AE_PERMISSION_DENIED => Ok(PermissionStatus::Denied),
                // AEDeterminePermissionToAutomateTarget cannot inspect TCC while the target is
                // not running. Report the conservative non-granted state and recheck on launch.
                AE_PERMISSION_NOT_DETERMINED | AE_TARGET_NOT_RUNNING => {
                    Ok(PermissionStatus::NotDetermined)
                }
                status => Err(PermissionError::UnexpectedAutomationStatus {
                    bundle_id: bundle_id.to_owned(),
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
mod tests;
