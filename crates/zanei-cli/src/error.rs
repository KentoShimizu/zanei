use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("required environment variable {0} is not set")]
    MissingEnvironment(&'static str),
    #[error("failed to read input: {0}")]
    Input(#[source] std::io::Error),
    #[error("failed to access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration error: {0}")]
    Config(#[from] zanei_core::config::ConfigError),
    #[error("configuration value error: {0}")]
    ConfigSet(#[from] zanei_core::config::ConfigSetError),
    #[error("invalid time expression: {0}")]
    Time(#[from] zanei_core::config::TimeExpressionError),
    #[error("store error: {0}")]
    Store(#[from] zanei_core::store::StoreError),
    #[error("timeline error: {0}")]
    Timeline(#[from] zanei_core::timeline::TimelineError),
    #[error("permission diagnostic failed: {0}")]
    Permission(#[from] zanei_macos::permission::PermissionError),
    #[error("daemon operation failed: {0}")]
    Daemon(#[from] crate::daemon::DaemonError),
    #[error("agent setup failed: {0}")]
    Setup(#[from] crate::setup::SetupError),
    #[error("MCP server failed: {0}")]
    Mcp(#[from] zanei_mcp::ServerError),
    #[error("failed to serialize JSON output: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to serialize TOML output: {0}")]
    Toml(#[from] toml::ser::Error),
    #[error("configuration file already exists at {0}")]
    ConfigAlreadyExists(PathBuf),
    #[error("configuration template is out of sync with option {0}")]
    ConfigTemplateOutOfSync(String),
    #[error(
        "failed to initialize configuration at {path}: {source}; failed to remove the partial file: {cleanup}"
    )]
    ConfigInitializationCleanup {
        path: PathBuf,
        #[source]
        source: std::io::Error,
        cleanup: std::io::Error,
    },
    #[error("invalid command value: {0}")]
    InvalidValue(String),
    #[error("$EDITOR command failed with status {0}")]
    EditorFailed(std::process::ExitStatus),
}

impl CliError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Exit code per the CLI contract: invalid argument values are usage
    /// errors (2), everything else is a general error (1).
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Time(_) | Self::ConfigSet(_) | Self::InvalidValue(_) => 2,
            _ => 1,
        }
    }
}
