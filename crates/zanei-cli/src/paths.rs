use std::path::PathBuf;

use crate::error::CliError;

const DEFAULT_CONFIG_RELATIVE_PATH: &str = ".config/zanei/config.toml";
const DEFAULT_STORE_RELATIVE_PATH: &str = ".local/state/zanei/store.sqlite";

#[derive(Clone, Debug)]
pub struct Paths {
    pub config: PathBuf,
    pub store: PathBuf,
}

impl Paths {
    pub fn resolve(config: Option<PathBuf>, store: Option<PathBuf>) -> Result<Self, CliError> {
        let config = match config {
            Some(path) => path,
            None => home_directory()?.join(DEFAULT_CONFIG_RELATIVE_PATH),
        };
        let store = match store {
            Some(path) => path,
            None => home_directory()?.join(DEFAULT_STORE_RELATIVE_PATH),
        };
        Ok(Self { config, store })
    }
}

fn home_directory() -> Result<PathBuf, CliError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(CliError::MissingEnvironment("HOME"))
}
