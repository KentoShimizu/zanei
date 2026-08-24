mod cli;
mod commands;
mod daemon;
mod error;
mod executable;
mod paths;
mod permissions;
mod setup;
mod store_access;

pub use cli::Cli;
pub use error::CliError;
use zanei_collector::AppDirectory;

pub fn run(cli: Cli) -> Result<u8, CliError> {
    commands::run(cli)
}

pub fn run_with_app_directory(cli: Cli, app_directory: &dyn AppDirectory) -> Result<u8, CliError> {
    commands::run_with_app_directory(cli, app_directory)
}
