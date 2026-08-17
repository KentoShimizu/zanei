use crate::cli::{RecordArgs, RecordFormat};
use crate::daemon::RecordOutput;
use crate::error::CliError;

use super::EXIT_SUCCESS;

pub fn run(config_path: &std::path::Path, args: RecordArgs) -> Result<u8, CliError> {
    let output = match (args.stream, args.out) {
        (true, None) => RecordOutput::Stdout,
        (false, Some(path)) => RecordOutput::File(path),
        _ => {
            return Err(CliError::InvalidValue(
                "exactly one of --stream or --out is required".to_owned(),
            ));
        }
    };
    match args.format {
        RecordFormat::Jsonl => {}
    }
    crate::daemon::run_record(config_path, output)?;
    Ok(EXIT_SUCCESS)
}
