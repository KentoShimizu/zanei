use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use serde::Serialize;
use time::OffsetDateTime;
use zanei_core::config::{Config, parse_time_expression};
use zanei_core::normalize::format_timestamp;
use zanei_core::store::{QueryFilter, StoreError, export_plain_sqlite};
use zanei_core::timeline::{
    Granularity, TimeRange, TimelineFormat, TimelineOptions, build, serialize,
};

use super::EXIT_SUCCESS;
use super::output::{write_json, write_jsonl, write_table};
use crate::cli::{
    ExportArgs, ExportFormat, GranularityArg, QueryArgs, QueryFormat, TimelineArgs,
    TimelineOutputFormat,
};
use crate::error::CliError;
use crate::store_access::{self, KeyPrompt};

#[derive(Serialize)]
struct TimelineQueryReport<'a> {
    #[serde(flatten)]
    timeline: &'a zanei_core::timeline::Timeline,
    skipped_unknown_types: u64,
}

pub fn query(
    config_path: &Path,
    store_path: &Path,
    args: QueryArgs,
    json: bool,
    quiet: bool,
) -> Result<u8, CliError> {
    let (since, until) = range(&args.since, &args.until)?;
    let types = args
        .types
        .as_deref()
        .map(parse_types)
        .transpose()?
        .unwrap_or_default();
    let filter = QueryFilter {
        since: Some(since),
        until: Some(until),
        types,
        app: args.app,
        bundle_id: args.bundle_id,
        limit: Some(args.limit),
    };
    let configured_retention_hours = Config::load(config_path)?.output.retention_hours;
    let result = store_access::open_reader(store_path, KeyPrompt::Allowed)?
        .query(&filter, configured_retention_hours)?;
    let format = if json { QueryFormat::Json } else { args.format };
    let mut stdout = io::stdout().lock();
    match format {
        QueryFormat::Jsonl => write_jsonl(&result.events, &mut stdout)?,
        QueryFormat::Json => write_json(&result.events, &mut stdout)?,
        QueryFormat::Table => write_table(&result.events, &mut stdout)?,
    }
    warn_unknown_types(result.skipped_unknown_types, quiet);
    Ok(EXIT_SUCCESS)
}

pub fn timeline(
    config_path: &Path,
    store_path: &Path,
    args: TimelineArgs,
    json: bool,
    quiet: bool,
) -> Result<u8, CliError> {
    let configured_retention_hours = Config::load(config_path)?.output.retention_hours;
    let (since, until) = range(&args.since, &args.until)?;
    let result = store_access::open_reader(store_path, KeyPrompt::Allowed)?.query(
        &QueryFilter {
            since: Some(since.clone()),
            until: Some(until.clone()),
            ..QueryFilter::default()
        },
        configured_retention_hours,
    )?;
    let format = if json {
        TimelineFormat::Json
    } else {
        match args.format {
            TimelineOutputFormat::Md => TimelineFormat::Markdown,
            TimelineOutputFormat::Json => TimelineFormat::Json,
        }
    };
    let timeline = build(
        &result.events,
        &TimelineOptions {
            range: TimeRange { since, until },
            token_budget: args.token_budget,
            granularity: match args.granularity {
                GranularityArg::Coarse => Granularity::Coarse,
                GranularityArg::Fine => Granularity::Fine,
            },
            format,
        },
    )?;
    let output = match format {
        TimelineFormat::Json => serde_json::to_string(&TimelineQueryReport {
            timeline: &timeline,
            skipped_unknown_types: result.skipped_unknown_types,
        })?,
        TimelineFormat::Markdown => serialize(&timeline, format)?,
    };
    println!("{output}");
    if format == TimelineFormat::Markdown {
        warn_unknown_types(result.skipped_unknown_types, quiet);
    }
    Ok(EXIT_SUCCESS)
}

pub fn export(
    config_path: &Path,
    store_path: &Path,
    args: ExportArgs,
    json: bool,
    quiet: bool,
) -> Result<u8, CliError> {
    let configured_retention_hours = Config::load(config_path)?.output.retention_hours;
    let (since, until) = range(&args.since, &args.until)?;
    let types = args
        .types
        .as_deref()
        .map(parse_types)
        .transpose()?
        .unwrap_or_else(|| vec!["*".to_owned()]);
    let format = if json {
        ExportFormat::Json
    } else {
        args.format
    };
    if format == ExportFormat::Sqlite {
        let out = args.out.ok_or_else(|| {
            CliError::InvalidValue("--out is required for --format sqlite".to_owned())
        })?;
        return export_sqlite(
            store_path,
            since,
            until,
            types,
            configured_retention_hours,
            &out,
            quiet,
        );
    }
    let result = store_access::open_reader(store_path, KeyPrompt::Allowed)?.query(
        &QueryFilter {
            since: Some(since),
            until: Some(until),
            types,
            ..QueryFilter::default()
        },
        configured_retention_hours,
    )?;
    match args.out {
        Some(path) => {
            let file = File::create(&path).map_err(|source| CliError::io(&path, source))?;
            let mut writer = BufWriter::new(file);
            write_export(&result.events, format, &mut writer)?;
            writer
                .flush()
                .map_err(|source| CliError::io(path, source))?;
        }
        None => {
            let mut stdout = io::stdout().lock();
            write_export(&result.events, format, &mut stdout)?;
        }
    }
    warn_unknown_types(result.skipped_unknown_types, quiet);
    Ok(EXIT_SUCCESS)
}

fn write_export(
    events: &[zanei_core::schema::Event],
    format: ExportFormat,
    writer: &mut impl Write,
) -> Result<(), CliError> {
    match format {
        ExportFormat::Jsonl => write_jsonl(events, writer),
        ExportFormat::Json => write_json(events, writer),
        ExportFormat::Sqlite => unreachable!("sqlite exports are written by export_sqlite"),
    }
}

/// Writes a plaintext SQLite copy of the store's events for the range. The
/// file is created owner-readable and never replaces an existing one; the
/// caller chose to hold this data unencrypted, exactly like a JSONL export.
fn export_sqlite(
    store_path: &Path,
    since: String,
    until: String,
    types: Vec<String>,
    configured_retention_hours: u64,
    out: &Path,
    quiet: bool,
) -> Result<u8, CliError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(out) {
        Ok(_) => {}
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            return Err(CliError::SnapshotAlreadyExists(out.to_path_buf()));
        }
        Err(source) => return Err(CliError::io(out, source)),
    }
    let filter = QueryFilter {
        since: Some(since),
        until: Some(until),
        types,
        ..QueryFilter::default()
    };
    let report = store_access::store_key_for(store_path, KeyPrompt::Allowed).and_then(|key| {
        export_plain_sqlite(
            store_path,
            key.as_ref(),
            &filter,
            configured_retention_hours,
            out,
        )
    });
    match report {
        Ok(report) => {
            if !quiet {
                println!(
                    "Wrote a plaintext SQLite snapshot with {} events to {}",
                    report.events,
                    out.display()
                );
            }
            Ok(EXIT_SUCCESS)
        }
        Err(error) => {
            let _ = fs::remove_file(out);
            Err(error.into())
        }
    }
}

fn range(since: &str, until: &str) -> Result<(String, String), CliError> {
    let now = OffsetDateTime::now_utc();
    let since = parse_time_expression(since, now)?;
    let until = parse_time_expression(until, now)?;
    if since > until {
        return Err(CliError::InvalidValue(
            "--since must not be later than --until".to_owned(),
        ));
    }
    Ok((format_timestamp(since), format_timestamp(until)))
}

pub(super) fn parse_types(input: &str) -> Result<Vec<String>, CliError> {
    let types: Vec<_> = input.split(',').map(str::trim).map(str::to_owned).collect();
    if types.iter().any(String::is_empty) {
        return Err(CliError::InvalidValue(
            "--types contains an empty event type".to_owned(),
        ));
    }
    QueryFilter {
        types: types.clone(),
        ..QueryFilter::default()
    }
    .validate()
    .map_err(|error| match error {
        StoreError::InvalidTypePattern(_) => CliError::InvalidValue(error.to_string()),
        other => CliError::Store(other),
    })?;
    Ok(types)
}

fn warn_unknown_types(skipped_unknown_types: u64, quiet: bool) {
    if skipped_unknown_types > 0 && !quiet {
        eprintln!("warning: skipped {skipped_unknown_types} events with unknown types");
    }
}
