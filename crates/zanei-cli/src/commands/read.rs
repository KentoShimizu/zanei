use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use time::OffsetDateTime;
use zanei_core::config::{Config, parse_time_expression};
use zanei_core::normalize::format_timestamp;
use zanei_core::store::{QueryFilter, StoreError, StoreReader};
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

pub fn query(
    config_path: &Path,
    store_path: &Path,
    args: QueryArgs,
    json: bool,
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
    filter.validate().map_err(|error| match error {
        StoreError::InvalidTypePattern(_) => CliError::InvalidValue(error.to_string()),
        other => CliError::Store(other),
    })?;
    let configured_retention_hours = Config::load(config_path)?.output.retention_hours;
    let events = StoreReader::open(store_path)?.query(&filter, configured_retention_hours)?;
    let format = if json { QueryFormat::Json } else { args.format };
    let mut stdout = io::stdout().lock();
    match format {
        QueryFormat::Jsonl => write_jsonl(&events, &mut stdout)?,
        QueryFormat::Json => write_json(&events, &mut stdout)?,
        QueryFormat::Table => write_table(&events, &mut stdout)?,
    }
    Ok(EXIT_SUCCESS)
}

pub fn timeline(
    config_path: &Path,
    store_path: &Path,
    args: TimelineArgs,
    json: bool,
) -> Result<u8, CliError> {
    let configured_retention_hours = Config::load(config_path)?.output.retention_hours;
    let (since, until) = range(&args.since, &args.until)?;
    let events = StoreReader::open(store_path)?.query(
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
        &events,
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
    println!("{}", serialize(&timeline, format)?);
    Ok(EXIT_SUCCESS)
}

pub fn export(
    config_path: &Path,
    store_path: &Path,
    args: ExportArgs,
    json: bool,
) -> Result<u8, CliError> {
    let configured_retention_hours = Config::load(config_path)?.output.retention_hours;
    let (since, until) = range(&args.since, &args.until)?;
    let events = StoreReader::open(store_path)?.query(
        &QueryFilter {
            since: Some(since),
            until: Some(until),
            ..QueryFilter::default()
        },
        configured_retention_hours,
    )?;
    let format = if json {
        ExportFormat::Json
    } else {
        args.format
    };
    match args.out {
        Some(path) => {
            let file = File::create(&path).map_err(|source| CliError::io(&path, source))?;
            let mut writer = BufWriter::new(file);
            write_export(&events, format, &mut writer)?;
            writer
                .flush()
                .map_err(|source| CliError::io(path, source))?;
        }
        None => {
            let mut stdout = io::stdout().lock();
            write_export(&events, format, &mut stdout)?;
        }
    }
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

fn parse_types(input: &str) -> Result<Vec<String>, CliError> {
    let types: Vec<_> = input.split(',').map(str::trim).map(str::to_owned).collect();
    if types.iter().any(String::is_empty) {
        return Err(CliError::InvalidValue(
            "--types contains an empty event type".to_owned(),
        ));
    }
    Ok(types)
}
