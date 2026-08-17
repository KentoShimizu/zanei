use std::collections::BTreeSet;

use super::{Config, ConfigError, RedactorKind};

pub(super) fn validate(config: &Config) -> Result<(), ConfigError> {
    if config.output.batch_interval_s == 0 {
        return Err(ConfigError::ZeroBatchInterval);
    }
    if config.output.retention_hours == 0 {
        return Err(ConfigError::ZeroRetention);
    }
    unique(
        "capture.sources",
        config.capture.sources.iter().map(|source| source.as_str()),
    )?;
    nonempty_unique("filter.exclude_apps", &config.filter.exclude_apps)?;
    nonempty_unique("filter.include_only_apps", &config.filter.include_only_apps)?;
    domains("filter.exclude_websites", &config.filter.exclude_websites)?;
    domains(
        "filter.include_only_websites",
        &config.filter.include_only_websites,
    )?;
    unique_redactors(&config.filter.redactors)
}

fn nonempty_unique(field: &'static str, values: &[String]) -> Result<(), ConfigError> {
    for value in values {
        if value.trim() != value || value.is_empty() {
            return Err(ConfigError::InvalidListValue {
                field,
                value: value.clone(),
            });
        }
    }
    unique(field, values.iter().map(String::as_str))
}

fn domains(field: &'static str, values: &[String]) -> Result<(), ConfigError> {
    nonempty_unique(field, values)?;
    for value in values {
        let domain = value.strip_suffix('.').unwrap_or(value);
        if domain.is_empty()
            || domain.len() > 253
            || domain.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    || !label
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphanumeric)
                    || !label
                        .as_bytes()
                        .last()
                        .is_some_and(u8::is_ascii_alphanumeric)
            })
        {
            return Err(ConfigError::InvalidListValue {
                field,
                value: value.clone(),
            });
        }
    }
    Ok(())
}

fn unique<'a>(
    field: &'static str,
    values: impl IntoIterator<Item = &'a str>,
) -> Result<(), ConfigError> {
    let mut seen = BTreeSet::new();
    for value in values {
        let normalized = value.to_lowercase();
        if !seen.insert(normalized) {
            return Err(ConfigError::DuplicateValue {
                field,
                value: value.to_owned(),
            });
        }
    }
    Ok(())
}

fn unique_redactors(values: &[RedactorKind]) -> Result<(), ConfigError> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(*value) {
            return Err(ConfigError::DuplicateValue {
                field: "filter.redactors",
                value: format!("{value:?}").to_ascii_lowercase(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::Config;

    #[test]
    fn rejects_duplicate_sources_and_redactors() {
        assert!(Config::from_toml("[capture]\nsources = [\"app\", \"app\"]").is_err());
        assert!(Config::from_toml("[filter]\nredactors = [\"email\", \"email\"]").is_err());
    }

    #[test]
    fn rejects_empty_app_entries_and_non_domain_website_entries() {
        assert!(Config::from_toml("[filter]\nexclude_apps = [\"\"]").is_err());
        assert!(
            Config::from_toml("[filter]\nexclude_websites = [\"https://example.com\"]").is_err()
        );
    }
}
