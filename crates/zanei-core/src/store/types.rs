use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const HEARTBEAT_STALE_AFTER_SECONDS: i64 = 15;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DaemonState {
    pub pid: Option<i64>,
    pub started_at: Option<String>,
    pub instance_id: Option<String>,
    pub mode: Option<DaemonMode>,
    pub heartbeat_at: Option<String>,
    pub retention_hours: Option<u64>,
    pub paused_until: Option<String>,
    pub events_captured: u64,
    pub events_dropped: u64,
    pub last_event_ts: Option<String>,
    pub degraded: BTreeMap<String, String>,
    pub collector_failures: BTreeMap<String, u64>,
    pub permissions: Option<DaemonPermissions>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StoreStatus {
    pub running: bool,
    pub paused: bool,
    pub pid: Option<i64>,
    pub started_at: Option<String>,
    pub instance_id: Option<String>,
    pub mode: Option<DaemonMode>,
    pub heartbeat_at: Option<String>,
    pub retention_hours: Option<u64>,
    pub paused_until: Option<String>,
    pub events_captured: u64,
    pub events_dropped: u64,
    pub last_event_ts: Option<String>,
    pub degraded: BTreeMap<String, String>,
    pub collector_failures: BTreeMap<String, u64>,
    pub permissions: Option<DaemonPermissions>,
    pub last_known_permissions: Option<DaemonPermissions>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonMode {
    Foreground,
    Launchd,
}

impl DaemonMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Launchd => "launchd",
        }
    }
}

impl StoreStatus {
    #[must_use]
    pub const fn effective_retention_hours(&self, configured_retention_hours: u64) -> u64 {
        if self.running {
            match self.retention_hours {
                Some(retention_hours) => retention_hours,
                None => configured_retention_hours,
            }
        } else {
            configured_retention_hours
        }
    }

    #[must_use]
    pub fn reported_permissions(&self) -> Option<&DaemonPermissions> {
        self.running.then_some(self.permissions.as_ref()).flatten()
    }

    /// Returns the last permission snapshot reported by the recorder, even after it stops.
    #[must_use]
    pub fn last_reported_permissions(&self) -> Option<&DaemonPermissions> {
        self.last_known_permissions.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct DaemonPermissions {
    pub permissions_ok: bool,
    pub accessibility: PermissionState,
    pub input_monitoring: PermissionState,
    pub automation: BTreeMap<String, PermissionState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    Granted,
    Denied,
    NotDetermined,
}
