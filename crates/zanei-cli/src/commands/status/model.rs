use std::collections::BTreeMap;
use std::fs;

use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use zanei_core::config::{Config, FilterScope};
use zanei_core::store::{HEARTBEAT_STALE_AFTER_SECONDS, StoreFormat, StoreStatus};

use super::{RETIRED_STORE_DEGRADED_COMPONENT, STORE_DEGRADED_COMPONENT, StoreInspection};
use crate::commands::doctor::permissions_ok;
use crate::commands::filter::ScopeSummary;
use crate::daemon::{StoreOwner, mode_name};
use crate::error::CliError;
use crate::paths::Paths;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StatusState {
    Stopped,
    Running,
    StoreMissing,
    StoreUnavailable,
    StoreCorrupt,
    StoreLocked,
}

impl StatusState {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::StoreMissing => "store_missing",
            Self::StoreUnavailable => "store_unavailable",
            Self::StoreCorrupt => "store_corrupt",
            Self::StoreLocked => "store_locked",
        }
    }

    pub(super) const fn exit_code(self) -> u8 {
        match self {
            Self::Running => super::super::EXIT_SUCCESS,
            Self::Stopped => super::super::EXIT_NO_DAEMON,
            Self::StoreMissing
            | Self::StoreUnavailable
            | Self::StoreCorrupt
            | Self::StoreLocked => super::EXIT_STORE_FAILURE,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct StatusReport {
    pub(super) state: StatusState,
    pub(super) running: bool,
    pub(super) paused: Option<bool>,
    pub(super) since: Option<String>,
    pub(super) instance: Option<String>,
    pub(super) mode: Option<String>,
    pub(super) uptime_s: Option<u64>,
    pub(super) events_captured: Option<u64>,
    pub(super) events_dropped: Option<u64>,
    pub(super) collector_failures: Option<BTreeMap<String, u64>>,
    pub(super) last_event_ts: Option<String>,
    pub(super) heartbeat_freshness: Option<HeartbeatFreshness>,
    pub(super) heartbeat_age_s: Option<u64>,
    pub(super) last_event_age_s: Option<u64>,
    pub(super) store_write_state: Option<StoreWriteState>,
    pub(super) degraded: BTreeMap<String, String>,
    pub(super) store: StoreReport,
    pub(super) capture: CaptureReport,
    pub(super) permissions_ok: bool,
}

impl StatusReport {
    pub(super) fn readable(
        paths: &Paths,
        config: &Config,
        status: &StoreStatus,
        owner: Option<&StoreOwner>,
        inspection: StoreInspection,
    ) -> Result<Self, CliError> {
        let StoreInspection {
            size_bytes,
            oldest_event_ts,
            format,
            retired,
        } = inspection;
        let now = OffsetDateTime::now_utc();
        let heartbeat = parse_status_timestamp("heartbeat_at", status.heartbeat_at.as_deref())?;
        let last_event = parse_status_timestamp("last_event_ts", status.last_event_ts.as_deref())?;
        let heartbeat_age_s = timestamp_age(now, heartbeat);
        let last_event_age_s = timestamp_age(now, last_event);
        let heartbeat_freshness = heartbeat.map_or(HeartbeatFreshness::Missing, |heartbeat| {
            let age = now - heartbeat;
            if age < time::Duration::ZERO {
                HeartbeatFreshness::Future
            } else if age.whole_seconds() <= HEARTBEAT_STALE_AFTER_SECONDS {
                HeartbeatFreshness::Fresh
            } else {
                HeartbeatFreshness::Stale
            }
        });
        let owner_matches_heartbeat = owner
            .is_some_and(|owner| status.instance_id.as_deref() == Some(owner.instance_id.as_str()));
        let owner_active = owner.is_some();
        let last_event_after_heartbeat = heartbeat
            .zip(last_event)
            .is_some_and(|(heartbeat, last_event)| last_event > heartbeat);
        let store_write_state = infer_store_write_state(
            heartbeat_freshness,
            owner_active,
            owner_matches_heartbeat,
            last_event_after_heartbeat,
        );
        let permissions_ok = owner_matches_heartbeat
            .then_some(status.capabilities.as_ref())
            .flatten()
            .map(|capabilities| capabilities.ready())
            .map_or_else(|| permissions_ok(config), Ok)?;
        let owner_fields = owner_fields(owner, now)?;

        Ok(Self {
            state: if owner_active {
                StatusState::Running
            } else {
                StatusState::Stopped
            },
            running: owner_active,
            paused: Some(owner_active && status.paused),
            since: owner_fields.since,
            instance: owner_fields.instance,
            mode: owner_fields.mode,
            uptime_s: owner_fields.uptime_s,
            events_captured: Some(status.events_captured),
            events_dropped: Some(status.events_dropped),
            collector_failures: Some(status.collector_failures.clone()),
            last_event_ts: status.last_event_ts.clone(),
            heartbeat_freshness: Some(heartbeat_freshness),
            heartbeat_age_s,
            last_event_age_s,
            store_write_state: Some(store_write_state),
            degraded: {
                let mut degraded = if owner_matches_heartbeat {
                    status.degraded.clone()
                } else {
                    BTreeMap::new()
                };
                if !retired.skipped.is_empty() {
                    degraded.insert(
                        RETIRED_STORE_DEGRADED_COMPONENT.to_owned(),
                        retired.skipped.join("; "),
                    );
                }
                degraded
            },
            store: StoreReport {
                path: paths.store.display().to_string(),
                size_bytes: Some(size_bytes),
                retention_hours: Some(
                    if owner_matches_heartbeat && heartbeat_freshness == HeartbeatFreshness::Fresh {
                        status
                            .retention_hours
                            .unwrap_or(config.output.retention_hours)
                    } else {
                        config.output.retention_hours
                    },
                ),
                oldest_event_ts,
                encryption: encryption_name(format),
                retired_plaintext: retired.paths,
            },
            capture: CaptureReport::new(config),
            permissions_ok,
        })
    }

    pub(super) fn unreadable(
        paths: &Paths,
        config: &Config,
        owner: Option<&StoreOwner>,
        state: StatusState,
        error: String,
    ) -> Result<Self, CliError> {
        let owner_fields = owner_fields(owner, OffsetDateTime::now_utc())?;
        let mut degraded = BTreeMap::new();
        if !error.is_empty() {
            degraded.insert(STORE_DEGRADED_COMPONENT.to_owned(), error);
        }
        Ok(Self {
            state,
            running: owner.is_some(),
            paused: None,
            since: owner_fields.since,
            instance: owner_fields.instance,
            mode: owner_fields.mode,
            uptime_s: owner_fields.uptime_s,
            events_captured: None,
            events_dropped: None,
            collector_failures: None,
            last_event_ts: None,
            heartbeat_freshness: None,
            heartbeat_age_s: None,
            last_event_age_s: None,
            store_write_state: None,
            degraded,
            store: StoreReport {
                path: paths.store.display().to_string(),
                size_bytes: fs::metadata(&paths.store)
                    .ok()
                    .map(|metadata| metadata.len()),
                retention_hours: None,
                oldest_event_ts: None,
                encryption: StoreFormat::probe(&paths.store)
                    .ok()
                    .and_then(encryption_name),
                retired_plaintext: super::RetiredReport::listed(&paths.store).paths,
            },
            capture: CaptureReport::new(config),
            permissions_ok: permissions_ok(config)?,
        })
    }
}

const fn encryption_name(format: StoreFormat) -> Option<&'static str> {
    match format {
        StoreFormat::Missing | StoreFormat::Unrecognized => None,
        StoreFormat::Plaintext | StoreFormat::Encrypted => Some(format.as_str()),
    }
}

struct OwnerReportFields {
    since: Option<String>,
    instance: Option<String>,
    mode: Option<String>,
    uptime_s: Option<u64>,
}

fn owner_fields(
    owner: Option<&StoreOwner>,
    now: OffsetDateTime,
) -> Result<OwnerReportFields, CliError> {
    let Some(owner) = owner else {
        return Ok(OwnerReportFields {
            since: None,
            instance: None,
            mode: None,
            uptime_s: None,
        });
    };
    let started = OffsetDateTime::parse(&owner.started_at, &Rfc3339).map_err(|error| {
        CliError::InvalidValue(format!(
            "invalid recorder owner started_at {}: {error}",
            owner.started_at
        ))
    })?;
    Ok(OwnerReportFields {
        since: Some(owner.started_at.clone()),
        instance: Some(owner.instance_id.clone()),
        mode: Some(mode_name(&owner.mode).to_owned()),
        uptime_s: Some((now - started).whole_seconds().max(0) as u64),
    })
}

fn parse_status_timestamp(
    field: &'static str,
    value: Option<&str>,
) -> Result<Option<OffsetDateTime>, CliError> {
    value
        .map(|value| {
            OffsetDateTime::parse(value, &Rfc3339).map_err(|error| {
                CliError::InvalidValue(format!("invalid daemon {field} {value}: {error}"))
            })
        })
        .transpose()
}

fn timestamp_age(now: OffsetDateTime, value: Option<OffsetDateTime>) -> Option<u64> {
    value.map(|value| u64::try_from((now - value).whole_seconds().max(0)).unwrap_or(u64::MAX))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum HeartbeatFreshness {
    Fresh,
    Stale,
    Future,
    Missing,
}

impl HeartbeatFreshness {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Future => "future",
            Self::Missing => "missing",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StoreWriteState {
    Healthy,
    SuspectedUnavailable,
    HeartbeatStale,
    Stopped,
}

impl StoreWriteState {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::SuspectedUnavailable => "suspected_unavailable",
            Self::HeartbeatStale => "heartbeat_stale",
            Self::Stopped => "stopped",
        }
    }
}

pub(super) const fn infer_store_write_state(
    freshness: HeartbeatFreshness,
    owner_active: bool,
    owner_matches_heartbeat: bool,
    last_event_after_heartbeat: bool,
) -> StoreWriteState {
    match (
        freshness,
        owner_active,
        owner_matches_heartbeat,
        last_event_after_heartbeat,
    ) {
        (_, false, _, _) => StoreWriteState::Stopped,
        (_, true, false, _) => StoreWriteState::SuspectedUnavailable,
        (HeartbeatFreshness::Fresh, true, true, _) => StoreWriteState::Healthy,
        (_, true, true, false) => StoreWriteState::SuspectedUnavailable,
        _ => StoreWriteState::HeartbeatStale,
    }
}

#[derive(Debug, Serialize)]
pub(super) struct StoreReport {
    pub(super) path: String,
    pub(super) size_bytes: Option<u64>,
    pub(super) retention_hours: Option<u64>,
    pub(super) oldest_event_ts: Option<String>,
    pub(super) encryption: Option<&'static str>,
    pub(super) retired_plaintext: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct CaptureReport {
    pub(super) sources: Vec<&'static str>,
    pub(super) text_content: bool,
    pub(super) content_snapshot: bool,
    #[serde(skip)]
    pub(super) text_scope: CaptureScopeReport,
    #[serde(skip)]
    pub(super) snapshot_scope: CaptureScopeReport,
}

#[derive(Debug)]
pub(super) struct CaptureScopeReport {
    pub(super) apps: String,
    pub(super) sites: String,
}

impl CaptureReport {
    fn new(config: &Config) -> Self {
        let text_scope = ScopeSummary::for_scope(config, FilterScope::TextContent, &[]);
        let snapshot_scope = ScopeSummary::for_scope(config, FilterScope::ContentSnapshot, &[]);
        Self {
            sources: config
                .capture
                .sources
                .iter()
                .map(|source| source.as_str())
                .collect(),
            text_content: config.capture.text_content,
            content_snapshot: config.capture.content_snapshot,
            text_scope: CaptureScopeReport {
                apps: text_scope.status_apps(),
                sites: text_scope.status_sites(),
            },
            snapshot_scope: CaptureScopeReport {
                apps: snapshot_scope.status_apps(),
                sites: snapshot_scope.status_sites(),
            },
        }
    }
}
