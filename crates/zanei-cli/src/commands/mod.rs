mod config;
mod control;
mod doctor;
mod filter;
mod output;
mod purge;
mod read;
mod record;
mod status;
pub(crate) use status::RETIRED_STORE_DEGRADED_COMPONENT;

use zanei_core::config::Config;
use zanei_core::store::DaemonMode;

use crate::cli::{Cli, Command};
use crate::error::CliError;
use crate::paths::Paths;
use crate::setup::{SetupRequest, execute as setup_agent};

pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_MISSING_PERMISSIONS: u8 = 3;
pub const EXIT_NO_DAEMON: u8 = 4;

pub fn run(cli: Cli) -> Result<u8, CliError> {
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
        Command::Start(args) => control::start(&paths, args.foreground, cli.quiet, cli.json),
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
        Command::Filter(args) => filter::run(&paths.config, args, cli.quiet),
        Command::Config(args) => config::run(&paths.config, &paths.store, args, cli.quiet),
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
            crate::daemon::run_daemon(&paths.config, &paths.store, DaemonMode::Launchd)?;
            Ok(EXIT_SUCCESS)
        }
    }
}

fn mcp_permissions_ok(config: &Config) -> Result<bool, String> {
    doctor::permissions_ok(config).map_err(|error| error.to_string())
}
