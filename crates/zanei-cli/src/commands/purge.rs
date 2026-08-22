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
    if !store_path
        .try_exists()
        .map_err(|source| CliError::io(store_path, source))?
    {
        if !quiet {
            println!("Purged 0 events");
        }
        return Ok(EXIT_SUCCESS);
    }
    let mut writer =
        store_access::open_writer(store_path, KeyAccess::Existing, KeyPrompt::Allowed)?;
    // Set-aside plaintext stores hold events too: purge them the same way, and
    // drop the whole file when everything goes.
    let deleted = match cutoff {
        Some(cutoff) => {
            let mut deleted = writer.purge_before(&cutoff)?;
            for retired in retired_plaintext_stores(store_path)? {
                deleted += StoreWriter::open(&retired.path)?.purge_before(&cutoff)?;
            }
            deleted
        }
        None => {
            let mut deleted = writer.purge_all()?;
            for retired in retired_plaintext_stores(store_path)? {
                deleted += StoreWriter::open(&retired.path)
                    .and_then(|mut retired_writer| retired_writer.purge_all())
                    .unwrap_or(0);
                remove_retired(&retired)?;
            }
            deleted
        }
    };
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
