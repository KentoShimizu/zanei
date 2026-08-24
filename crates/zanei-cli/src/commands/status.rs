use std::fs;

use zanei_core::config::Config;
use zanei_core::store::{StoreFailureKind, StoreFormat, retired_plaintext_stores};

use crate::daemon::{StoreOwner, StoreOwnership};
use crate::error::CliError;
use crate::paths::Paths;
use crate::store_access::{self, KeyPrompt};

mod model;
mod render;

use model::{StatusReport, StatusState};
use render::print_human;

#[cfg(test)]
use super::{EXIT_NO_DAEMON, EXIT_SUCCESS};
#[cfg(test)]
use model::{HeartbeatFreshness, StoreWriteState, infer_store_write_state};

const EXIT_STORE_FAILURE: u8 = 1;
const STORE_DEGRADED_COMPONENT: &str = "store";
pub(crate) const RETIRED_STORE_DEGRADED_COMPONENT: &str = "retired_store";

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
    let reader = match store_access::open_reader(&paths.store, KeyPrompt::Allowed) {
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
    let retired = RetiredReport {
        paths: reader
            .retired_stores()
            .iter()
            .map(|retired| retired.path.display().to_string())
            .collect(),
        skipped: reader
            .skipped_retired()
            .iter()
            .map(zanei_core::store::SkippedRetired::describe)
            .collect(),
    };
    StatusReport::readable(
        paths,
        config,
        &status,
        owner,
        StoreInspection {
            size_bytes,
            oldest_event_ts,
            format: reader.format(),
            retired,
        },
    )
}

struct StoreInspection {
    size_bytes: u64,
    oldest_event_ts: Option<String>,
    format: StoreFormat,
    retired: RetiredReport,
}

#[derive(Debug, Default)]
struct RetiredReport {
    paths: Vec<String>,
    skipped: Vec<String>,
}

impl RetiredReport {
    fn listed(store: &std::path::Path) -> Self {
        Self {
            paths: retired_plaintext_stores(store)
                .map(|retired| {
                    retired
                        .into_iter()
                        .map(|retired| retired.path.display().to_string())
                        .collect()
                })
                .unwrap_or_default(),
            skipped: Vec::new(),
        }
    }
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
                StoreFailureKind::Locked => StatusState::StoreLocked,
            };
            StatusReport::unreadable(paths, config, owner, state, error.to_string())
        }
    }
}

#[cfg(test)]
mod tests;
