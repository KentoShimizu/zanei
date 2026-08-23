use std::process::ExitCode;

use clap::Parser;
use zanei_cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match zanei_cli::run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}
