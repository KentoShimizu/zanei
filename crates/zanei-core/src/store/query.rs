use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::event_row::{self, DecodedEventRow};
use super::selection::{EventSelection, validate_type_patterns};
use super::{StoreError, StoreReader};

const COLUMNS: &str = "id, ts, mono_ns, source, type, bundle_id, app_name, pid, \
                       window_title, window_id, element_json, data_json, redaction_json";
const MIN_QUERY_PAGE_ROWS: usize = 64;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueryFilter {
    pub since: Option<String>,
    pub until: Option<String>,
    pub types: Vec<String>,
    pub app: Option<String>,
    pub bundle_id: Option<String>,
    pub limit: Option<usize>,
}

impl QueryFilter {
    pub fn validate(&self) -> Result<(), StoreError> {
        validate_type_patterns(&self.types)
    }

    fn selection(&self) -> EventSelection {
        EventSelection {
            types: self.types.clone(),
            before: None,
            app: self.app.clone(),
            bundle_id: self.bundle_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryResult {
    pub events: Vec<crate::schema::Event>,
    pub skipped_unknown_types: u64,
}

pub(super) fn run(
    reader: &StoreReader,
    filter: &QueryFilter,
    retention_cutoff: &str,
) -> Result<QueryResult, StoreError> {
    filter.validate()?;
    let base = QueryConditions::new(filter, retention_cutoff)?;
    let sources = reader.event_sources();
    let mut result = QueryResult::default();
    let mut cursor = None;

    loop {
        let remaining = filter
            .limit
            .map(|limit| limit.saturating_sub(result.events.len()));
        if remaining == Some(0) {
            break;
        }
        let page_rows = remaining.map(|count| count.max(MIN_QUERY_PAGE_ROWS));
        let (sql, parameters) = build_page(&sources, &base, cursor.as_ref(), page_rows)?;
        let mut statement = reader.connection().prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(parameters.iter()))?;
        let mut page_count = 0_usize;

        while let Some(row) = rows.next()? {
            page_count += 1;
            cursor = Some(QueryCursor {
                ts: row.get(1)?,
                mono_ns: row.get(2)?,
                id: row.get(0)?,
                source_rank: row.get(13)?,
            });
            match event_row::decode(row)? {
                DecodedEventRow::Known(event) => result.events.push(event),
                DecodedEventRow::UnknownType => {
                    result.skipped_unknown_types = result
                        .skipped_unknown_types
                        .checked_add(1)
                        .ok_or(StoreError::NumericOverflow("skipped_unknown_types"))?;
                }
            }
            if filter
                .limit
                .is_some_and(|limit| result.events.len() >= limit)
            {
                break;
            }
        }

        match page_rows {
            None => break,
            Some(page_rows) if page_count < page_rows => break,
            Some(_) => {}
        }
    }
    Ok(result)
}

struct QueryConditions {
    sql: String,
    parameters: Vec<SqlValue>,
}

impl QueryConditions {
    fn new(filter: &QueryFilter, retention_cutoff: &str) -> Result<Self, StoreError> {
        let mut conditions = vec!["ts >= ?".to_owned()];
        let mut parameters = vec![SqlValue::Text(retention_cutoff.to_owned())];
        append_bound(
            &mut conditions,
            &mut parameters,
            "ts >= ?",
            "since",
            filter.since.as_deref(),
        )?;
        append_bound(
            &mut conditions,
            &mut parameters,
            "ts <= ?",
            "until",
            filter.until.as_deref(),
        )?;
        filter
            .selection()
            .append_predicate(&mut conditions, &mut parameters)?;
        Ok(Self {
            sql: conditions.join(" AND "),
            parameters,
        })
    }
}

struct QueryCursor {
    ts: String,
    mono_ns: i64,
    id: String,
    source_rank: i64,
}

fn build_page(
    sources: &[String],
    base: &QueryConditions,
    cursor: Option<&QueryCursor>,
    limit: Option<usize>,
) -> Result<(String, Vec<SqlValue>), StoreError> {
    let union = sources
        .iter()
        .enumerate()
        .map(|(rank, source)| {
            format!("SELECT {COLUMNS}, {rank} AS source_rank FROM {source}.events")
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let mut conditions = vec![base.sql.clone()];
    let mut parameters = base.parameters.clone();
    if let Some(cursor) = cursor {
        conditions.push(
            "(ts > ? OR (ts = ? AND mono_ns > ?) OR \
             (ts = ? AND mono_ns = ? AND id > ?) OR \
             (ts = ? AND mono_ns = ? AND id = ? AND source_rank > ?))"
                .to_owned(),
        );
        parameters.extend([
            SqlValue::Text(cursor.ts.clone()),
            SqlValue::Text(cursor.ts.clone()),
            SqlValue::Integer(cursor.mono_ns),
            SqlValue::Text(cursor.ts.clone()),
            SqlValue::Integer(cursor.mono_ns),
            SqlValue::Text(cursor.id.clone()),
            SqlValue::Text(cursor.ts.clone()),
            SqlValue::Integer(cursor.mono_ns),
            SqlValue::Text(cursor.id.clone()),
            SqlValue::Integer(cursor.source_rank),
        ]);
    }
    let mut sql = format!(
        "SELECT {COLUMNS}, source_rank FROM ({union}) WHERE {} \
         ORDER BY ts ASC, mono_ns ASC, id ASC, source_rank ASC",
        conditions.join(" AND ")
    );
    if let Some(limit) = limit {
        let limit = i64::try_from(limit).map_err(|_| StoreError::NumericOverflow("limit"))?;
        sql.push_str(" LIMIT ?");
        parameters.push(SqlValue::Integer(limit));
    }
    Ok((sql, parameters))
}

fn append_bound(
    conditions: &mut Vec<String>,
    parameters: &mut Vec<SqlValue>,
    condition: &str,
    field: &'static str,
    value: Option<&str>,
) -> Result<(), StoreError> {
    if let Some(value) = normalized_bound(field, value)? {
        conditions.push(condition.to_owned());
        parameters.push(SqlValue::Text(value));
    }
    Ok(())
}

/// Validates an optional RFC3339 bound and normalizes it to the store's timestamp form.
pub(super) fn normalized_bound(
    field: &'static str,
    value: Option<&str>,
) -> Result<Option<String>, StoreError> {
    value.map(|value| normalize_bound(field, value)).transpose()
}

pub(super) fn normalize_bound(field: &'static str, value: &str) -> Result<String, StoreError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| StoreError::invalid_timestamp(field, value.to_owned()))
        .map(crate::normalize::format_timestamp)
}
