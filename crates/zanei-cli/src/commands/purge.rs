use std::io::{self, Write};
use std::path::Path;

use time::OffsetDateTime;
use zanei_core::config::parse_time_expression;
use zanei_core::normalize::format_timestamp;
use zanei_core::store::{
    PurgeFilter, StoreFormat, StoreWriter, remove_retired, retired_plaintext_stores,
};

use super::EXIT_SUCCESS;
use crate::cli::PurgeArgs;
use crate::error::CliError;
use crate::store_access::{self, KeyAccess, KeyPrompt};

pub fn run(store_path: &Path, args: PurgeArgs, quiet: bool) -> Result<u8, CliError> {
    let filter = if args.all {
        PurgeFilter::all()
    } else {
        let before = args
            .before
            .as_deref()
            .map(|before| parse_time_expression(before, OffsetDateTime::now_utc()))
            .transpose()?
            .map(format_timestamp);
        let types = args
            .types
            .as_deref()
            .map(super::read::parse_types)
            .transpose()?
            .unwrap_or_else(|| vec!["*".to_owned()]);
        PurgeFilter {
            types,
            before,
            app: args.app,
            bundle_id: args.bundle_id,
        }
    };
    if filter.is_universal() && !quiet && !confirmed()? {
        println!("Purge cancelled");
        return Ok(EXIT_SUCCESS);
    }
    // The live store and the set-aside plaintext stores are purged
    // independently: either may exist without the other (a set-aside store
    // outlives a crash before the encrypted store was created), and none of
    // them is created just to be purged.
    let live_store_exists = store_path
        .try_exists()
        .map_err(|source| CliError::io(store_path, source))?;
    let mut deleted = 0;
    if live_store_exists {
        let mut writer =
            store_access::open_writer(store_path, KeyAccess::Existing, KeyPrompt::Allowed)?;
        deleted += writer.purge(&filter)?;
    }
    // Listed after the live purge: the recorder's first start after the
    // upgrade may set the live store aside at any moment, and a set-aside that
    // coincides with the purge waits for the purge's connection, so listing
    // afterwards sees every file the purge has not already emptied.
    for retired in retired_plaintext_stores(store_path)? {
        let actual = StoreFormat::probe(&retired.path)?;
        if actual != StoreFormat::Plaintext {
            return Err(zanei_core::store::StoreError::UnexpectedStoreFormat {
                path: retired.path,
                expected: StoreFormat::Plaintext,
                actual,
            }
            .into());
        }
        let mut retired_writer =
            StoreWriter::open_known(&retired.path, StoreFormat::Plaintext, None)?;
        deleted += retired_writer.purge(&filter)?;
        drop(retired_writer);
        if args.all {
            remove_retired(&retired)?;
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
