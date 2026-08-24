use std::collections::BTreeMap;

use serde::Serialize;
use zanei_core::store::StoreStatus;

use crate::error::CliError;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HealthState {
    Healthy,
    Degraded,
    StatusUnreadable,
    StatusMissing,
}

impl HealthState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
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
    pub(super) fn from_status(status: &StoreStatus) -> Self {
        Self {
            state: if status.degraded.is_empty() {
                HealthState::Healthy
            } else {
                HealthState::Degraded
            },
            degraded: Some(status.degraded.clone()),
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
                output.push_str(&format!("  {component}: {reason}\n"));
            }
        }
        if let Some(error) = &self.status_error {
            output.push_str(&format!("  status: {error}\n"));
        }
        match &self.collector_failures {
            Some(failures) if failures.is_empty() => {
                output.push_str("COLLECTOR FAILURES none\n");
            }
            Some(failures) => {
                output.push_str("COLLECTOR FAILURES\n");
                for (component, count) in failures {
                    output.push_str(&format!("  {component}: {count}\n"));
                }
            }
            None => output.push_str("COLLECTOR FAILURES -\n"),
        }
        output
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

    pub(super) fn health_report(&self) -> HealthReport {
        match self {
            Self::Readable(status) => HealthReport::from_status(status),
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
