use rusqlite::types::Value as SqlValue;

use super::StoreError;

/// Event fields shared by reads, snapshots, and destructive selections.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EventSelection {
    pub types: Vec<String>,
    pub before: Option<String>,
    pub app: Option<String>,
    pub bundle_id: Option<String>,
}

impl EventSelection {
    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        validate_type_patterns(&self.types)
    }

    /// Appends the selection as SQL conditions.
    ///
    /// An empty `types` list selects every type except the `content.*` family, so a caller
    /// that wants everything (export, purge --all, retention) passes `["*"]` explicitly.
    pub(crate) fn append_predicate(
        &self,
        conditions: &mut Vec<String>,
        parameters: &mut Vec<SqlValue>,
    ) -> Result<(), StoreError> {
        self.validate()?;
        if let Some(before) = self.before.as_deref() {
            let before = super::query::normalize_bound("before", before)?;
            conditions.push("ts < ?".to_owned());
            parameters.push(SqlValue::Text(before));
        }
        append_type_predicate(conditions, parameters, &self.types);
        append_optional_text(conditions, parameters, "app_name = ?", self.app.as_deref());
        append_optional_text(
            conditions,
            parameters,
            "bundle_id = ?",
            self.bundle_id.as_deref(),
        );
        Ok(())
    }
}

pub(crate) fn validate_type_patterns(patterns: &[String]) -> Result<(), StoreError> {
    for pattern in patterns {
        let wildcard_count = pattern.bytes().filter(|byte| *byte == b'*').count();
        if wildcard_count > 1 || (wildcard_count == 1 && !pattern.ends_with('*')) {
            return Err(StoreError::InvalidTypePattern(pattern.clone()));
        }
    }
    Ok(())
}

fn append_type_predicate(
    conditions: &mut Vec<String>,
    parameters: &mut Vec<SqlValue>,
    patterns: &[String],
) {
    if patterns.is_empty() {
        conditions.push("type NOT LIKE 'content.%' ESCAPE '\\'".to_owned());
        return;
    }

    let mut type_conditions = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        if let Some(prefix) = pattern.strip_suffix('*') {
            type_conditions.push("type LIKE ? ESCAPE '\\'".to_owned());
            parameters.push(SqlValue::Text(format!("{}%", escape_like(prefix))));
        } else {
            type_conditions.push("type = ?".to_owned());
            parameters.push(SqlValue::Text(pattern.clone()));
        }
    }
    conditions.push(format!("({})", type_conditions.join(" OR ")));
}

fn append_optional_text(
    conditions: &mut Vec<String>,
    parameters: &mut Vec<SqlValue>,
    condition: &str,
    value: Option<&str>,
) {
    if let Some(value) = value {
        conditions.push(condition.to_owned());
        parameters.push(SqlValue::Text(value.to_owned()));
    }
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
