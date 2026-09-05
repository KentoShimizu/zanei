//! Append-order references from the active store. No raw event body is selected.
//!
//! The caller durably publishes each Page/Gap before adopting its cursor. Source
//! authorization/epochs and the wire byte/time budget belong to the CLI/backend.
//! External byte-for-byte restores cannot all be detected: identity, high-water
//! regression and a surviving cursor anchor detect observable discontinuities.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{STORE_SCHEMA_VERSION, StoreError, StoreReader};

pub const MAX_CONTEXT_PAGE_ROWS: usize = 256;
pub const MAX_CONTEXT_TEXT_BYTES: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum ContextPageError {
    #[error("invalid context page request: {0}")]
    InvalidRequest(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<rusqlite::Error> for ContextPageError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}

/// Opaque to consumers; serialization preserves the anchor across worker restarts.
/// A position may also serve as a fixed upper bound for a replayable scan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextCursor {
    store_identity: String,
    sequence: u64,
    observed_head: u64,
    anchor: Option<Anchor>,
}

impl ContextCursor {
    /// Identity for the caller's SourceBinding check; the position stays opaque.
    #[must_use]
    pub fn store_identity(&self) -> &str {
        &self.store_identity
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Anchor {
    id: String,
    // Canonical RFC3339 keeps the original offset while bounding fractional precision.
    ts: String,
}

#[derive(Clone, Debug, Default)]
pub struct ContextPageRequest {
    pub cursor: Option<ContextCursor>,
    pub upper_bound: Option<ContextCursor>,
    /// Must be in 1..=MAX_CONTEXT_PAGE_ROWS; no implicit default at this boundary.
    pub limit: usize,
}

/// Sequence interval `(after, through]` within the returned cursor's store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextRange {
    pub after: u64,
    pub through: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextGapReason {
    RetentionOrDeletion,
    StoreChanged,
    ContinuityUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextGap {
    pub reason: ContextGapReason,
    /// For StoreChanged/ContinuityUnknown this is the old requested interval.
    pub affected_range: ContextRange,
    pub resume_cursor: ContextCursor,
    pub upper_bound: ContextCursor,
}

/// Missing optional metadata is distinct from metadata omitted for size.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextText {
    Absent,
    Value(String),
    Omitted { utf8_bytes: u64 },
}

/// Original event identity and surface metadata; body/element/redaction are
/// references, never implicit understanding coverage. Unknown event types remain
/// visible here so a consumer cannot mistake unsupported content for no activity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextObservation {
    pub append_sequence: u64,
    pub id: String,
    pub ts: String,
    pub source: ContextText,
    pub event_type: ContextText,
    pub bundle_id: ContextText,
    pub app_name: ContextText,
    pub pid: Option<i64>,
    pub window_title: ContextText,
    pub window_id: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextPage {
    pub observations: Vec<ContextObservation>,
    pub next_cursor: ContextCursor,
    pub upper_bound: ContextCursor,
    pub has_more: bool,
    /// Read coverage only. Retrieve selected evidence to establish body coverage.
    pub coverage: ContextRange,
}

impl ContextPage {
    /// Adopt a nonempty prefix after the wire owner measures its encoded budget.
    /// The retained rows and all progress fields advance together.
    pub fn retain_prefix(&mut self, rows: usize) -> Result<(), ContextPageError> {
        if rows == self.observations.len() {
            return Ok(());
        }
        if rows == 0 || rows > self.observations.len() {
            return Err(ContextPageError::InvalidRequest("invalid page prefix"));
        }
        self.observations.truncate(rows);
        let last = &self.observations[rows - 1];
        self.next_cursor.sequence = last.append_sequence;
        self.next_cursor.anchor = Some(Anchor {
            id: last.id.clone(),
            ts: last.ts.clone(),
        });
        self.coverage.through = last.append_sequence;
        self.has_more = last.append_sequence < self.upper_bound.sequence;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextPageResult {
    Page(ContextPage),
    Gap(ContextGap),
    Incompatible { version: i64 },
}

impl StoreReader {
    pub fn read_context_page(
        &self,
        request: &ContextPageRequest,
        configured_retention_hours: u64,
    ) -> Result<ContextPageResult, ContextPageError> {
        self.read_context_page_at(
            request,
            configured_retention_hours,
            OffsetDateTime::now_utc(),
        )
    }

    /// Holds a single SQLite read snapshot across head, retention and row checks.
    /// Retention is re-evaluated on every replay: expired evidence becomes a Gap.
    /// Set-aside stores have independent histories and are deliberately excluded.
    pub fn read_context_page_at(
        &self,
        request: &ContextPageRequest,
        configured_retention_hours: u64,
        now: OffsetDateTime,
    ) -> Result<ContextPageResult, ContextPageError> {
        validate_request(request)?;
        if self.schema_version != STORE_SCHEMA_VERSION {
            return Ok(ContextPageResult::Incompatible {
                version: self.schema_version,
            });
        }
        let transaction = self.connection.unchecked_transaction()?;
        // Reject a schema changed after this reader opened as well.
        let version = transaction.query_row("SELECT schema_version FROM meta", [], |r| r.get(0))?;
        if version != STORE_SCHEMA_VERSION {
            return Ok(ContextPageResult::Incompatible { version });
        }
        let head = self.append_head()?;
        let current = position(
            &transaction,
            &head.store_identity,
            head.sequence,
            head.sequence,
        )?;
        let start = request.cursor.clone().unwrap_or_else(|| ContextCursor {
            store_identity: head.store_identity.clone(),
            sequence: 0,
            observed_head: head.sequence,
            anchor: None,
        });
        let upper = request
            .upper_bound
            .clone()
            .unwrap_or_else(|| current.clone());
        let retention_hours = self.effective_retention_hours_at(now, configured_retention_hours)?;
        let cutoff = super::retention_boundary(now, retention_hours)?;
        for cursor in request.cursor.iter().chain(request.upper_bound.iter()) {
            let reason = if cursor.store_identity != head.store_identity {
                Some(ContextGapReason::StoreChanged)
            } else if cursor.observed_head > head.sequence
                || !anchor_matches(&transaction, cursor, cutoff)?
            {
                Some(ContextGapReason::ContinuityUnknown)
            } else {
                None
            };
            if let Some(reason) = reason {
                return Ok(ContextPageResult::Gap(ContextGap {
                    reason,
                    affected_range: ContextRange {
                        after: start.sequence,
                        through: request
                            .upper_bound
                            .as_ref()
                            .map_or(start.observed_head, |bound| bound.sequence),
                    },
                    resume_cursor: ContextCursor {
                        store_identity: head.store_identity,
                        sequence: 0,
                        observed_head: head.sequence,
                        anchor: None,
                    },
                    upper_bound: current,
                }));
            }
        }
        let result = read_range(&transaction, &start, &upper, request.limit, cutoff)?;
        transaction.commit()?;
        Ok(result)
    }
}

fn validate_request(request: &ContextPageRequest) -> Result<(), ContextPageError> {
    if !(1..=MAX_CONTEXT_PAGE_ROWS).contains(&request.limit) {
        return Err(ContextPageError::InvalidRequest(
            "limit is outside the page row budget",
        ));
    }
    for cursor in request.cursor.iter().chain(request.upper_bound.iter()) {
        if cursor.store_identity.len() != 32
            || !cursor.store_identity.bytes().all(|b| b.is_ascii_hexdigit())
            || cursor.sequence > cursor.observed_head
            || cursor.observed_head > i64::MAX as u64
            || (cursor.sequence == 0 && cursor.anchor.is_some())
        {
            return Err(ContextPageError::InvalidRequest("malformed cursor"));
        }
        if let Some(anchor) = &cursor.anchor {
            if anchor.id.len() != 30 || !anchor.id.starts_with("evt_") {
                return Err(ContextPageError::InvalidRequest("malformed cursor anchor"));
            }
            OffsetDateTime::parse(&anchor.ts, &Rfc3339).map_err(|_| {
                ContextPageError::InvalidRequest("malformed cursor anchor timestamp")
            })?;
        }
    }
    if let (Some(start), Some(upper)) = (&request.cursor, &request.upper_bound) {
        if start.store_identity != upper.store_identity || start.sequence > upper.sequence {
            return Err(ContextPageError::InvalidRequest(
                "cursor and upper bound do not match",
            ));
        }
    }
    Ok(())
}

fn position(
    connection: &Connection,
    identity: &str,
    sequence: u64,
    head: u64,
) -> Result<ContextCursor, StoreError> {
    let anchor = read_anchor(connection, sequence)?;
    Ok(ContextCursor {
        store_identity: identity.to_owned(),
        sequence,
        observed_head: head,
        anchor,
    })
}

fn read_anchor(connection: &Connection, sequence: u64) -> Result<Option<Anchor>, StoreError> {
    let raw = connection
        .query_row(
            "SELECT id, ts FROM events WHERE append_sequence=?1",
            [sequence],
            |row| {
                Ok(Anchor {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                })
            },
        )
        .optional()?;
    raw.map(|mut anchor| {
        anchor.ts = canonical_event_time(&anchor.ts)?;
        Ok(anchor)
    })
    .transpose()
}

fn anchor_matches(
    connection: &Connection,
    cursor: &ContextCursor,
    cutoff: OffsetDateTime,
) -> Result<bool, StoreError> {
    let Some(expected) = &cursor.anchor else {
        return Ok(true);
    };
    let actual = read_anchor(connection, cursor.sequence)?;
    // Expiry of a consumed anchor alone says nothing about the unread interval.
    // If the row survives, compare even when expired to catch observable rewrites.
    let expired = parse_event_time(&expected.ts)? < cutoff;
    Ok(actual.as_ref().map_or(expired, |actual| actual == expected))
}

fn read_range(
    connection: &Connection,
    start: &ContextCursor,
    upper: &ContextCursor,
    limit: usize,
    cutoff: OffsetDateTime,
) -> Result<ContextPageResult, ContextPageError> {
    // No JSON extraction: even a multi-megabyte raw body never enters this read.
    // Bound optional surface metadata in SQLite; required id/ts preserve the
    // writer contract. The CLI owns the total wire byte budget.
    let texts = ["source", "type", "bundle_id", "app_name", "window_title"];
    let columns = texts.iter().map(|column| format!(
        "CASE WHEN length(CAST({column} AS BLOB)) <= {MAX_CONTEXT_TEXT_BYTES} THEN {column} END, \
         length(CAST({column} AS BLOB))"
    )).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT append_sequence, id, ts, pid, window_id, {columns} FROM events \
         WHERE append_sequence > ?1 AND append_sequence <= ?2 \
         ORDER BY append_sequence LIMIT ?3"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(params![start.sequence, upper.sequence, limit as u64])?;
    let mut observations = Vec::new();
    let mut after = start.sequence;
    let mut expired = false;
    while let Some(row) = rows.next()? {
        let sequence: u64 = row.get(0)?;
        if sequence != after + 1 {
            if observations.is_empty() && !expired {
                return unavailable_range(connection, start, upper, sequence - 1);
            }
            break;
        }
        let ts: String = row.get(2)?;
        if parse_event_time(&ts)? < cutoff {
            if !observations.is_empty() {
                break;
            }
            expired = true;
        } else {
            if expired {
                break;
            }
            observations.push(ContextObservation {
                append_sequence: sequence,
                id: row.get(1)?,
                ts: canonical_event_time(&ts)?,
                pid: row.get(3)?,
                window_id: row.get(4)?,
                source: text(row, 5)?,
                event_type: text(row, 7)?,
                bundle_id: text(row, 9)?,
                app_name: text(row, 11)?,
                window_title: text(row, 13)?,
            });
        }
        after = sequence;
    }
    if expired {
        return unavailable_range(connection, start, upper, after);
    }
    if observations.is_empty() && after < upper.sequence {
        return unavailable_range(connection, start, upper, upper.sequence);
    }
    Ok(ContextPageResult::Page(ContextPage {
        observations,
        next_cursor: position(
            connection,
            &upper.store_identity,
            after,
            upper.observed_head,
        )?,
        upper_bound: upper.clone(),
        has_more: after < upper.sequence,
        coverage: ContextRange {
            after: start.sequence,
            through: after,
        },
    }))
}

fn unavailable_range(
    connection: &Connection,
    start: &ContextCursor,
    upper: &ContextCursor,
    through: u64,
) -> Result<ContextPageResult, ContextPageError> {
    // The schema has no deletion ledger; absence and expired rows share this
    // truthful category. A missing interval must never become an empty success.
    Ok(ContextPageResult::Gap(ContextGap {
        reason: ContextGapReason::RetentionOrDeletion,
        affected_range: ContextRange {
            after: start.sequence,
            through,
        },
        resume_cursor: position(
            connection,
            &upper.store_identity,
            through,
            upper.observed_head,
        )?,
        upper_bound: upper.clone(),
    }))
}

fn parse_event_time(value: &str) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| StoreError::invalid_timestamp("event.ts", value.to_owned()))
}

pub(super) fn canonical_event_time(value: &str) -> Result<String, StoreError> {
    parse_event_time(value)?
        .format(&Rfc3339)
        .map_err(|_| StoreError::invalid_timestamp("event.ts", value.to_owned()))
}

fn text(row: &rusqlite::Row<'_>, index: usize) -> Result<ContextText, rusqlite::Error> {
    match (
        row.get::<_, Option<String>>(index)?,
        row.get::<_, Option<u64>>(index + 1)?,
    ) {
        (Some(value), _) => Ok(ContextText::Value(value)),
        (None, Some(utf8_bytes)) => Ok(ContextText::Omitted { utf8_bytes }),
        (None, None) => Ok(ContextText::Absent),
    }
}
