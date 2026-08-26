use std::collections::BTreeMap;

use serde::Serialize;
use zanei_core::store::StoreStatus;

use super::super::human_text::sanitize_human_text;
use crate::daemon::StoreOwner;
use crate::error::CliError;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HealthState {
    Healthy,
    Degraded,
    Stopped,
    Stale,
    SuspectedUnavailable,
    StatusUnreadable,
    StatusMissing,
}

impl HealthState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Stopped => "stopped",
            Self::Stale => "stale",
            Self::SuspectedUnavailable => "suspected_unavailable",
            Self::StatusUnreadable => "status_unreadable",
            Self::StatusMissing => "status_missing",
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct HealthReport {
    state: HealthState,
    degraded: Option<BTreeMap<String, String>>,
    collector_failures: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_error: Option<String>,
}

impl HealthReport {
    pub(super) fn from_status(status: &StoreStatus, owner: Option<&StoreOwner>) -> Self {
        let owner_matches_heartbeat = owner
            .is_some_and(|owner| status.instance_id.as_deref() == Some(owner.instance_id.as_str()));
        Self {
            state: match owner {
                None => HealthState::Stopped,
                Some(_) if !owner_matches_heartbeat => HealthState::SuspectedUnavailable,
                Some(_) if !status.running => HealthState::Stale,
                Some(_) if status.degraded.is_empty() => HealthState::Healthy,
                Some(_) => HealthState::Degraded,
            },
            degraded: Some(if owner_matches_heartbeat {
                status.degraded.clone()
            } else {
                BTreeMap::new()
            }),
            collector_failures: Some(status.collector_failures.clone()),
            status_error: None,
        }
    }

    pub(super) const fn status_missing() -> Self {
        Self {
            state: HealthState::StatusMissing,
            degraded: None,
            collector_failures: None,
            status_error: None,
        }
    }

    fn status_unreadable(error: String) -> Self {
        Self {
            state: HealthState::StatusUnreadable,
            degraded: None,
            collector_failures: None,
            status_error: Some(error),
        }
    }

    pub(super) fn render_human(&self) -> String {
        let mut output = format!("COLLECTOR HEALTH  {}\n", self.state.as_str());
        if let Some(degraded) = &self.degraded {
            for (component, reason) in degraded {
                output.push_str(&format!(
                    "  {}: {}\n",
                    sanitize_human_text(component),
                    sanitize_human_text(reason)
                ));
            }
        }
        if let Some(error) = &self.status_error {
            output.push_str(&format!("  status: {}\n", sanitize_human_text(error)));
        }
        match &self.collector_failures {
            Some(failures) if failures.is_empty() => {
                output.push_str("COLLECTOR FAILURES none\n");
            }
            Some(failures) => {
                output.push_str("COLLECTOR FAILURES\n");
                for (component, count) in failures {
                    output.push_str(&format!("  {}: {count}\n", sanitize_human_text(component)));
                }
            }
            None => output.push_str("COLLECTOR FAILURES -\n"),
        }
        output
    }

    pub(super) const fn is_running(&self) -> bool {
        matches!(self.state, HealthState::Healthy | HealthState::Degraded)
    }
}

pub(super) enum StatusRead {
    Readable(StoreStatus),
    Unreadable(CliError),
    Missing,
}

impl StatusRead {
    pub(super) const fn readable(status: StoreStatus) -> Self {
        Self::Readable(status)
    }

    pub(super) fn unreadable(error: CliError) -> Self {
        Self::Unreadable(error)
    }

    pub(super) const fn missing() -> Self {
        Self::Missing
    }

    pub(super) const fn status(&self) -> Option<&StoreStatus> {
        match self {
            Self::Readable(status) => Some(status),
            Self::Unreadable(_) | Self::Missing => None,
        }
    }

    pub(super) fn health_report(&self, owner: Option<&StoreOwner>) -> HealthReport {
        match self {
            Self::Readable(status) => HealthReport::from_status(status, owner),
            Self::Unreadable(error) => HealthReport::status_unreadable(error.to_string()),
            Self::Missing => HealthReport::status_missing(),
        }
    }

    pub(super) fn into_status(self) -> Result<Option<StoreStatus>, CliError> {
        match self {
            Self::Readable(status) => Ok(Some(status)),
            Self::Unreadable(error) => Err(error),
            Self::Missing => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use zanei_collector::Capability;
    use zanei_core::config::Config;
    use zanei_core::store::{DaemonMode, StoreStatus};
    use zanei_core::{CapabilityState, DaemonCapabilities};

    use super::{HealthReport, HealthState};
    use crate::daemon::StoreOwner;

    const CONTROL_TEXT_COMPONENT: &str = "chrome\nforged\r\u{1b}[2J";
    const CONTROL_TEXT_REASON: &str = "failed\r\n\u{1b}[31m";

    #[test]
    fn fresh_heartbeat_without_permission_snapshot_is_pending_for_start() {
        let status = StoreStatus {
            running: true,
            capabilities: None,
            ..StoreStatus::default()
        };

        assert_eq!(
            super::super::evaluate_recorder_for_start(
                &Config::default(),
                &status,
                None,
                Path::new("/tmp/zanei")
            )
            .expect("pending recorder permission snapshot"),
            super::super::StartPermissionState::PendingSnapshot
        );
    }

    #[test]
    fn health_requires_a_current_heartbeat_from_the_store_owner() {
        let owner = store_owner("current");
        let degraded = BTreeMap::from([("chrome".to_owned(), "unavailable".to_owned())]);
        let mut status = StoreStatus {
            running: true,
            instance_id: Some(owner.instance_id.clone()),
            ..StoreStatus::default()
        };

        assert_eq!(
            HealthReport::from_status(&status, Some(&owner)).state,
            HealthState::Healthy
        );
        status.degraded.clone_from(&degraded);
        assert_eq!(
            HealthReport::from_status(&status, Some(&owner)).state,
            HealthState::Degraded
        );

        let stopped = HealthReport::from_status(&status, None);
        assert_eq!(stopped.state, HealthState::Stopped);
        assert_eq!(stopped.degraded, Some(BTreeMap::new()));

        let unavailable = HealthReport::from_status(&status, Some(&store_owner("other")));
        assert_eq!(unavailable.state, HealthState::SuspectedUnavailable);
        assert_eq!(unavailable.degraded, Some(BTreeMap::new()));

        let stale = HealthReport::from_status(
            &StoreStatus {
                running: false,
                instance_id: status.instance_id,
                degraded,
                ..StoreStatus::default()
            },
            Some(&owner),
        );
        assert_eq!(stale.state, HealthState::Stale);
    }

    #[test]
    fn human_health_escapes_controls_without_changing_json_values() {
        let owner = store_owner("current");
        let status = StoreStatus {
            running: true,
            instance_id: Some(owner.instance_id.clone()),
            degraded: BTreeMap::from([(
                CONTROL_TEXT_COMPONENT.to_owned(),
                CONTROL_TEXT_REASON.to_owned(),
            )]),
            collector_failures: BTreeMap::from([(CONTROL_TEXT_COMPONENT.to_owned(), 3)]),
            ..StoreStatus::default()
        };
        let report = HealthReport::from_status(&status, Some(&owner));

        assert!(report.render_human().contains(
            "  chrome\\nforged\\r\\u{1b}[2J: failed\\r\\n\\u{1b}[31m\n\
                     COLLECTOR FAILURES\n  chrome\\nforged\\r\\u{1b}[2J: 3"
        ));
        let json = serde_json::to_value(&report).expect("serialize health report");
        assert_eq!(
            json["degraded"][CONTROL_TEXT_COMPONENT],
            CONTROL_TEXT_REASON
        );
        assert_eq!(json["collector_failures"][CONTROL_TEXT_COMPONENT], 3);

        let unreadable = HealthReport::status_unreadable("bad\n\u{1b}[2J".to_owned());
        assert!(
            unreadable
                .render_human()
                .contains("  status: bad\\n\\u{1b}[2J\n")
        );
        assert_eq!(
            serde_json::to_value(&unreadable).expect("serialize unreadable health")["status_error"],
            "bad\n\u{1b}[2J"
        );
    }

    #[test]
    fn degraded_health_does_not_change_permission_exit_or_fix_policy() {
        let config =
            Config::from_toml("[capture]\nsources = [\"input\"]\n").expect("input capture config");
        let required = crate::daemon::required_capabilities_for(&config);
        let snapshot = DaemonCapabilities::new(
            required.clone(),
            CapabilityState::Available,
            CapabilityState::ActionRequired,
            CapabilityState::Deferred,
        );
        let owner = store_owner("current");
        let status = StoreStatus {
            running: true,
            instance_id: Some(owner.instance_id.clone()),
            degraded: BTreeMap::from([(
                "eventtap".to_owned(),
                "event capture unavailable".to_owned(),
            )]),
            ..StoreStatus::default()
        };
        let report = super::super::build_report(
            &config,
            &required,
            snapshot,
            true,
            super::super::StoreKeyReport::default(),
            HealthReport::from_status(&status, Some(&owner)),
        )
        .expect("degraded permission report");

        assert!(!report.ok);
        assert_eq!(report.missing_permissions, [Capability::ObserveInput]);
        assert_eq!(report.exit_code(), super::super::EXIT_MISSING_PERMISSIONS);
        assert_eq!(
            report.permissions_to_fix(true),
            Some([Capability::ObserveInput].as_slice())
        );
        assert_eq!(report.permissions_to_fix(false), None);
        assert_eq!(
            serde_json::to_value(&report).expect("serialize doctor report")["health"]["state"],
            "degraded"
        );
    }

    fn store_owner(instance_id: &str) -> StoreOwner {
        StoreOwner {
            pid: 42,
            instance_id: instance_id.to_owned(),
            mode: DaemonMode::Foreground,
            started_at: "2026-08-24T00:00:00.000Z".to_owned(),
        }
    }
}
