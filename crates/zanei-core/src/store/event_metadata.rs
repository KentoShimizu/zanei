use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use time::OffsetDateTime;

use super::{EventSelection, StoreError, StoreReader, query, retention_cutoff};

const METADATA_COLUMNS: &str = "id, ts, bundle_id, app_name, window_id";
const SELECTION_COLUMNS: &str = "id, ts, bundle_id, app_name, window_id, type";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventMetadata {
    pub id: String,
    pub ts: String,
    pub bundle_id: Option<String>,
    pub app_name: String,
    pub window_id: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataFilter {
    pub since: Option<String>,
    pub until: Option<String>,
    pub types: Vec<String>,
    pub app: Option<String>,
    pub bundle_id: Option<String>,
    pub configured_retention_hours: u64,
}

pub(super) fn run(
    reader: &StoreReader,
    filter: &MetadataFilter,
) -> Result<Vec<EventMetadata>, StoreError> {
    let now = OffsetDateTime::now_utc();
    let retention_hours =
        reader.effective_retention_hours_at(now, filter.configured_retention_hours)?;
    let mut conditions = vec!["ts >= ?".to_owned()];
    let mut parameters = vec![SqlValue::Text(retention_cutoff(now, retention_hours)?)];
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
    EventSelection {
        types: filter.types.clone(),
        before: None,
        app: filter.app.clone(),
        bundle_id: filter.bundle_id.clone(),
    }
    .append_predicate(&mut conditions, &mut parameters)?;

    let union = reader
        .event_sources()
        .iter()
        .enumerate()
        .map(|(rank, source)| {
            format!("SELECT {SELECTION_COLUMNS}, {rank} AS source_rank FROM {source}.events")
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let sql = format!(
        "SELECT {METADATA_COLUMNS} FROM ({union}) WHERE {} \
         ORDER BY ts ASC, id ASC, source_rank ASC",
        conditions.join(" AND ")
    );
    let mut statement = reader.connection().prepare(&sql)?;
    statement
        .query_map(params_from_iter(parameters.iter()), |row| {
            Ok(EventMetadata {
                id: row.get(0)?,
                ts: row.get(1)?,
                bundle_id: row.get(2)?,
                app_name: row.get(3)?,
                window_id: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn append_bound(
    conditions: &mut Vec<String>,
    parameters: &mut Vec<SqlValue>,
    condition: &str,
    field: &'static str,
    value: Option<&str>,
) -> Result<(), StoreError> {
    if let Some(value) = query::normalized_bound(field, value)? {
        conditions.push(condition.to_owned());
        parameters.push(SqlValue::Text(value));
    }
    Ok(())
}
