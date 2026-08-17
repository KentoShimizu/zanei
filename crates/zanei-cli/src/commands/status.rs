use std::{collections::BTreeMap, fmt::Display, fs};

use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use zanei_core::{
    config::Config,
    store::{HEARTBEAT_STALE_AFTER_SECONDS, StoreFailureKind, StoreReader, StoreStatus},
};

use super::doctor::permissions_ok;
use super::{EXIT_NO_DAEMON, EXIT_SUCCESS};
use crate::{
    daemon::{StoreOwner, StoreOwnership, mode_name},
    error::CliError,
    paths::Paths,
};

const EXIT_STORE_FAILURE: u8 = 1;
const STORE_DEGRADED_COMPONENT: &str = "store";

pub fn run(paths: &Paths, json: bool) -> Result<u8, CliError> {
    let owner = StoreOwnership::probe(&paths.store)?;
    let config = Config::load(&paths.config)?;
    let report = inspect(paths, &config, owner.as_ref())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report);
    }
    Ok(report.state.exit_code())
}

fn inspect(
    paths: &Paths,
    config: &Config,
    owner: Option<&StoreOwner>,
) -> Result<StatusReport, CliError> {
    match paths.store.try_exists() {
        Ok(false) => return missing_report(paths, config, owner),
        Err(error) => {
            return StatusReport::unreadable(
                paths,
                config,
                owner,
                StatusState::StoreUnavailable,
                error.to_string(),
            );
        }
        Ok(true) => {}
    }

    let size_bytes = match fs::metadata(&paths.store) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            return StatusReport::unreadable(
                paths,
                config,
                owner,
                StatusState::StoreUnavailable,
                error.to_string(),
            );
        }
    };
    let reader = match StoreReader::open(&paths.store) {
        Ok(reader) => reader,
        Err(error) => return store_error_report(paths, config, owner, &error),
    };
    let status = match reader.status() {
        Ok(status) => status,
        Err(error) => return store_error_report(paths, config, owner, &error),
    };
    let oldest_event_ts = match reader.oldest_event_ts() {
        Ok(timestamp) => timestamp,
        Err(error) => return store_error_report(paths, config, owner, &error),
    };
    StatusReport::readable(paths, config, &status, owner, size_bytes, oldest_event_ts)
}

fn missing_report(
    paths: &Paths,
    config: &Config,
    owner: Option<&StoreOwner>,
) -> Result<StatusReport, CliError> {
    match owner {
        Some(_) => StatusReport::unreadable(
            paths,
            config,
            owner,
            StatusState::StoreMissing,
            "store file is missing while the recorder owns the store".to_owned(),
        ),
        None => StatusReport::unreadable(paths, config, owner, StatusState::Stopped, String::new()),
    }
}

fn store_error_report(
    paths: &Paths,
    config: &Config,
    owner: Option<&StoreOwner>,
    error: &zanei_core::store::StoreError,
) -> Result<StatusReport, CliError> {
    match paths.store.try_exists() {
        Ok(false) => missing_report(paths, config, owner),
        Err(existence_error) => StatusReport::unreadable(
            paths,
            config,
            owner,
            StatusState::StoreUnavailable,
            existence_error.to_string(),
        ),
        Ok(true) => {
            let state = match error.failure_kind() {
                StoreFailureKind::Unavailable => StatusState::StoreUnavailable,
                StoreFailureKind::Corrupt => StatusState::StoreCorrupt,
            };
            StatusReport::unreadable(paths, config, owner, state, error.to_string())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StatusState {
    Stopped,
    Running,
    StoreMissing,
    StoreUnavailable,
    StoreCorrupt,
}

impl StatusState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::StoreMissing => "store_missing",
            Self::StoreUnavailable => "store_unavailable",
            Self::StoreCorrupt => "store_corrupt",
        }
    }

    const fn exit_code(self) -> u8 {
        match self {
            Self::Running => EXIT_SUCCESS,
            Self::Stopped => EXIT_NO_DAEMON,
            Self::StoreMissing | Self::StoreUnavailable | Self::StoreCorrupt => EXIT_STORE_FAILURE,
        }
    }
}

#[derive(Debug, Serialize)]
struct StatusReport {
    state: StatusState,
    running: bool,
    paused: Option<bool>,
    since: Option<String>,
    instance: Option<String>,
    mode: Option<String>,
    uptime_s: Option<u64>,
    events_captured: Option<u64>,
    events_dropped: Option<u64>,
    collector_failures: Option<BTreeMap<String, u64>>,
    last_event_ts: Option<String>,
    heartbeat_freshness: Option<HeartbeatFreshness>,
    heartbeat_age_s: Option<u64>,
    last_event_age_s: Option<u64>,
    store_write_state: Option<StoreWriteState>,
    degraded: BTreeMap<String, String>,
    store: StoreReport,
    capture: CaptureReport,
    permissions_ok: bool,
}

impl StatusReport {
    fn readable(
        paths: &Paths,
        config: &Config,
        status: &StoreStatus,
        owner: Option<&StoreOwner>,
        size_bytes: u64,
        oldest_event_ts: Option<String>,
    ) -> Result<Self, CliError> {
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
            .then_some(status.permissions.as_ref())
            .flatten()
            .map(|permissions| permissions.permissions_ok)
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
            degraded: if owner_matches_heartbeat {
                status.degraded.clone()
            } else {
                BTreeMap::new()
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
            },
            capture: CaptureReport::new(config),
            permissions_ok,
        })
    }

    fn unreadable(
        paths: &Paths,
        config: &Config,
        owner: Option<&StoreOwner>,
        state: StatusState,
        error: String,
    ) -> Result<Self, CliError> {
        let now = OffsetDateTime::now_utc();
        let owner_fields = owner_fields(owner, now)?;
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
            },
            capture: CaptureReport::new(config),
            permissions_ok: permissions_ok(config)?,
        })
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
enum HeartbeatFreshness {
    Fresh,
    Stale,
    Future,
    Missing,
}

impl HeartbeatFreshness {
    const fn as_str(self) -> &'static str {
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
enum StoreWriteState {
    Healthy,
    SuspectedUnavailable,
    HeartbeatStale,
    Stopped,
}

impl StoreWriteState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::SuspectedUnavailable => "suspected_unavailable",
            Self::HeartbeatStale => "heartbeat_stale",
            Self::Stopped => "stopped",
        }
    }
}

const fn infer_store_write_state(
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
struct StoreReport {
    path: String,
    size_bytes: Option<u64>,
    retention_hours: Option<u64>,
    oldest_event_ts: Option<String>,
}

#[derive(Debug, Serialize)]
struct CaptureReport {
    sources: Vec<&'static str>,
    text_content: bool,
}

impl CaptureReport {
    fn new(config: &Config) -> Self {
        Self {
            sources: config
                .capture
                .sources
                .iter()
                .map(|source| source.as_str())
                .collect(),
            text_content: config.capture.text_content,
        }
    }
}

fn print_human(report: &StatusReport) {
    println!("STATE             {}", report.state.as_str());
    println!("PAUSED            {}", display_optional(report.paused));
    println!(
        "SINCE             {}",
        display_optional(report.since.as_deref())
    );
    println!(
        "INSTANCE          {}",
        display_optional(report.instance.as_deref())
    );
    println!(
        "MODE              {}",
        display_optional(report.mode.as_deref())
    );
    println!(
        "EVENTS CAPTURED   {}",
        display_optional(report.events_captured)
    );
    println!(
        "EVENTS DROPPED    {}",
        display_optional(report.events_dropped)
    );
    println!(
        "LAST EVENT        {}",
        display_optional(report.last_event_ts.as_deref())
    );
    println!("HEARTBEAT         {}", heartbeat_text(report));
    println!(
        "STORE WRITES      {}",
        report
            .store_write_state
            .map_or("-", StoreWriteState::as_str)
    );
    println!("STORE             {}", report.store.path);
    print_text_content(report.capture.text_content);
    println!("PERMISSIONS OK    {}", report.permissions_ok);
    if report.degraded.is_empty() {
        println!("DEGRADED          false");
    } else {
        println!("DEGRADED          true");
        for (component, reason) in &report.degraded {
            println!("  {component}: {reason}");
        }
    }
}

fn display_optional<T: Display>(value: Option<T>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

fn heartbeat_text(report: &StatusReport) -> String {
    report.heartbeat_freshness.map_or_else(
        || "-".to_owned(),
        |freshness| {
            format!(
                "{}{}",
                freshness.as_str(),
                report
                    .heartbeat_age_s
                    .map(|age| format!(" ({age}s old)"))
                    .unwrap_or_default()
            )
        },
    )
}

fn print_text_content(enabled: bool) {
    let status = text_content_status(enabled);
    if enabled {
        println!("TEXT CONTENT      {status} (opt-in)");
    } else {
        println!("TEXT CONTENT      {status} (opt-in: zanei config set capture.text_content true)");
    }
}

const fn text_content_status(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

#[cfg(test)]
mod tests;
