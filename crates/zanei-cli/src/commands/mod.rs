mod apps;
mod config;
mod control;
mod doctor;
mod filter;
mod human_text;
mod output;
mod purge;
mod read;
mod record;
mod status;
pub(crate) use status::RETIRED_STORE_DEGRADED_COMPONENT;

use zanei_collector::AppDirectory;
use zanei_core::config::Config;
use zanei_core::store::DaemonMode;

use crate::cli::{Cli, Command};
use crate::error::CliError;
use crate::paths::Paths;
use crate::setup::{SetupRequest, execute as setup_agent};

pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_MISSING_PERMISSIONS: u8 = 3;
pub const EXIT_NO_DAEMON: u8 = 4;

#[cfg(target_os = "macos")]
pub fn run(cli: Cli) -> Result<u8, CliError> {
    run_with_app_directory(cli, &zanei_macos::app_directory::MacosAppDirectory)
}

#[cfg(not(target_os = "macos"))]
pub fn run(cli: Cli) -> Result<u8, CliError> {
    run_with_app_directory(cli, &UnsupportedAppDirectory)
}

pub(crate) fn run_with_app_directory(
    cli: Cli,
    app_directory: &dyn AppDirectory,
) -> Result<u8, CliError> {
    let key_environment =
        crate::store_access::initialize_key_environment().map_err(CliError::InvalidValue)?;
    if key_environment.uses_custom_keychain_identity()
        && (matches!(&cli.command, Command::Start(args) if !args.foreground)
            || matches!(&cli.command, Command::Daemon))
    {
        return Err(CliError::InvalidValue(
            "custom Keychain identity requires `zanei start --foreground`; launchd does not inherit it"
                .to_owned(),
        ));
    }
    let paths = Paths::resolve(cli.config, cli.store)?;
    if cli.verbose > 0 {
        eprintln!(
            "config={} store={}",
            paths.config.display(),
            paths.store.display()
        );
    }

    match cli.command {
        Command::Doctor(args) => doctor::run(&paths.config, &paths.store, args.fix, cli.json),
        Command::Start(args) => control::start(
            &paths,
            args.foreground,
            args.exit_on_stdin_eof,
            cli.quiet,
            cli.json,
        ),
        Command::Stop => control::stop(&paths.store, cli.quiet),
        Command::Pause(args) => control::pause(&paths.store, args.duration.as_deref(), cli.quiet),
        Command::Resume => control::resume(&paths.store, cli.quiet),
        Command::Status => status::run(&paths, cli.json),
        Command::Record(args) => record::run(&paths.config, args),
        Command::Query(args) => read::query(&paths.config, &paths.store, args, cli.json, cli.quiet),
        Command::Timeline(args) => {
            read::timeline(&paths.config, &paths.store, args, cli.json, cli.quiet)
        }
        Command::Export(args) => {
            read::export(&paths.config, &paths.store, args, cli.json, cli.quiet)
        }
        Command::Purge(args) => purge::run(&paths.store, args, cli.quiet),
        Command::Apps(args) => apps::run(&paths, app_directory, args, cli.json),
        Command::Filter(args) => filter::run(&paths, app_directory, args, cli.quiet),
        Command::Config(args) => config::run(&paths, app_directory, args, cli.quiet),
        Command::Mcp => {
            zanei_mcp::run(
                paths.store,
                paths.config,
                mcp_permissions_ok,
                crate::store_access::mcp_store_key,
            )?;
            Ok(EXIT_SUCCESS)
        }
        Command::Setup(args) => {
            let request = SetupRequest {
                agent: args.agent,
                scope: args.scope,
                print: args.print,
                cwd: std::env::current_dir().map_err(CliError::Input)?,
            };
            let report = setup_agent(&request)?;
            if !cli.quiet || args.print {
                print!("{report}");
            }
            Ok(EXIT_SUCCESS)
        }
        Command::Daemon => {
            crate::daemon::run_daemon(&paths.config, &paths.store, DaemonMode::Launchd, false)?;
            Ok(EXIT_SUCCESS)
        }
    }
}

#[cfg(not(target_os = "macos"))]
struct UnsupportedAppDirectory;

#[cfg(not(target_os = "macos"))]
impl AppDirectory for UnsupportedAppDirectory {
    fn installed(
        &self,
    ) -> Result<zanei_collector::InstalledApps, zanei_collector::AppDirectoryError> {
        Ok(zanei_collector::InstalledApps::default())
    }

    fn running(&self) -> Result<Vec<zanei_collector::AppInfo>, zanei_collector::AppDirectoryError> {
        Ok(Vec::new())
    }

    fn installed_by_id(
        &self,
        _: &str,
    ) -> Result<Option<zanei_collector::AppInfo>, zanei_collector::AppDirectoryError> {
        Ok(None)
    }
}

fn mcp_permissions_ok(config: &Config) -> Result<bool, String> {
    doctor::permissions_ok(config).map_err(|error| error.to_string())
}
