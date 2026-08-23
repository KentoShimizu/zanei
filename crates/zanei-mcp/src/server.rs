use std::path::PathBuf;

use rmcp::ErrorData;
use rmcp::handler::server::{
    router::tool::ToolRouter,
    wrapper::{Json, Parameters},
};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use zanei_core::config::{Config, parse_time_expression};
use zanei_core::normalize::format_timestamp;
use zanei_core::store::{QueryFilter, QueryResult, StoreError, StoreReader};
use zanei_core::timeline::{
    Granularity, MIN_TIMELINE_TOKEN_BUDGET_TOKENS, TimeRange, TimelineFormat, TimelineOptions,
    build, serialize,
};

use crate::{KeyProvider, PermissionCheck};

mod output;

use output::{CaptureOutput, GetStatusOutput, GetTimelineOutput, QueryEventsOutput, RangeOutput};

const DEFAULT_TIMELINE_SINCE: &str = "1h";
const DEFAULT_QUERY_SINCE: &str = "15m";
const DEFAULT_TIMELINE_TOKEN_BUDGET: usize = 4_000;
const DEFAULT_QUERY_LIMIT: usize = 200;
const MAX_QUERY_LIMIT: usize = 1_000;

/// `degraded` component naming set-aside stores a read left out; the CLI's
/// `status` uses the same name.
const RETIRED_STORE_DEGRADED_COMPONENT: &str = "retired_store";

#[derive(Clone, Debug)]
pub struct ZaneiServer {
    store_path: PathBuf,
    config_path: PathBuf,
    permission_check: PermissionCheck,
    key_provider: KeyProvider,
    tool_router: ToolRouter<Self>,
}

impl ZaneiServer {
    pub(crate) fn new(
        store_path: PathBuf,
        config_path: PathBuf,
        permission_check: PermissionCheck,
        key_provider: KeyProvider,
    ) -> Self {
        Self {
            store_path,
            config_path,
            permission_check,
            key_provider,
            tool_router: Self::tool_router(),
        }
    }

    /// Opens the store for this call. A missing store is "nothing recorded yet"
    /// (`None`); an encrypted store whose key cannot be obtained is an error, so
    /// "locked" is never mistaken for "empty".
    fn reader(&self) -> Result<Option<StoreReader>, ErrorData> {
        if !self.store_path.exists() {
            return Ok(None);
        }
        let key = (self.key_provider)(&self.store_path).map_err(internal_error)?;
        StoreReader::open_with_key(&self.store_path, key.as_ref())
            .map(Some)
            .map_err(internal_error)
    }

    fn retention_hours(&self) -> Result<u64, ErrorData> {
        Config::load(&self.config_path)
            .map(|config| config.output.retention_hours)
            .map_err(internal_error)
    }
}

#[tool_router]
impl ZaneiServer {
    #[tool(
        description = "Return an LLM-ready timeline for a time range",
        annotations(read_only_hint = true)
    )]
    fn get_timeline(
        &self,
        Parameters(input): Parameters<GetTimelineInput>,
    ) -> Result<Json<GetTimelineOutput>, ErrorData> {
        let (since, until) = parse_range(&input.since, &input.until)?;
        if input.token_budget < MIN_TIMELINE_TOKEN_BUDGET_TOKENS {
            return Err(invalid_params(format!(
                "token_budget must be at least {MIN_TIMELINE_TOKEN_BUDGET_TOKENS}"
            )));
        }
        let retention_hours = self.retention_hours()?;
        let (result, snapshot_metadata) = match self.reader()? {
            Some(reader) => {
                let result = reader
                    .query(
                        &QueryFilter {
                            since: Some(since.clone()),
                            until: Some(until.clone()),
                            ..QueryFilter::default()
                        },
                        retention_hours,
                    )
                    .map_err(store_error)?;
                let metadata = reader
                    .query_metadata(&zanei_core::store::MetadataFilter {
                        since: Some(since.clone()),
                        until: Some(until.clone()),
                        types: vec!["content.snapshot".to_owned()],
                        app: None,
                        bundle_id: None,
                        configured_retention_hours: retention_hours,
                    })
                    .map_err(store_error)?;
                (result, metadata)
            }
            None => (QueryResult::default(), Vec::new()),
        };
        let core_format = match input.format {
            TimelineOutputFormat::Markdown => TimelineFormat::Markdown,
            TimelineOutputFormat::Structured => TimelineFormat::Json,
        };
        let timeline = build(
            &result.events,
            &snapshot_metadata,
            &TimelineOptions {
                range: TimeRange { since, until },
                token_budget: input.token_budget,
                granularity: match input.granularity {
                    TimelineGranularity::Coarse => Granularity::Coarse,
                    TimelineGranularity::Fine => Granularity::Fine,
                },
                format: core_format,
            },
        )
        .map_err(internal_error)?;

        let output = match input.format {
            TimelineOutputFormat::Markdown => {
                let content =
                    serialize(&timeline, TimelineFormat::Markdown).map_err(internal_error)?;
                GetTimelineOutput::markdown(
                    timeline.range,
                    content,
                    timeline.token_estimate,
                    timeline.truncated,
                )
            }
            TimelineOutputFormat::Structured => GetTimelineOutput::structured(
                timeline.range,
                timeline.sessions,
                timeline.token_estimate,
                timeline.truncated,
                result.skipped_unknown_types,
            ),
        };
        Ok(Json(output))
    }

    #[tool(
        description = "Return raw recorded events matching query filters",
        output_schema = output::query_events_output_schema(),
        annotations(read_only_hint = true)
    )]
    fn query_events(
        &self,
        Parameters(input): Parameters<QueryEventsInput>,
    ) -> Result<Json<QueryEventsOutput>, ErrorData> {
        validate_limit(input.limit)?;
        let (since, until) = parse_range(&input.since, &input.until)?;
        let fetch_limit = input.limit + 1;
        let filter = QueryFilter {
            since: Some(since.clone()),
            until: Some(until.clone()),
            types: input.types,
            app: input.app,
            bundle_id: input.bundle_id,
            limit: Some(fetch_limit),
        };
        filter.validate().map_err(store_error)?;
        let retention_hours = self.retention_hours()?;
        let mut result = match self.reader()? {
            Some(reader) => reader
                .query(&filter, retention_hours)
                .map_err(store_error)?,
            None => QueryResult::default(),
        };
        let truncated = result.events.len() > input.limit;
        result.events.truncate(input.limit);

        Ok(Json(QueryEventsOutput {
            range: RangeOutput { since, until },
            count: result.events.len(),
            truncated,
            skipped_unknown_types: result.skipped_unknown_types,
            events: result.events,
        }))
    }

    #[tool(
        description = "Return recording, capture, permission, and retention status",
        annotations(read_only_hint = true)
    )]
    fn get_status(&self) -> Result<Json<GetStatusOutput>, ErrorData> {
        let config = Config::load(&self.config_path).map_err(internal_error)?;
        let (status, oldest_event_ts) = match self.reader()? {
            Some(reader) => {
                let mut status = reader.status().map_err(internal_error)?;
                // A set-aside plaintext store this reader could not attach is
                // missing from every read; say so here, as the CLI's `status` does.
                if !reader.skipped_retired().is_empty() {
                    let summary = reader
                        .skipped_retired()
                        .iter()
                        .map(zanei_core::store::SkippedRetired::describe)
                        .collect::<Vec<_>>()
                        .join("; ");
                    status
                        .degraded
                        .insert(RETIRED_STORE_DEGRADED_COMPONENT.to_owned(), summary);
                }
                let oldest_event_ts = reader.oldest_event_ts().map_err(internal_error)?;
                (status, oldest_event_ts)
            }
            None => (Default::default(), None),
        };
        let permissions_ok = status
            .reported_permissions()
            .map(|permissions| permissions.permissions_ok)
            .map_or_else(
                || (self.permission_check)(&config).map_err(internal_error),
                Ok,
            )?;
        let retention_hours = status.effective_retention_hours(config.output.retention_hours);

        Ok(Json(GetStatusOutput {
            running: status.running,
            paused: status.paused,
            last_event_ts: status.last_event_ts,
            events_dropped: status.events_dropped,
            degraded: status.degraded,
            collector_failures: status.collector_failures,
            retention_hours,
            oldest_event_ts,
            capture: CaptureOutput {
                sources: config
                    .capture
                    .sources
                    .into_iter()
                    .map(|source| source.as_str().to_owned())
                    .collect(),
                text_content: config.capture.text_content,
                content_snapshot: config.capture.content_snapshot,
            },
            permissions_ok,
        }))
    }
}

#[tool_handler]
impl ServerHandler for ZaneiServer {
    fn get_info(&self) -> ServerInfo {
        let server_info = Implementation {
            name: "zanei".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            ..Default::default()
        };
        ServerInfo {
            instructions: Some("Read-only access to the local Zanei activity store".into()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info,
            ..Default::default()
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
struct GetTimelineInput {
    /// Start of the range as a relative duration or RFC3339 timestamp.
    since: String,
    /// End of the range as `now`, a relative duration, or an RFC3339 timestamp.
    until: String,
    /// Timeline output shape.
    format: TimelineOutputFormat,
    /// Approximate maximum number of LLM tokens in the result.
    #[schemars(range(min = MIN_TIMELINE_TOKEN_BUDGET_TOKENS))]
    token_budget: usize,
    /// Session-level or per-interaction detail.
    granularity: TimelineGranularity,
}

impl Default for GetTimelineInput {
    fn default() -> Self {
        Self {
            since: DEFAULT_TIMELINE_SINCE.to_owned(),
            until: "now".to_owned(),
            format: TimelineOutputFormat::Markdown,
            token_budget: DEFAULT_TIMELINE_TOKEN_BUDGET,
            granularity: TimelineGranularity::Coarse,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum TimelineOutputFormat {
    #[default]
    Markdown,
    Structured,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum TimelineGranularity {
    #[default]
    Coarse,
    Fine,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
struct QueryEventsInput {
    /// Start of the range as a relative duration or RFC3339 timestamp.
    since: String,
    /// End of the range as `now`, a relative duration, or an RFC3339 timestamp.
    until: String,
    /// Exact event types or family wildcards such as `browser.*`.
    types: Vec<String>,
    /// Exact application name to match.
    app: Option<String>,
    /// Exact application bundle identifier to match.
    bundle_id: Option<String>,
    /// Maximum number of events to return, from 1 through 1000.
    #[schemars(range(min = 1, max = 1_000))]
    limit: usize,
}

impl Default for QueryEventsInput {
    fn default() -> Self {
        Self {
            since: DEFAULT_QUERY_SINCE.to_owned(),
            until: "now".to_owned(),
            types: Vec::new(),
            app: None,
            bundle_id: None,
            limit: DEFAULT_QUERY_LIMIT,
        }
    }
}

fn parse_range(since: &str, until: &str) -> Result<(String, String), ErrorData> {
    let now = OffsetDateTime::now_utc();
    let since = parse_time_expression(since, now).map_err(invalid_params)?;
    let until = parse_time_expression(until, now).map_err(invalid_params)?;
    if since > until {
        return Err(invalid_params("since must not be later than until"));
    }
    Ok((format_timestamp(since), format_timestamp(until)))
}

fn validate_limit(limit: usize) -> Result<(), ErrorData> {
    if (1..=MAX_QUERY_LIMIT).contains(&limit) {
        Ok(())
    } else {
        Err(invalid_params(format!(
            "limit must be between 1 and {MAX_QUERY_LIMIT}"
        )))
    }
}

fn store_error(error: StoreError) -> ErrorData {
    match error {
        StoreError::InvalidTypePattern(_) => invalid_params(error),
        _ => internal_error(error),
    }
}

fn invalid_params(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::invalid_params(error.to_string(), None)
}

fn internal_error(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::{MAX_QUERY_LIMIT, parse_range, validate_limit};

    #[test]
    fn rejects_invalid_ranges_and_query_limits() {
        assert!(parse_range("bogus", "now").is_err());
        assert!(parse_range("2026-08-16T10:00:00Z", "2026-08-16T09:00:00Z").is_err());
        assert!(validate_limit(0).is_err());
        assert!(validate_limit(MAX_QUERY_LIMIT + 1).is_err());
        assert!(validate_limit(MAX_QUERY_LIMIT).is_ok());
    }
}
