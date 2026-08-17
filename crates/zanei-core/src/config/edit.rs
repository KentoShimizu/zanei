use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use super::{Config, ConfigError};

const PUBLIC_SUFFIX_SNAPSHOT: &str = include_str!("public_suffix_snapshot_v1.txt");
static NEXT_TEMPORARY_ID: AtomicU64 = AtomicU64::new(0);

const ARRAY_EDIT_GUIDANCE: &str =
    "arrays are managed with dedicated commands (filter) or config edit";

#[derive(Debug, Error)]
pub enum ConfigSetError {
    #[error("unknown configuration key: {0}")]
    UnknownKey(String),
    #[error("configuration key {0} is an array; {ARRAY_EDIT_GUIDANCE}")]
    ArrayKey(String),
    #[error("invalid value for {key}: {value}; expected {expected}")]
    InvalidValue {
        key: &'static str,
        value: String,
        expected: &'static str,
    },
    #[error("invalid value for {key}: {source}")]
    Validation {
        key: &'static str,
        #[source]
        source: Box<ConfigError>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScalarEditResult {
    pub config: Config,
    pub changed: bool,
    pub restart_recording: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScalarConfigKey {
    CaptureTextContent,
    OutputBatchIntervalSeconds,
    OutputRetentionHours,
}

impl ScalarConfigKey {
    fn parse(dotted_key: &str) -> Result<Self, ConfigSetError> {
        match dotted_key {
            "capture.text_content" => Ok(Self::CaptureTextContent),
            "output.batch_interval_s" => Ok(Self::OutputBatchIntervalSeconds),
            "output.retention_hours" => Ok(Self::OutputRetentionHours),
            "capture.sources"
            | "filter.exclude_apps"
            | "filter.include_only_apps"
            | "filter.exclude_websites"
            | "filter.include_only_websites"
            | "filter.redactors" => Err(ConfigSetError::ArrayKey(dotted_key.to_owned())),
            _ => Err(ConfigSetError::UnknownKey(dotted_key.to_owned())),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::CaptureTextContent => "capture.text_content",
            Self::OutputBatchIntervalSeconds => "output.batch_interval_s",
            Self::OutputRetentionHours => "output.retention_hours",
        }
    }

    const fn restart_recording(self) -> bool {
        matches!(
            self,
            Self::CaptureTextContent | Self::OutputBatchIntervalSeconds
        )
    }
}

pub fn apply_scalar_edit(
    config: &Config,
    dotted_key: &str,
    value: &str,
) -> Result<ScalarEditResult, ConfigSetError> {
    let key = ScalarConfigKey::parse(dotted_key)?;
    let mut edited = config.clone();

    match key {
        ScalarConfigKey::CaptureTextContent => {
            edited.capture.text_content = parse_bool(key, value)?;
        }
        ScalarConfigKey::OutputBatchIntervalSeconds => {
            edited.output.batch_interval_s = parse_u64(key, value)?;
        }
        ScalarConfigKey::OutputRetentionHours => {
            edited.output.retention_hours = parse_u64(key, value)?;
        }
    }

    edited
        .validate()
        .map_err(|source| ConfigSetError::Validation {
            key: key.name(),
            source: Box::new(source),
        })?;

    Ok(ScalarEditResult {
        changed: edited != *config,
        config: edited,
        restart_recording: key.restart_recording(),
    })
}

fn parse_bool(key: ScalarConfigKey, value: &str) -> Result<bool, ConfigSetError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(invalid_value(key, value, "true or false")),
    }
}

fn parse_u64(key: ScalarConfigKey, value: &str) -> Result<u64, ConfigSetError> {
    value
        .parse()
        .map_err(|_| invalid_value(key, value, "an unsigned integer"))
}

fn invalid_value(key: ScalarConfigKey, value: &str, expected: &'static str) -> ConfigSetError {
    ConfigSetError::InvalidValue {
        key: key.name(),
        value: value.to_owned(),
        expected,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterList {
    ExcludeApps,
    IncludeOnlyApps,
    ExcludeWebsites,
    IncludeOnlyWebsites,
}

impl FilterList {
    const fn is_website(self) -> bool {
        matches!(self, Self::ExcludeWebsites | Self::IncludeOnlyWebsites)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterEdit {
    Add,
    Remove,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilterEditResult {
    pub config: Config,
    pub changed: bool,
    pub public_suffix_warning: bool,
}

pub fn edit_filter(
    path: impl AsRef<Path>,
    list: FilterList,
    edit: FilterEdit,
    value: &str,
) -> Result<FilterEditResult, ConfigError> {
    let path = path.as_ref();
    let config = Config::load(path)?;
    let result = apply_filter_edit(&config, list, edit, value)?;
    if result.changed {
        save(&result.config, path)?;
    }
    Ok(result)
}

pub fn save(config: &Config, path: impl AsRef<Path>) -> Result<(), ConfigError> {
    config.validate()?;
    let path = path.as_ref();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ConfigError::CreateDirectory {
        path: parent.to_path_buf(),
        source,
    })?;

    let permissions = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(ConfigError::Metadata {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let temporary = temporary_path(path)?;
    let result = write_and_replace(config, path, &temporary, permissions);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn apply_filter_edit(
    config: &Config,
    list: FilterList,
    edit: FilterEdit,
    value: &str,
) -> Result<FilterEditResult, ConfigError> {
    config.validate()?;
    validate_edit_value(config, list, value)?;

    let mut edited = config.clone();
    let values = filter_values_mut(&mut edited, list);
    let existing = values.iter().position(|current| same_value(current, value));
    let changed = match (edit, existing) {
        (FilterEdit::Add, None) => {
            values.push(value.to_owned());
            true
        }
        (FilterEdit::Remove, Some(index)) => {
            values.remove(index);
            true
        }
        (FilterEdit::Add, Some(_)) | (FilterEdit::Remove, None) => false,
    };
    edited.validate()?;

    Ok(FilterEditResult {
        config: edited,
        changed,
        public_suffix_warning: edit == FilterEdit::Add
            && list.is_website()
            && is_public_suffix(value),
    })
}

fn validate_edit_value(config: &Config, list: FilterList, value: &str) -> Result<(), ConfigError> {
    let mut candidate = config.clone();
    let values = filter_values_mut(&mut candidate, list);
    if !values.iter().any(|current| same_value(current, value)) {
        values.push(value.to_owned());
    }
    candidate.validate()
}

fn filter_values_mut(config: &mut Config, list: FilterList) -> &mut Vec<String> {
    match list {
        FilterList::ExcludeApps => &mut config.filter.exclude_apps,
        FilterList::IncludeOnlyApps => &mut config.filter.include_only_apps,
        FilterList::ExcludeWebsites => &mut config.filter.exclude_websites,
        FilterList::IncludeOnlyWebsites => &mut config.filter.include_only_websites,
    }
}

fn same_value(left: &str, right: &str) -> bool {
    left.to_lowercase() == right.to_lowercase()
}

fn is_public_suffix(value: &str) -> bool {
    let normalized = value.trim_end_matches('.').to_ascii_lowercase();
    PUBLIC_SUFFIX_SNAPSHOT
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .flat_map(str::split_ascii_whitespace)
        .any(|suffix| suffix == normalized)
}

fn temporary_path(path: &Path) -> Result<PathBuf, ConfigError> {
    let file_name = path
        .file_name()
        .ok_or_else(|| ConfigError::InvalidPath(path.to_path_buf()))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary_name = OsString::from(".");
    temporary_name.push(file_name);
    temporary_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        NEXT_TEMPORARY_ID.fetch_add(1, Ordering::Relaxed)
    ));
    Ok(parent.join(temporary_name))
}

fn write_and_replace(
    config: &Config,
    path: &Path,
    temporary: &Path,
    permissions: Option<fs::Permissions>,
) -> Result<(), ConfigError> {
    let mut encoded = toml::to_string_pretty(config)?;
    if !encoded.ends_with('\n') {
        encoded.push('\n');
    }

    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(temporary)
        .map_err(|source| ConfigError::CreateTemporary {
            path: temporary.to_path_buf(),
            source,
        })?;
    file.write_all(encoded.as_bytes())
        .map_err(|source| ConfigError::WriteTemporary {
            path: temporary.to_path_buf(),
            source,
        })?;
    file.sync_all()
        .map_err(|source| ConfigError::SyncTemporary {
            path: temporary.to_path_buf(),
            source,
        })?;
    drop(file);

    if let Some(permissions) = permissions {
        fs::set_permissions(temporary, permissions).map_err(|source| {
            ConfigError::SetPermissions {
                path: temporary.to_path_buf(),
                source,
            }
        })?;
    }
    fs::rename(temporary, path).map_err(|source| ConfigError::Replace {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn add_is_case_insensitively_deduplicated_and_remove_uses_the_same_key() {
        let path = test_path("dedupe");
        let added = edit_filter(
            &path,
            FilterList::IncludeOnlyApps,
            FilterEdit::Add,
            "com.apple.Safari",
        )
        .expect("valid app should be added");
        assert!(added.changed);

        let duplicate = edit_filter(
            &path,
            FilterList::IncludeOnlyApps,
            FilterEdit::Add,
            "COM.APPLE.SAFARI",
        )
        .expect("duplicate app should be a no-op");
        assert!(!duplicate.changed);
        assert_eq!(
            duplicate.config.filter.include_only_apps,
            ["com.apple.Safari"]
        );

        let removed = edit_filter(
            &path,
            FilterList::IncludeOnlyApps,
            FilterEdit::Remove,
            "COM.APPLE.SAFARI",
        )
        .expect("case-insensitive remove should work");
        assert!(removed.changed);
        assert!(removed.config.filter.include_only_apps.is_empty());
        remove_test_tree(&path);
    }

    #[test]
    fn invalid_add_and_remove_values_do_not_create_a_configuration() {
        let path = test_path("invalid");
        for (list, edit, value) in [
            (FilterList::ExcludeApps, FilterEdit::Add, ""),
            (FilterList::ExcludeApps, FilterEdit::Remove, " app "),
            (
                FilterList::ExcludeWebsites,
                FilterEdit::Add,
                "https://example.com",
            ),
        ] {
            assert!(edit_filter(&path, list, edit, value).is_err());
            assert!(!path.exists());
        }
        remove_test_tree(&path);
    }

    #[test]
    fn save_creates_parent_and_persists_a_valid_effective_configuration() {
        let path = test_path("save");
        let mut config = Config::default();
        config.filter.exclude_websites = vec!["example.com".to_owned()];

        save(&config, &path).expect("configuration should be saved atomically");

        assert_eq!(
            Config::load(&path).expect("saved config should load"),
            config
        );
        let entries: Vec<_> = fs::read_dir(path.parent().expect("test path parent"))
            .expect("test directory should be readable")
            .collect::<Result<_, _>>()
            .expect("directory entries should be readable");
        assert_eq!(entries.len(), 1, "temporary file must not remain");
        remove_test_tree(&path);
    }

    #[test]
    fn add_warns_for_snapshot_public_suffixes_only() {
        for suffix in ["com", "co.jp"] {
            let path = test_path(&format!("suffix-{}", suffix.replace('.', "-")));
            let result = edit_filter(&path, FilterList::ExcludeWebsites, FilterEdit::Add, suffix)
                .expect("public suffix is valid configuration syntax");
            assert!(result.public_suffix_warning, "{suffix}");
            remove_test_tree(&path);
        }

        let path = test_path("registrable-domain");
        let result = edit_filter(
            &path,
            FilterList::ExcludeWebsites,
            FilterEdit::Add,
            "example.com",
        )
        .expect("registrable domain should be added");
        assert!(!result.public_suffix_warning);
        remove_test_tree(&path);
    }

    fn test_path(label: &str) -> PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "zanei-config-edit-{label}-{}-{id}",
                std::process::id()
            ))
            .join("nested")
            .join("config.toml")
    }

    fn remove_test_tree(path: &Path) {
        let root = path
            .ancestors()
            .nth(2)
            .expect("test path has a generated root");
        let _ = fs::remove_dir_all(root);
    }
}
