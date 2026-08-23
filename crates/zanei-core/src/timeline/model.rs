use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Granularity {
    Coarse,
    Fine,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TimelineFormat {
    #[serde(rename = "md")]
    Markdown,
    Json,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TimeRange {
    pub since: String,
    pub until: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineOptions {
    pub range: TimeRange,
    pub token_budget: usize,
    pub granularity: Granularity,
    pub format: TimelineFormat,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Timeline {
    pub range: TimeRange,
    pub token_estimate: usize,
    pub truncated: bool,
    pub sessions: Vec<Session>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Session {
    pub start: String,
    pub end: String,
    pub app: String,
    pub title_summary: Option<String>,
    pub activities: Vec<String>,
    pub content_snapshots: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_ids: Option<Vec<String>>,
    #[serde(default)]
    pub event_ids_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactions: Option<Vec<Interaction>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Interaction {
    pub ts: String,
    pub activity: String,
}
