//! Configuration loading and shared CLI value parsing.

mod edit;
mod scalar_file;
mod time_expression;
#[path = "config/validation.rs"]
mod validation;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, SystemTime};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use edit::{
    ConfigSetError, FilterEdit, FilterEditResult, FilterList, ScalarEditResult, apply_scalar_edit,
    edit_filter, save,
};
pub use scalar_file::{capture_text_content_is_explicit, save_capture_text_content};
pub use time_expression::{TimeExpressionError, parse_duration_expression, parse_time_expression};

pub const CONFIG_WATCH_INTERVAL: StdDuration = StdDuration::from_secs(2);
pub const DEFAULT_BATCH_INTERVAL_SECONDS: u64 = 5;
pub const DEFAULT_RETENTION_HOURS: u64 = 48;

const DEFAULT_EXCLUDED_APPS: [&str; 2] = ["1Password", "Keychain Access"];

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSource {
    App,
    Window,
    Ui,
    Input,
    Browser,
}

impl CaptureSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Window => "window",
            Self::Ui => "ui",
            Self::Input => "input",
            Self::Browser => "browser",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactorKind {
    Email,
    CreditCard,
    Token,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub capture: CaptureConfig,
    pub filter: FilterConfig,
    pub output: OutputConfig,
}

impl Config {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        parse_config(input, Path::new("<inline>"))
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let input = match fs::read_to_string(path) {
            Ok(input) => input,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        parse_config(&input, path)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        validation::validate(self)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub sources: Vec<CaptureSource>,
    pub text_content: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            sources: vec![
                CaptureSource::App,
                CaptureSource::Window,
                CaptureSource::Ui,
                CaptureSource::Input,
                CaptureSource::Browser,
            ],
            text_content: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilterConfig {
    pub exclude_apps: Vec<String>,
    pub include_only_apps: Vec<String>,
    pub exclude_websites: Vec<String>,
    pub include_only_websites: Vec<String>,
    pub redactors: Vec<RedactorKind>,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            exclude_apps: DEFAULT_EXCLUDED_APPS.map(str::to_owned).to_vec(),
            include_only_apps: Vec::new(),
            exclude_websites: Vec::new(),
            include_only_websites: Vec::new(),
            redactors: vec![
                RedactorKind::Email,
                RedactorKind::CreditCard,
                RedactorKind::Token,
            ],
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    pub batch_interval_s: u64,
    pub retention_hours: u64,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            batch_interval_s: DEFAULT_BATCH_INTERVAL_SECONDS,
            retention_hours: DEFAULT_RETENTION_HOURS,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read configuration at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to inspect configuration at {path}: {source}")]
    Metadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid TOML configuration at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("configuration path has no file name: {0}")]
    InvalidPath(PathBuf),
    #[error("failed to create configuration directory at {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create temporary configuration at {path}: {source}")]
    CreateTemporary {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write temporary configuration at {path}: {source}")]
    WriteTemporary {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to sync temporary configuration at {path}: {source}")]
    SyncTemporary {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to preserve configuration permissions at {path}: {source}")]
    SetPermissions {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to atomically replace configuration at {path}: {source}")]
    Replace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("output.batch_interval_s must be greater than zero")]
    ZeroBatchInterval,
    #[error("output.retention_hours must be greater than zero")]
    ZeroRetention,
    #[error("{field} contains a duplicate value: {value}")]
    DuplicateValue { field: &'static str, value: String },
    #[error("{field} contains an invalid value: {value}")]
    InvalidListValue { field: &'static str, value: String },
}

#[derive(Debug)]
pub struct ConfigWatcher {
    path: PathBuf,
    last_modified: Option<SystemTime>,
}

impl ConfigWatcher {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        let last_modified = modified_time(&path)?;
        Ok(Self {
            path,
            last_modified,
        })
    }

    pub fn has_changed(&self) -> Result<bool, ConfigError> {
        Ok(modified_time(&self.path)? != self.last_modified)
    }

    pub fn reload_if_changed(&mut self) -> Result<Option<Config>, ConfigError> {
        let modified = modified_time(&self.path)?;
        if modified == self.last_modified {
            return Ok(None);
        }

        let config = Config::load(&self.path)?;
        self.last_modified = modified;
        Ok(Some(config))
    }
}

fn parse_config(input: &str, path: &Path) -> Result<Config, ConfigError> {
    let config: Config = toml::from_str(input).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    config.validate()?;
    Ok(config)
}

fn modified_time(path: &Path) -> Result<Option<SystemTime>, ConfigError> {
    match fs::metadata(path) {
        Ok(metadata) => metadata
            .modified()
            .map(Some)
            .map_err(|source| ConfigError::Metadata {
                path: path.to_path_buf(),
                source,
            }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Metadata {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, FileTimes, OpenOptions};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration as StdDuration, SystemTime};

    use time::{OffsetDateTime, format_description::well_known::Rfc3339};

    use super::{
        CONFIG_WATCH_INTERVAL, CaptureSource, Config, ConfigError, ConfigWatcher,
        DEFAULT_BATCH_INTERVAL_SECONDS, DEFAULT_RETENTION_HOURS, RedactorKind, TimeExpressionError,
        parse_duration_expression, parse_time_expression,
    };

    static NEXT_TEMP_PATH_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn defaults_match_the_public_configuration_contract() {
        let config = Config::default();

        assert_eq!(
            config.capture.sources,
            [
                CaptureSource::App,
                CaptureSource::Window,
                CaptureSource::Ui,
                CaptureSource::Input,
                CaptureSource::Browser,
            ]
        );
        assert!(!config.capture.text_content);
        assert_eq!(config.filter.exclude_apps, ["1Password", "Keychain Access"]);
        assert!(config.filter.include_only_apps.is_empty());
        assert!(config.filter.exclude_websites.is_empty());
        assert!(config.filter.include_only_websites.is_empty());
        assert_eq!(
            config.filter.redactors,
            [
                RedactorKind::Email,
                RedactorKind::CreditCard,
                RedactorKind::Token,
            ]
        );
        assert_eq!(
            config.output.batch_interval_s,
            DEFAULT_BATCH_INTERVAL_SECONDS
        );
        assert_eq!(config.output.retention_hours, DEFAULT_RETENTION_HOURS);
        assert_eq!(CONFIG_WATCH_INTERVAL.as_secs(), 2);
    }

    #[test]
    fn partial_toml_merges_unspecified_defaults() {
        let config = Config::from_toml(
            r#"
                [capture]
                text_content = true
            "#,
        )
        .expect("partial configuration should parse");

        assert_eq!(config.capture.sources, Config::default().capture.sources);
        assert!(config.capture.text_content);
        assert_eq!(config.filter, Config::default().filter);
        assert_eq!(
            config.output.batch_interval_s,
            DEFAULT_BATCH_INTERVAL_SECONDS
        );
        assert_eq!(config.output.retention_hours, DEFAULT_RETENTION_HOURS);
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_level() {
        let root = Config::from_toml("unexpected = true");
        assert!(matches!(root, Err(ConfigError::Parse { .. })));

        let nested = Config::from_toml("[capture]\nunexpected = true");
        assert!(matches!(nested, Err(ConfigError::Parse { .. })));

        for removed_key in ["mode", "store"] {
            let removed = Config::from_toml(&format!("[output]\n{removed_key} = \"legacy\""));
            assert!(matches!(removed, Err(ConfigError::Parse { .. })));
        }
    }

    #[test]
    fn zero_intervals_are_rejected() {
        let interval = Config::from_toml("[output]\nbatch_interval_s = 0");
        assert!(matches!(interval, Err(ConfigError::ZeroBatchInterval)));

        let retention = Config::from_toml("[output]\nretention_hours = 0");
        assert!(matches!(retention, Err(ConfigError::ZeroRetention)));
    }

    #[test]
    fn missing_file_uses_defaults() {
        let path = temp_config_path();
        assert_eq!(
            Config::load(&path).expect("missing configuration should use defaults"),
            Config::default()
        );
    }

    #[test]
    fn watcher_detects_creation_and_removal() {
        let path = temp_config_path();
        let parent = path.parent().expect("temporary path has a parent");
        let mut watcher = ConfigWatcher::new(&path).expect("missing path is watchable");
        assert!(!watcher.has_changed().expect("metadata check should work"));

        fs::create_dir_all(parent).expect("temporary directory should be created");
        fs::write(&path, "[capture]\ntext_content = true\n")
            .expect("temporary configuration should be written");
        assert!(watcher.has_changed().expect("creation should be detected"));
        let loaded = watcher
            .reload_if_changed()
            .expect("created configuration should reload")
            .expect("creation should produce a configuration");
        assert!(loaded.capture.text_content);
        assert!(
            !watcher
                .has_changed()
                .expect("reload should mark current mtime")
        );

        fs::remove_file(&path).expect("temporary configuration should be removed");
        assert!(watcher.has_changed().expect("removal should be detected"));
        assert_eq!(
            watcher
                .reload_if_changed()
                .expect("removal should reload defaults"),
            Some(Config::default())
        );
        fs::remove_dir_all(parent).expect("temporary directory should be removed");
    }

    #[test]
    fn watcher_detects_mtime_change() {
        let path = temp_config_path();
        let parent = path.parent().expect("temporary path has a parent");
        fs::create_dir_all(parent).expect("temporary directory should be created");
        fs::write(&path, "[capture]\ntext_content = false\n")
            .expect("temporary configuration should be written");
        set_modified_time(&path, SystemTime::UNIX_EPOCH + StdDuration::from_secs(1));
        let mut watcher = ConfigWatcher::new(&path).expect("configuration should be watchable");

        fs::write(&path, "[capture]\ntext_content = true\n")
            .expect("temporary configuration should be updated");
        set_modified_time(&path, SystemTime::UNIX_EPOCH + StdDuration::from_secs(2));

        assert!(
            watcher
                .has_changed()
                .expect("mtime change should be detected")
        );
        let loaded = watcher
            .reload_if_changed()
            .expect("updated configuration should reload")
            .expect("mtime change should produce a configuration");
        assert!(loaded.capture.text_content);
        assert!(
            !watcher
                .has_changed()
                .expect("reload should mark current mtime")
        );
        fs::remove_dir_all(parent).expect("temporary directory should be removed");
    }

    #[test]
    fn parses_relative_time_units_and_now() {
        let now = timestamp("2026-08-16T12:00:00Z");

        assert_eq!(parse_time_expression(" now ", now), Ok(now));
        assert_eq!(
            parse_time_expression("15m", now),
            Ok(timestamp("2026-08-16T11:45:00Z"))
        );
        assert_eq!(
            parse_time_expression("2h", now),
            Ok(timestamp("2026-08-16T10:00:00Z"))
        );
        assert_eq!(
            parse_time_expression("1d", now),
            Ok(timestamp("2026-08-15T12:00:00Z"))
        );
        assert_eq!(
            parse_time_expression("1w", now),
            Ok(timestamp("2026-08-09T12:00:00Z"))
        );
        assert_eq!(
            parse_time_expression("1s", now),
            Ok(timestamp("2026-08-16T11:59:59Z"))
        );
    }

    #[test]
    fn parses_positive_relative_durations() {
        assert_eq!(
            parse_duration_expression(" 30m "),
            Ok(time::Duration::minutes(30))
        );
        assert_eq!(
            parse_duration_expression("2h"),
            Ok(time::Duration::hours(2))
        );
    }

    #[test]
    fn parses_rfc3339_time() {
        let now = timestamp("2026-08-16T12:00:00Z");
        assert_eq!(
            parse_time_expression(" 2026-08-16T09:00:00+09:00 ", now),
            Ok(timestamp("2026-08-16T09:00:00+09:00"))
        );
    }

    #[test]
    fn rejects_invalid_and_overflowing_time_expressions() {
        let now = timestamp("2026-08-16T12:00:00Z");

        assert!(matches!(
            parse_time_expression("", now),
            Err(TimeExpressionError::Empty)
        ));
        for invalid in ["0m", "-1h", "+1h", "1.5h"] {
            assert!(matches!(
                parse_time_expression(invalid, now),
                Err(TimeExpressionError::InvalidRelative(_))
            ));
        }
        assert!(matches!(
            parse_time_expression("1H", now),
            Err(TimeExpressionError::InvalidTimestamp { .. })
        ));
        assert!(matches!(
            parse_time_expression("18446744073709551615w", now),
            Err(TimeExpressionError::Overflow(_))
        ));
        assert!(matches!(
            parse_time_expression("1s", time::Date::MIN.midnight().assume_utc()),
            Err(TimeExpressionError::Overflow(_))
        ));
    }

    #[test]
    fn rejects_non_positive_absolute_and_overflowing_durations() {
        for invalid in ["", "now", "0m", "-1h", "+1h", "1.5h", "1H"] {
            assert!(parse_duration_expression(invalid).is_err(), "{invalid}");
        }
        assert!(matches!(
            parse_duration_expression("18446744073709551615w"),
            Err(TimeExpressionError::Overflow(_))
        ));
    }

    fn temp_config_path() -> PathBuf {
        let id = NEXT_TEMP_PATH_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("zanei-config-test-{}-{id}", std::process::id()))
            .join("config.toml")
    }

    fn timestamp(input: &str) -> OffsetDateTime {
        OffsetDateTime::parse(input, &Rfc3339).expect("test timestamp should be valid RFC3339")
    }

    fn set_modified_time(path: &Path, modified: SystemTime) {
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .expect("temporary configuration should open");
        file.set_times(FileTimes::new().set_modified(modified))
            .expect("temporary configuration mtime should be set");
    }
}
