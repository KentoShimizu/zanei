//! Agent skill installation and MCP registration planning.

mod assets;
mod error;
mod plan;

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub use error::SetupError;
pub use plan::{Agent, Scope, SetupReport};

#[derive(Debug)]
pub struct SetupRequest {
    pub agent: Agent,
    pub scope: Scope,
    pub print: bool,
    pub cwd: PathBuf,
}

/// Installs the requested integration, or returns a write-free preview.
pub fn execute(request: &SetupRequest) -> Result<SetupReport, SetupError> {
    let home_dir = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(SetupError::HomeDirectoryMissing)?;
    let config_dir = config_directory(&home_dir);

    run_at(
        request.agent,
        request.scope,
        request.print,
        &request.cwd,
        &home_dir,
        &config_dir,
    )
}

fn config_directory(home_dir: &Path) -> PathBuf {
    resolve_config_directory(env::var_os("XDG_CONFIG_HOME"), home_dir)
}

/// XDG declares a relative `XDG_CONFIG_HOME` invalid, so it is ignored rather than
/// resolved against the working directory.
fn resolve_config_directory(xdg_config_home: Option<OsString>, home_dir: &Path) -> PathBuf {
    xdg_config_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home_dir.join(".config"))
}

fn run_at(
    agent: Agent,
    scope: Scope,
    print_only: bool,
    project_dir: &Path,
    home_dir: &Path,
    config_dir: &Path,
) -> Result<SetupReport, SetupError> {
    let installation = plan::Installation::build(agent, scope, project_dir, home_dir, config_dir)?;
    if !print_only {
        installation.apply()?;
    }
    Ok(installation.report(print_only))
}

#[cfg(test)]
mod tests;
