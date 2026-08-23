//! OS-independent application discovery contract.

use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppInfo {
    pub name: String,
    pub bundle_id: Option<String>,
    pub path: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppDirectoryError {
    #[error("failed to {operation} at {path}: {source}")]
    FileSystem {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid application metadata at {path}: {reason}")]
    InvalidMetadata { path: PathBuf, reason: String },
    #[error("application directory platform operation failed: {0}")]
    Platform(String),
}

impl AppDirectoryError {
    #[must_use]
    pub fn file_system(
        operation: &'static str,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::FileSystem {
            operation,
            path: path.into(),
            source,
        }
    }
}

pub trait AppDirectory {
    fn installed(&self) -> Result<Vec<AppInfo>, AppDirectoryError>;

    fn running(&self) -> Result<Vec<AppInfo>, AppDirectoryError>;

    fn installed_by_id(&self, bundle_id: &str) -> Result<Option<AppInfo>, AppDirectoryError>;
}
