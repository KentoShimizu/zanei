use std::path::Path;

use zanei_core::config::{Config, ConfigError, FilterEdit, FilterList, edit_filter};
use zanei_core::privacy::{BUILT_IN_EXCLUDED_APP_NAMES, BUILT_IN_EXCLUDED_BUNDLE_IDS};

use super::EXIT_SUCCESS;
use crate::cli::{FilterAction, FilterArgs, FilterCommand, FilterMutationArgs};
use crate::error::CliError;

pub fn run(config_path: &Path, args: FilterArgs, quiet: bool) -> Result<u8, CliError> {
    match args.command {
        FilterCommand::Show => show(&Config::load(config_path)?),
        FilterCommand::ExcludeApp(args) => {
            mutate(config_path, FilterList::ExcludeApps, args, quiet, true)?;
        }
        FilterCommand::OnlyApp(args) => {
            mutate(config_path, FilterList::IncludeOnlyApps, args, quiet, true)?;
        }
        FilterCommand::ExcludeSite(args) => {
            mutate(config_path, FilterList::ExcludeWebsites, args, quiet, false)?;
        }
        FilterCommand::OnlySite(args) => {
            mutate(
                config_path,
                FilterList::IncludeOnlyWebsites,
                args,
                quiet,
                false,
            )?;
        }
    }
    Ok(EXIT_SUCCESS)
}

fn mutate(
    config_path: &Path,
    list: FilterList,
    args: FilterMutationArgs,
    quiet: bool,
    app_list: bool,
) -> Result<(), CliError> {
    let (edit, value) = match args.action {
        FilterAction::Add { value } => (FilterEdit::Add, value),
        FilterAction::Remove { value } => (FilterEdit::Remove, value),
    };
    let result = edit_filter(config_path, list, edit, &value).map_err(|error| match error {
        ConfigError::DuplicateValue { .. } | ConfigError::InvalidListValue { .. } => {
            CliError::InvalidValue(error.to_string())
        }
        other => CliError::Config(other),
    })?;
    if !quiet {
        if result.changed {
            println!("Updated {}", config_path.display());
        } else {
            println!("No change: {value}");
        }
        if result.public_suffix_warning {
            eprintln!(
                "warning: {value} is a public suffix; use a registrable domain such as example.{value}"
            );
        }
        if app_list && !value.contains('.') {
            eprintln!(
                "notice: app display names can change; a bundle_id such as com.example.App is recommended"
            );
        }
    }
    Ok(())
}

fn show(config: &Config) {
    let app_mode = if config.filter.include_only_apps.is_empty() {
        "deny-list"
    } else {
        "allow-list"
    };
    let site_mode = if config.filter.include_only_websites.is_empty() {
        "deny-list"
    } else {
        "allow-list"
    };
    println!("App mode: {app_mode}");
    print_values("exclude_apps", &config.filter.exclude_apps);
    print_values("include_only_apps", &config.filter.include_only_apps);
    println!("Website mode: {site_mode}");
    print_values("exclude_websites", &config.filter.exclude_websites);
    print_values(
        "include_only_websites",
        &config.filter.include_only_websites,
    );
    println!("Built-in excluded app names:");
    for value in BUILT_IN_EXCLUDED_APP_NAMES {
        println!("  - {value}");
    }
    println!("Built-in excluded bundle IDs:");
    for value in BUILT_IN_EXCLUDED_BUNDLE_IDS {
        println!("  - {value}");
    }
}

fn print_values(label: &str, values: &[String]) {
    println!("{label}:");
    if values.is_empty() {
        println!("  (none)");
    } else {
        for value in values {
            println!("  - {value}");
        }
    }
}
