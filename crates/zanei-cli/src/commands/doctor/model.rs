use std::collections::BTreeMap;

use serde::Serialize;
use zanei_collector::Permission;
use zanei_core::store::LockedReason;

#[derive(Debug, Serialize)]
pub(super) struct DoctorReport {
    pub(super) ok: bool,
    pub(super) capture_sources: Vec<&'static str>,
    pub(super) permissions: PermissionReport,
    pub(super) missing_required: Vec<&'static str>,
    pub(super) settings_pane: Option<&'static str>,
    #[serde(skip)]
    pub(super) missing_permissions: Vec<Permission>,
    pub(super) reported_by_recorder: bool,
    pub(super) store_key: StoreKeyReport,
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

#[derive(Debug, Default, Serialize)]
pub(super) struct PermissionReport {
    pub(super) accessibility: StatusDetail,
    pub(super) input_monitoring: StatusDetail,
    pub(super) automation: AutomationDetail,
}

#[derive(Debug, Default, Serialize)]
pub(super) struct StatusDetail {
    pub(super) status: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) required_for: Vec<&'static str>,
}

#[derive(Debug, Default, Serialize)]
pub(super) struct AutomationDetail {
    pub(super) per_app: BTreeMap<String, &'static str>,
}
