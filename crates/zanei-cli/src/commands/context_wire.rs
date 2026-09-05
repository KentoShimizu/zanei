//! Versioned wire shapes. Core cursors remain the single owner of scan identity.
use serde::{Deserialize, Serialize};
use zanei_core::schema::EventData;
use zanei_core::store::{
    ContextCursor, ContextGap, ContextGapReason, ContextObservation, ContextPage, ContextRange,
    ContextText, EvidenceContent, EvidenceOrigin, SelectedEvidence,
};

pub const PROTOCOL_VERSION: i64 = 1;
pub const MAX_INPUT_BYTES: usize = 8 * 1024;
pub const MAX_PAGE_BYTES: usize = 512 * 1024;
pub const MAX_EVIDENCE_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Page {
        protocol_version: i64,
        #[serde(deserialize_with = "Option::deserialize")]
        cursor: Option<String>,
        #[serde(deserialize_with = "Option::deserialize")]
        upper_bound: Option<String>,
        limit: usize,
    },
    Evidence {
        protocol_version: i64,
        origin: EvidenceOrigin,
        start: u64,
        #[serde(deserialize_with = "Option::deserialize")]
        end: Option<u64>,
    },
}
impl Request {
    pub fn version(&self) -> i64 {
        match self {
            Self::Page {
                protocol_version, ..
            }
            | Self::Evidence {
                protocol_version, ..
            } => *protocol_version,
        }
    }
}

#[derive(Serialize)]
pub struct Response<'a> {
    protocol_version: i64,
    #[serde(flatten)]
    result: Result<'a>,
}
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Result<'a> {
    Page {
        store_identity: &'a str,
        observations: Vec<Observation<'a>>,
        next_cursor: String,
        upper_bound: String,
        has_more: bool,
        coverage: Range,
    },
    Gap {
        store_identity: &'a str,
        reason: GapReason,
        affected_range: Range,
        resume_cursor: String,
        upper_bound: String,
    },
    Evidence {
        origin: &'a EvidenceOrigin,
        content: Content<'a>,
        metadata: Metadata<'a>,
    },
    Expired,
    Denied,
    InvalidRequest,
    Unavailable {
        reason: UnavailableReason,
    },
    Incompatible {
        reason: IncompatibleReason,
        version: Option<i64>,
    },
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    Config,
    Key,
    Store,
    Input,
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompatibleReason {
    Protocol,
    StoreSchema,
    StoreCorrupt,
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    RetentionOrDeletion,
    StoreChanged,
    ContinuityUnknown,
}
#[derive(Serialize)]
pub struct Range {
    after: u64,
    through: u64,
}
impl From<&ContextRange> for Range {
    fn from(value: &ContextRange) -> Self {
        Self {
            after: value.after,
            through: value.through,
        }
    }
}
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Text<'a> {
    Absent,
    Value { text: &'a str },
    Omitted { utf8_bytes: u64 },
}
impl<'a> From<&'a ContextText> for Text<'a> {
    fn from(value: &'a ContextText) -> Self {
        match value {
            ContextText::Absent => Self::Absent,
            ContextText::Value(text) => Self::Value { text },
            ContextText::Omitted { utf8_bytes } => Self::Omitted {
                utf8_bytes: *utf8_bytes,
            },
        }
    }
}
#[derive(Serialize)]
pub struct Observation<'a> {
    append_sequence: u64,
    id: &'a str,
    ts: &'a str,
    source: Text<'a>,
    event_type: Text<'a>,
    bundle_id: Text<'a>,
    app_name: Text<'a>,
    pid: Option<i64>,
    window_title: Text<'a>,
    window_id: Option<i64>,
}
impl<'a> From<&'a ContextObservation> for Observation<'a> {
    fn from(o: &'a ContextObservation) -> Self {
        Self {
            append_sequence: o.append_sequence,
            id: &o.id,
            ts: &o.ts,
            source: (&o.source).into(),
            event_type: (&o.event_type).into(),
            bundle_id: (&o.bundle_id).into(),
            app_name: (&o.app_name).into(),
            pid: o.pid,
            window_title: (&o.window_title).into(),
            window_id: o.window_id,
        }
    }
}
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Content<'a> {
    Absent,
    Text {
        text: &'a str,
        start: u64,
        end: u64,
        total_bytes: u64,
        remaining: Option<(u64, u64)>,
    },
}
#[derive(Serialize)]
pub struct Metadata<'a> {
    event_type: &'static str,
    // Null text fields mean retrieval was omitted; only Content::Absent means absence.
    payload_without_text: &'a EventData,
    redaction_applied: bool,
    truncated: bool,
}

pub fn cursor(value: &str) -> std::result::Result<ContextCursor, ()> {
    serde_json::from_str(value.strip_prefix("v1:").ok_or(())?).map_err(|_| ())
}
fn encode_cursor(value: &ContextCursor) -> std::result::Result<String, serde_json::Error> {
    serde_json::to_string(value).map(|json| format!("v1:{json}"))
}
pub fn encode(result: Result<'_>) -> std::result::Result<Vec<u8>, serde_json::Error> {
    let mut bytes = serde_json::to_vec(&Response {
        protocol_version: PROTOCOL_VERSION,
        result,
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}
pub fn page(value: &ContextPage) -> std::result::Result<Vec<u8>, serde_json::Error> {
    encode(Result::Page {
        store_identity: value.next_cursor.store_identity(),
        observations: value.observations.iter().map(Observation::from).collect(),
        next_cursor: encode_cursor(&value.next_cursor)?,
        upper_bound: encode_cursor(&value.upper_bound)?,
        has_more: value.has_more,
        coverage: (&value.coverage).into(),
    })
}
pub fn gap(value: &ContextGap) -> std::result::Result<Vec<u8>, serde_json::Error> {
    encode(Result::Gap {
        store_identity: value.resume_cursor.store_identity(),
        reason: match value.reason {
            ContextGapReason::RetentionOrDeletion => GapReason::RetentionOrDeletion,
            ContextGapReason::StoreChanged => GapReason::StoreChanged,
            ContextGapReason::ContinuityUnknown => GapReason::ContinuityUnknown,
        },
        affected_range: (&value.affected_range).into(),
        resume_cursor: encode_cursor(&value.resume_cursor)?,
        upper_bound: encode_cursor(&value.upper_bound)?,
    })
}
pub fn evidence(
    value: &SelectedEvidence,
    prefix: usize,
) -> std::result::Result<Vec<u8>, serde_json::Error> {
    let content = match &value.content {
        EvidenceContent::Absent => Content::Absent,
        EvidenceContent::Text {
            text,
            start,
            end,
            total_bytes,
            remaining,
        } => {
            let requested_end = remaining.map_or(*end, |(_, end)| end);
            let end = start + prefix as u64;
            Content::Text {
                text: &text[..prefix],
                start: *start,
                end,
                total_bytes: *total_bytes,
                remaining: (end < requested_end).then_some((end, requested_end)),
            }
        }
    };
    encode(Result::Evidence {
        origin: &value.origin,
        content,
        metadata: Metadata {
            event_type: value.details.payload.event_type(),
            payload_without_text: &value.details.payload,
            redaction_applied: value.details.redaction_applied,
            truncated: value.details.truncated,
        },
    })
}
