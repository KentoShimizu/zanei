use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::handler::server::tool::schema_for_output;
use rmcp::model::JsonObject;
use rmcp::{schemars, schemars::JsonSchema};
use serde::Serialize;
use serde_json::Value;
use zanei_core::schema::Event;
use zanei_core::timeline::{Interaction, Session, TimeRange};

const EVENT_SCHEMA: &str = include_str!("../../../../docs/public/schema/event.schema.json");

#[derive(Debug, Serialize, JsonSchema)]
#[serde(tag = "format", rename_all = "lowercase", deny_unknown_fields)]
#[schemars(transform = require_object_root)]
pub(super) enum GetTimelineOutput {
    Markdown {
        range: RangeOutput,
        content: String,
        token_estimate: usize,
        truncated: bool,
    },
    Structured {
        range: RangeOutput,
        sessions: Vec<SessionOutput>,
        token_estimate: usize,
        truncated: bool,
        skipped_unknown_types: u64,
    },
}

impl GetTimelineOutput {
    pub(super) fn markdown(
        range: TimeRange,
        content: String,
        token_estimate: usize,
        truncated: bool,
    ) -> Self {
        Self::Markdown {
            range: range.into(),
            content,
            token_estimate,
            truncated,
        }
    }

    pub(super) fn structured(
        range: TimeRange,
        sessions: Vec<Session>,
        token_estimate: usize,
        truncated: bool,
        skipped_unknown_types: u64,
    ) -> Self {
        Self::Structured {
            range: range.into(),
            sessions: sessions.into_iter().map(SessionOutput::from).collect(),
            token_estimate,
            truncated,
            skipped_unknown_types,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct QueryEventsOutput {
    pub(super) range: RangeOutput,
    pub(super) count: usize,
    pub(super) truncated: bool,
    pub(super) skipped_unknown_types: u64,
    #[schemars(with = "Vec<serde_json::Value>")]
    pub(super) events: Vec<Event>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct GetStatusOutput {
    pub(super) running: bool,
    pub(super) paused: bool,
    pub(super) last_event_ts: Option<String>,
    pub(super) events_dropped: u64,
    pub(super) degraded: BTreeMap<String, String>,
    pub(super) collector_failures: BTreeMap<String, u64>,
    pub(super) retention_hours: u64,
    pub(super) oldest_event_ts: Option<String>,
    pub(super) capture: CaptureOutput,
    pub(super) permissions_ok: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct CaptureOutput {
    pub(super) sources: Vec<String>,
    pub(super) text_content: bool,
    pub(super) content_snapshot: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct RangeOutput {
    pub(super) since: String,
    pub(super) until: String,
}

impl From<TimeRange> for RangeOutput {
    fn from(range: TimeRange) -> Self {
        Self {
            since: range.since,
            until: range.until,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SessionOutput {
    start: String,
    end: String,
    app: String,
    title_summary: Option<String>,
    activities: Vec<String>,
    content_snapshots: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_ids: Option<Vec<String>>,
    event_ids_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    interactions: Option<Vec<InteractionOutput>>,
}

impl From<Session> for SessionOutput {
    fn from(session: Session) -> Self {
        Self {
            start: session.start,
            end: session.end,
            app: session.app,
            title_summary: session.title_summary,
            activities: session.activities,
            content_snapshots: session.content_snapshots,
            event_ids: session.event_ids,
            event_ids_truncated: session.event_ids_truncated,
            interactions: session
                .interactions
                .map(|items| items.into_iter().map(InteractionOutput::from).collect()),
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
struct InteractionOutput {
    ts: String,
    activity: String,
}

impl From<Interaction> for InteractionOutput {
    fn from(interaction: Interaction) -> Self {
        Self {
            ts: interaction.ts,
            activity: interaction.activity,
        }
    }
}

pub(super) fn query_events_output_schema() -> Arc<JsonObject> {
    let generated = schema_for_output::<QueryEventsOutput>()
        .expect("QueryEventsOutput schema root must remain an object");
    let mut output_schema = Value::Object((*generated).clone());
    let event_schema: Value =
        serde_json::from_str(EVENT_SCHEMA).expect("canonical event schema must be valid JSON");

    merge_event_definitions(&mut output_schema, &event_schema);
    let items = output_schema
        .pointer_mut("/properties/events/items")
        .expect("QueryEventsOutput schema must expose events.items");
    *items = event_schema;

    match output_schema {
        Value::Object(schema) => Arc::new(schema),
        _ => unreachable!("QueryEventsOutput schema root was generated as an object"),
    }
}

fn merge_event_definitions(output_schema: &mut Value, event_schema: &Value) {
    let event_definitions = event_schema
        .get("$defs")
        .and_then(Value::as_object)
        .expect("canonical event schema must define $defs");
    let output = output_schema
        .as_object_mut()
        .expect("QueryEventsOutput schema root must be an object");
    let output_definitions = output
        .entry("$defs")
        .or_insert_with(|| Value::Object(JsonObject::new()))
        .as_object_mut()
        .expect("QueryEventsOutput $defs must be an object");

    for (name, definition) in event_definitions {
        match output_definitions.get(name) {
            Some(existing) if existing != definition => {
                panic!("canonical event schema definition {name:?} collides with MCP output schema")
            }
            Some(_) => {}
            None => {
                output_definitions.insert(name.clone(), definition.clone());
            }
        }
    }
}

fn require_object_root(schema: &mut schemars::Schema) {
    schema.insert("type".to_owned(), "object".into());
}
