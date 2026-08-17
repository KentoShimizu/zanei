//! macOS TCC permission diagnostics and System Settings navigation.

use std::{io, process::Command};

use thiserror::Error;
use zanei_collector::Permission;

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
        permission: &Permission,
    ) -> Result<PermissionStatus, PermissionError> {
        permission_status_with(&NativePermissionProbe, permission)
    }

    pub fn open_settings(&self, permission: &Permission) -> Result<(), PermissionError> {
        open_settings_with(&ProcessSettingsOpener, permission)
    }
}

pub fn permission_status(permission: &Permission) -> Result<PermissionStatus, PermissionError> {
    PermissionChecker::new().permission_status(permission)
}

pub fn open_settings(permission: &Permission) -> Result<(), PermissionError> {
    PermissionChecker::new().open_settings(permission)
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
    fn automation_status(&self, bundle_id: &str) -> Result<i32, AutomationTargetError>;
}

struct NativePermissionProbe;

impl PermissionProbe for NativePermissionProbe {
    fn accessibility_is_trusted(&self) -> bool {
        accessibility_is_trusted()
    }

    fn input_monitoring_status(&self) -> i32 {
        input_monitoring_status()
    }

    fn automation_status(&self, bundle_id: &str) -> Result<i32, AutomationTargetError> {
        AutomationTarget::new(bundle_id).map(|target| target.permission_status())
    }
}

fn permission_status_with(
    probe: &impl PermissionProbe,
    permission: &Permission,
) -> Result<PermissionStatus, PermissionError> {
    match permission {
        Permission::Accessibility => Ok(if probe.accessibility_is_trusted() {
            PermissionStatus::Granted
        } else {
            // AXIsProcessTrusted exposes only a Boolean and cannot distinguish a first request
            // from an explicit denial.
            PermissionStatus::Denied
        }),
        Permission::InputMonitoring => match probe.input_monitoring_status() {
            IO_HID_ACCESS_GRANTED => Ok(PermissionStatus::Granted),
            IO_HID_ACCESS_DENIED => Ok(PermissionStatus::Denied),
            IO_HID_ACCESS_UNKNOWN => Ok(PermissionStatus::NotDetermined),
            status => Err(PermissionError::UnexpectedInputMonitoringStatus { status }),
        },
        Permission::Automation { bundle_id } => {
            let status = probe
                .automation_status(bundle_id)
                .map_err(|error| automation_target_error(bundle_id, error))?;
            match status {
                AE_PERMISSION_GRANTED => Ok(PermissionStatus::Granted),
                AE_PERMISSION_DENIED => Ok(PermissionStatus::Denied),
                // AEDeterminePermissionToAutomateTarget cannot inspect TCC while the target is
                // not running. Report the conservative non-granted state and recheck on launch.
                AE_PERMISSION_NOT_DETERMINED | AE_TARGET_NOT_RUNNING => {
                    Ok(PermissionStatus::NotDetermined)
                }
                status => Err(PermissionError::UnexpectedAutomationStatus {
                    bundle_id: bundle_id.clone(),
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
    permission: &Permission,
) -> Result<(), PermissionError> {
    let settings_url = match permission {
        Permission::Accessibility => ACCESSIBILITY_SETTINGS_URL,
        Permission::InputMonitoring => INPUT_MONITORING_SETTINGS_URL,
        Permission::Automation { .. } => AUTOMATION_SETTINGS_URL,
    };
    opener.open(settings_url)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, io};

    use super::{
        ACCESSIBILITY_SETTINGS_URL, AUTOMATION_SETTINGS_URL, AutomationTargetError,
        INPUT_MONITORING_SETTINGS_URL, Permission, PermissionError, PermissionProbe,
        PermissionStatus, SettingsOpener, open_settings_with, permission_status_with,
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

        fn automation_status(&self, _bundle_id: &str) -> Result<i32, AutomationTargetError> {
            self.automation_status
        }
    }

    #[derive(Default)]
    struct RecordingOpener {
        opened_urls: RefCell<Vec<&'static str>>,
    }

    impl SettingsOpener for RecordingOpener {
        fn open(&self, settings_url: &'static str) -> Result<(), PermissionError> {
            self.opened_urls.borrow_mut().push(settings_url);
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

    #[test]
    fn maps_accessibility_boolean_to_granted_or_denied() {
        let trusted = probe_with(true, 0, Ok(0));
        let untrusted = probe_with(false, 0, Ok(0));

        assert_eq!(
            permission_status_with(&trusted, &Permission::Accessibility).unwrap(),
            PermissionStatus::Granted
        );
        assert_eq!(
            permission_status_with(&untrusted, &Permission::Accessibility).unwrap(),
            PermissionStatus::Denied
        );
    }

    #[test]
    fn maps_all_input_monitoring_statuses() {
        let permission = Permission::InputMonitoring;

        assert_eq!(
            permission_status_with(&probe_with(false, 0, Ok(0)), &permission).unwrap(),
            PermissionStatus::Granted
        );
        assert_eq!(
            permission_status_with(&probe_with(false, 1, Ok(0)), &permission).unwrap(),
            PermissionStatus::Denied
        );
        assert_eq!(
            permission_status_with(&probe_with(false, 2, Ok(0)), &permission).unwrap(),
            PermissionStatus::NotDetermined
        );
        assert!(matches!(
            permission_status_with(&probe_with(false, 99, Ok(0)), &permission),
            Err(PermissionError::UnexpectedInputMonitoringStatus { status: 99 })
        ));
    }

    #[test]
    fn maps_all_automation_statuses() {
        let permission = Permission::Automation {
            bundle_id: "com.google.Chrome".to_owned(),
        };

        assert_eq!(
            permission_status_with(&probe_with(false, 0, Ok(0)), &permission).unwrap(),
            PermissionStatus::Granted
        );
        assert_eq!(
            permission_status_with(&probe_with(false, 0, Ok(-1_743)), &permission).unwrap(),
            PermissionStatus::Denied
        );
        assert_eq!(
            permission_status_with(&probe_with(false, 0, Ok(-1_744)), &permission).unwrap(),
            PermissionStatus::NotDetermined
        );
        assert_eq!(
            permission_status_with(&probe_with(false, 0, Ok(-600)), &permission).unwrap(),
            PermissionStatus::NotDetermined
        );
    }

    #[test]
    fn preserves_automation_descriptor_failures() {
        let permission = Permission::Automation {
            bundle_id: "com.google.Chrome".to_owned(),
        };
        let probe = probe_with(
            false,
            0,
            Err(AutomationTargetError::CreateFailed { status: -1_708 }),
        );

        assert!(matches!(
            permission_status_with(&probe, &permission),
            Err(PermissionError::AutomationTargetCreation {
                bundle_id,
                status: -1_708
            }) if bundle_id == "com.google.Chrome"
        ));
    }

    #[test]
    fn opens_the_permission_specific_settings_pane() {
        let opener = RecordingOpener::default();

        open_settings_with(&opener, &Permission::Accessibility).unwrap();
        open_settings_with(&opener, &Permission::InputMonitoring).unwrap();
        open_settings_with(
            &opener,
            &Permission::Automation {
                bundle_id: "com.google.Chrome".to_owned(),
            },
        )
        .unwrap();

        assert_eq!(
            *opener.opened_urls.borrow(),
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
            open_settings_with(&FailingOpener, &Permission::Accessibility),
            Err(PermissionError::SettingsLaunch {
                settings_url: ACCESSIBILITY_SETTINGS_URL,
                ..
            })
        ));
    }
}
