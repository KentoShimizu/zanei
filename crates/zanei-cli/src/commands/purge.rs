use std::io::{self, Write};
use std::path::Path;

use time::OffsetDateTime;
use zanei_core::config::parse_time_expression;
use zanei_core::normalize::format_timestamp;
use zanei_core::store::{StoreWriter, remove_retired, retired_plaintext_stores};

use super::EXIT_SUCCESS;
use crate::cli::PurgeArgs;
use crate::error::CliError;
use crate::store_access::{self, KeyAccess, KeyPrompt};

pub fn run(store_path: &Path, args: PurgeArgs, quiet: bool) -> Result<u8, CliError> {
    let cutoff = if args.all {
        if !quiet && !confirmed()? {
            println!("Purge cancelled");
            return Ok(EXIT_SUCCESS);
        }
        None
    } else {
        let before = args
            .before
            .as_deref()
            .ok_or_else(|| CliError::InvalidValue("--before is required".to_owned()))?;
        Some(format_timestamp(parse_time_expression(
            before,
            OffsetDateTime::now_utc(),
        )?))
    };
    // The live store and the set-aside plaintext stores are purged
    // independently: either may exist without the other (a set-aside store
    // outlives a crash before the encrypted store was created), and none of
    // them is created just to be purged.
    let live_store_exists = store_path
        .try_exists()
        .map_err(|source| CliError::io(store_path, source))?;
    let retired = retired_plaintext_stores(store_path)?;
    let mut deleted = 0;
    if live_store_exists {
        let mut writer =
            store_access::open_writer(store_path, KeyAccess::Existing, KeyPrompt::Allowed)?;
        deleted += match cutoff.as_deref() {
            Some(cutoff) => writer.purge_before(cutoff)?,
            None => writer.purge_all()?,
        };
    }
    for retired in retired {
        match cutoff.as_deref() {
            Some(cutoff) => deleted += StoreWriter::open(&retired.path)?.purge_before(cutoff)?,
            None => {
                deleted += StoreWriter::open(&retired.path)
                    .and_then(|mut retired_writer| retired_writer.purge_all())
                    .unwrap_or(0);
                remove_retired(&retired)?;
            }
        }
    }
    if !quiet {
        println!("Purged {deleted} events");
    }
    Ok(EXIT_SUCCESS)
}

fn confirmed() -> Result<bool, CliError> {
    eprint!("Delete all stored Zanei events? [y/N] ");
    io::stderr().flush().map_err(CliError::Input)?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(CliError::Input)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
