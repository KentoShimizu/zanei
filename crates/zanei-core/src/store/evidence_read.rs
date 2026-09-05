//! Selected decoded fields from one current-store event, never an entire raw body.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::context_page::canonical_event_time;
use super::{ContextPageError, STORE_SCHEMA_VERSION, StoreError, StoreReader};
use crate::schema::{Event, EventData};

pub const MAX_EVIDENCE_BYTES: usize = 64 * 1024;
// All variable text is removed before decoding the existing closed EventData type.
const MAX_DETAIL_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceField {
    Source,
    EventType,
    BundleId,
    AppName,
    WindowTitle,
    ElementRole,
    ElementTitle,
    ElementValue,
    Text,
    Url,
    TabTitle,
    PreviousTitle,
    KeyCombo,
}

impl EvidenceField {
    fn sql(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::EventType => "type",
            Self::BundleId => "bundle_id",
            Self::AppName => "app_name",
            Self::WindowTitle => "window_title",
            Self::ElementRole => "json_extract(element_json, '$.role')",
            Self::ElementTitle => "json_extract(element_json, '$.title')",
            Self::ElementValue => "json_extract(element_json, '$.value')",
            Self::Text => "json_extract(data_json, '$.text')",
            Self::Url => "json_extract(data_json, '$.url')",
            Self::TabTitle => "json_extract(data_json, '$.tab_title')",
            Self::PreviousTitle => "json_extract(data_json, '$.prev_title')",
            Self::KeyCombo => "json_extract(data_json, '$.combo')",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceOrigin {
    pub store_identity: String,
    pub append_sequence: u64,
    pub event_id: String,
    pub observed_at: String,
    pub field: EvidenceField,
}

/// UTF-8 bytes of the decoded field, 0-based [start, end). No JSON byte offsets.
#[derive(Clone, Debug)]
pub struct EvidenceRequest {
    pub origin: EvidenceOrigin,
    pub start: u64,
    pub end: Option<u64>,
    pub max_bytes: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EvidenceDetails {
    /// Existing payload schema with variable text nulled. Fetch original text
    /// fields separately; these nulls do not assert that the source had no text.
    pub payload: EventData,
    pub redaction_applied: bool,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvidenceContent {
    Absent,
    Text {
        text: String,
        start: u64,
        end: u64,
        total_bytes: u64,
        remaining: Option<(u64, u64)>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelectedEvidence {
    pub origin: EvidenceOrigin,
    pub content: EvidenceContent,
    pub details: EvidenceDetails,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EvidenceResult {
    Evidence(Box<SelectedEvidence>),
    Expired,
    Denied,
    Incompatible { version: i64 },
}

impl StoreReader {
    pub fn read_evidence(
        &self,
        request: &EvidenceRequest,
        retention_hours: u64,
    ) -> Result<EvidenceResult, ContextPageError> {
        self.read_evidence_at(request, retention_hours, OffsetDateTime::now_utc())
    }

    pub fn read_evidence_at(
        &self,
        request: &EvidenceRequest,
        retention_hours: u64,
        now: OffsetDateTime,
    ) -> Result<EvidenceResult, ContextPageError> {
        if request
            .end
            .is_some_and(|end| request.start > end || end > i64::MAX as u64)
            || request.start > i64::MAX as u64
            || !(4..=MAX_EVIDENCE_BYTES).contains(&request.max_bytes)
            || request.origin.append_sequence == 0
            || request.origin.append_sequence > i64::MAX as u64
        {
            return Err(ContextPageError::InvalidRequest(
                "invalid evidence range or budget",
            ));
        }
        let expected_time = OffsetDateTime::parse(&request.origin.observed_at, &Rfc3339)
            .map_err(|_| ContextPageError::InvalidRequest("invalid evidence timestamp"))?;
        if self.schema_version != STORE_SCHEMA_VERSION {
            return Ok(EvidenceResult::Incompatible {
                version: self.schema_version,
            });
        }
        let transaction = self.connection.unchecked_transaction()?;
        let version = transaction.query_row("SELECT schema_version FROM meta", [], |r| r.get(0))?;
        if version != STORE_SCHEMA_VERSION {
            return Ok(EvidenceResult::Incompatible { version });
        }
        let head = self.append_head()?;
        if head.store_identity != request.origin.store_identity {
            return Ok(EvidenceResult::Denied);
        }
        let row: Option<(String, String)> = transaction
            .query_row(
                "SELECT id,ts FROM events WHERE append_sequence=?1",
                [request.origin.append_sequence],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, ts)) = row else {
            return Ok(EvidenceResult::Expired);
        };
        let actual_time = OffsetDateTime::parse(&ts, &Rfc3339)
            .map_err(|_| StoreError::invalid_timestamp("event.ts", ts.clone()))?;
        if id != request.origin.event_id || actual_time != expected_time {
            return Ok(EvidenceResult::Denied);
        }
        let retention = self.effective_retention_hours_at(now, retention_hours)?;
        if actual_time < super::retention_boundary(now, retention)? {
            return Ok(EvidenceResult::Expired);
        }
        let expression = request.origin.field.sql();
        // BLOB length/substr operate after JSON decoding and preserve embedded NUL.
        let sql = format!(
            "SELECT typeof({expression}), length(CAST({expression} AS BLOB)), \
            substr(CAST({expression} AS BLOB), ?2 + 1, ?3) FROM events WHERE append_sequence=?1"
        );
        let requested_bytes = request
            .end
            .map_or(request.max_bytes as u64, |end| end - request.start);
        let bytes_to_read = request.max_bytes.min(requested_bytes as usize);
        let (kind, total, bytes): (String, Option<u64>, Option<Vec<u8>>) = transaction.query_row(
            &sql,
            params![
                request.origin.append_sequence,
                request.start,
                bytes_to_read as u64
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let content = if kind == "null" {
            EvidenceContent::Absent
        } else {
            if kind != "text" {
                return Err(corrupt("selected evidence field is not text"));
            }
            let total = total.ok_or_else(|| corrupt("missing evidence length"))?;
            let requested_end = request.end.unwrap_or(total);
            if requested_end > total || request.start > total {
                return Err(ContextPageError::InvalidRequest(
                    "evidence range exceeds field",
                ));
            }
            // SQLite substr of an empty BLOB returns NULL. The original type/length
            // above distinguish this from an absent field.
            let mut bytes = if requested_end == request.start {
                Vec::new()
            } else {
                bytes.ok_or_else(|| corrupt("missing evidence bytes"))?
            };
            match std::str::from_utf8(&bytes) {
                Ok(_) => {}
                Err(error)
                    if error.error_len().is_none()
                        && (bytes.len() as u64) < requested_end - request.start =>
                {
                    bytes.truncate(error.valid_up_to());
                }
                Err(_) => {
                    return Err(ContextPageError::InvalidRequest(
                        "evidence range splits UTF-8",
                    ));
                }
            }
            let text = String::from_utf8(bytes).map_err(|_| corrupt("invalid evidence UTF-8"))?;
            let end = request.start + text.len() as u64;
            EvidenceContent::Text {
                text,
                start: request.start,
                end,
                total_bytes: total,
                remaining: (end < requested_end).then_some((end, requested_end)),
            }
        };
        let details = read_details(&transaction, request.origin.append_sequence)?;
        transaction.commit()?;
        let mut origin = request.origin.clone();
        origin.observed_at = canonical_event_time(&ts)?;
        Ok(EvidenceResult::Evidence(Box::new(SelectedEvidence {
            origin,
            content,
            details,
        })))
    }
}

fn read_details(
    connection: &rusqlite::Connection,
    sequence: u64,
) -> Result<EvidenceDetails, ContextPageError> {
    let stripped = "json_replace(data_json, '$.text', NULL, '$.url', NULL, '$.tab_title', NULL, '$.prev_title', NULL, '$.combo', NULL)";
    let sql = format!(
        "SELECT type, CASE WHEN length(CAST({stripped} AS BLOB)) <= {MAX_DETAIL_BYTES} THEN {stripped} END, \
        json_extract(redaction_json, '$.applied'), \
        EXISTS(SELECT 1 FROM json_each(redaction_json, '$.rules') WHERE value=?2) \
        FROM events WHERE append_sequence=?1"
    );
    let (event_type, data, redaction, truncated): (String, Option<String>, bool, bool) = connection
        .query_row(&sql, params![sequence, Event::SIZE_LIMIT_RULE], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
    let data = data.ok_or_else(|| corrupt("event details exceed schema budget"))?;
    let value =
        serde_json::from_str(&data).map_err(|e| StoreError::invalid_json("event.data", e))?;
    let payload = EventData::from_type_and_value(&event_type, value)
        .map_err(|e| StoreError::invalid_json("event.data", e))?;
    Ok(EvidenceDetails {
        payload,
        redaction_applied: redaction,
        truncated,
    })
}

fn corrupt(message: &'static str) -> ContextPageError {
    StoreError::Database(rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    ))
    .into()
}
