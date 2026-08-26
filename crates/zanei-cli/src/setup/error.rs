use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("unsupported setup agent `{value}`")]
    UnsupportedAgent { value: String },
    #[error("unsupported setup scope `{value}`")]
    UnsupportedScope { value: String },
    #[error("HOME is required to resolve the requested setup destination")]
    HomeDirectoryMissing,
    #[error("failed to read setup target {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("setup target {path} contains invalid JSON: {source}")]
    InvalidJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("setup target {path} must contain a JSON object at its root")]
    JsonRootNotObject { path: PathBuf },
    #[error("setup target {path} has a non-object `{field}` field")]
    JsonFieldNotObject { path: PathBuf, field: &'static str },
    #[error("failed to serialize setup target {path}: {source}")]
    SerializeJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("setup target has no parent directory: {path}")]
    MissingParent { path: PathBuf },
    #[error("failed to create setup directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write setup target {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to replace setup target {path}: {source}")]
    Replace {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to clean up temporary setup target {path} after `{original}`: {source}")]
    Cleanup {
        path: PathBuf,
        original: Box<SetupError>,
        #[source]
        source: io::Error,
    },
}
