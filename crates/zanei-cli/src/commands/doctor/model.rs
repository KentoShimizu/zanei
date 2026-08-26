use serde::Serialize;
use zanei_collector::Capability;
use zanei_core::CapabilityState;
use zanei_core::store::LockedReason;
use zanei_macos::permission::{MacOsCapabilityDetail, capability_detail};

use super::health::HealthReport;

#[derive(Debug, Serialize)]
pub(super) struct DoctorReport {
    pub(super) ok: bool,
    pub(super) capture_sources: Vec<&'static str>,
    pub(super) capabilities: CapabilityReport,
    #[serde(skip)]
    pub(super) missing_permissions: Vec<Capability>,
    pub(super) reported_by_recorder: bool,
    pub(super) store_key: StoreKeyReport,
    pub(super) health: HealthReport,
}

impl DoctorReport {
    pub(super) const fn exit_code(&self) -> u8 {
        if self.ok {
            super::EXIT_SUCCESS
        } else {
            super::EXIT_MISSING_PERMISSIONS
        }
    }

    pub(super) fn permissions_to_fix(&self, fix: bool) -> Option<&[Capability]> {
        (fix && !self.missing_permissions.is_empty()).then_some(&self.missing_permissions)
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct StoreKeyReport {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl Default for StoreKeyReport {
    fn default() -> Self {
        Self::new("not_needed", None)
    }
}

impl StoreKeyReport {
    pub(super) const fn new(state: &'static str, detail: Option<String>) -> Self {
        Self { state, detail }
    }

    pub(super) fn from_locked(reason: &LockedReason) -> Self {
        match reason {
            LockedReason::KeyMissing => Self::new("missing", None),
            LockedReason::KeyMismatch => Self::new("mismatch", None),
            LockedReason::KeyStoreLocked(advice) => {
                Self::new("key_store_locked", Some(advice.clone()))
            }
            LockedReason::KeyStoreDenied(advice) => {
                Self::new("key_store_denied", Some(advice.clone()))
            }
            LockedReason::KeyUnavailable(detail) => Self::new("unavailable", Some(detail.clone())),
        }
    }

    pub(super) fn describe(&self) -> String {
        match (self.state, self.detail.as_deref()) {
            ("key_store", Some(location)) => format!("in {location}"),
            ("key_store", None) => "in the platform key store".to_owned(),
            ("not_needed", _) => "not needed yet (the store is not encrypted)".to_owned(),
            ("missing", _) => {
                "missing: the store is encrypted but no key for it is available".to_owned()
            }
            ("mismatch", _) => "does not decrypt this store".to_owned(),
            ("key_store_locked" | "key_store_denied", Some(advice)) => {
                format!("unavailable: {advice}")
            }
            (_, Some(detail)) => format!("unavailable ({detail})"),
            _ => "unavailable".to_owned(),
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct CapabilityReport {
    pub(super) read_accessibility_tree: CapabilityDetail,
    pub(super) observe_input: CapabilityDetail,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) automate_browser: Option<CapabilityDetail>,
}

impl CapabilityReport {
    pub(super) const fn get(&self, capability: Capability) -> Option<&CapabilityDetail> {
        match capability {
            Capability::ReadAccessibilityTree => Some(&self.read_accessibility_tree),
            Capability::ObserveInput => Some(&self.observe_input),
            Capability::AutomateBrowser => self.automate_browser.as_ref(),
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct CapabilityDetail {
    pub(super) state: &'static str,
    pub(super) required: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) required_for: Vec<&'static str>,
    pub(super) detail: PlatformDetail,
}

impl CapabilityDetail {
    pub(super) fn new(
        capability: Capability,
        state: CapabilityState,
        required: bool,
        required_for: Vec<&'static str>,
    ) -> Self {
        Self {
            state: capability_state_name(state),
            required,
            required_for,
            detail: PlatformDetail::new(capability_detail(capability, state)),
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct PlatformDetail {
    platform: &'static str,
    pub(super) permission: &'static str,
    pub(super) status: &'static str,
    pub(super) settings_url: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) target_bundle_id: Option<&'static str>,
}

impl PlatformDetail {
    fn new(detail: MacOsCapabilityDetail) -> Self {
        Self {
            platform: detail.platform,
            permission: detail.permission.as_str(),
            status: detail.status.as_str(),
            settings_url: detail.settings_url,
            target_bundle_id: detail.target_bundle_id,
        }
    }
}

const fn capability_state_name(state: CapabilityState) -> &'static str {
    match state {
        CapabilityState::Available => "available",
        CapabilityState::ActionRequired => "action_required",
        CapabilityState::Deferred => "deferred",
    }
}
