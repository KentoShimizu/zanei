use std::path::Path;

use zanei_collector::AppDirectory;
use zanei_core::config::{
    CaptureSource, Config, ConfigError, FilterEdit, FilterList, FilterScope,
    PRIVATE_WINDOW_UNDETECTABLE_BROWSER_BUNDLE_IDS, edit_filter,
};
use zanei_core::privacy::CHROME_BUNDLE_ID;

mod prompt;
mod render;
mod resolver;

pub(crate) use render::ScopeSummary;

use super::EXIT_SUCCESS;
use super::apps::{AppCandidate, collect};
use crate::cli::{FilterAction, FilterArgs, FilterCommand, FilterMutationArgs, FilterScopeArg};
use crate::error::CliError;
use crate::paths::Paths;

pub fn run(
    paths: &Paths,
    app_directory: &dyn AppDirectory,
    args: FilterArgs,
    quiet: bool,
) -> Result<u8, CliError> {
    let scope = match args.scope {
        None => FilterScope::AllEvents,
        Some(FilterScopeArg::TextContent) => FilterScope::TextContent,
        Some(FilterScopeArg::ContentSnapshot) => FilterScope::ContentSnapshot,
    };
    if matches!(args.command, FilterCommand::Show) && scope != FilterScope::AllEvents {
        return Err(CliError::InvalidValue(
            "filter show does not accept a scope".to_owned(),
        ));
    }
    match args.command {
        FilterCommand::Show => {
            let config = Config::load(&paths.config)?;
            let candidates = collect(paths, app_directory)?.apps;
            render::show(&config, &candidates);
        }
        FilterCommand::ExcludeApp(args) => mutate_app(
            paths,
            app_directory,
            scope,
            FilterList::ExcludeApps,
            args,
            quiet,
        )?,
        FilterCommand::OnlyApp(args) => mutate_app(
            paths,
            app_directory,
            scope,
            FilterList::IncludeOnlyApps,
            args,
            quiet,
        )?,
        FilterCommand::ExcludeSite(args) => mutate_site(
            &paths.config,
            scope,
            FilterList::ExcludeWebsites,
            args,
            quiet,
        )?,
        FilterCommand::OnlySite(args) => mutate_site(
            &paths.config,
            scope,
            FilterList::IncludeOnlyWebsites,
            args,
            quiet,
        )?,
    }
    Ok(EXIT_SUCCESS)
}

fn mutate_app(
    paths: &Paths,
    app_directory: &dyn AppDirectory,
    scope: FilterScope,
    list: FilterList,
    args: FilterMutationArgs,
    quiet: bool,
) -> Result<(), CliError> {
    match args.action {
        FilterAction::Add {
            value,
            unverified: true,
        } => {
            let value = value.ok_or_else(|| {
                CliError::InvalidValue("--unverified requires an app value".to_owned())
            })?;
            let changed = persist(&paths.config, scope, list, FilterEdit::Add, &value)?;
            note_chrome_topology(&paths.config, scope, list, FilterEdit::Add, &value, changed)?;
            if !quiet && changed {
                println!("Added {value}");
            }
            eprintln!("warning: saved unverified app value \"{value}\"; it may not match any app");
            if !quiet {
                let unresolved = resolver::ResolvedApp {
                    stored_value: value.clone(),
                    name: render::browser_name(&value).unwrap_or(&value).to_owned(),
                };
                warn_private_browser(scope, list, FilterEdit::Add, &unresolved);
            }
        }
        FilterAction::Add {
            value,
            unverified: false,
        } => {
            let mut candidates = collect(paths, app_directory)?.apps;
            let selected = match value {
                Some(value) => resolve_add_with_lookup(&value, &mut candidates, app_directory)?,
                None => {
                    let candidate = prompt::choose_app(&candidates, quiet)?;
                    resolver::resolve_add(
                        candidate
                            .bundle_id
                            .as_deref()
                            .unwrap_or(candidate.name.as_str()),
                        &candidates,
                    )
                    .map_err(resolution_error)?
                }
            };
            let changed = persist(
                &paths.config,
                scope,
                list,
                FilterEdit::Add,
                &selected.stored_value,
            )?;
            note_chrome_topology(
                &paths.config,
                scope,
                list,
                FilterEdit::Add,
                &selected.stored_value,
                changed,
            )?;
            if !quiet {
                if changed {
                    println!("{}", selected.added_message());
                } else {
                    println!("No change: {}", selected.stored_value);
                }
                warn_private_browser(scope, list, FilterEdit::Add, &selected);
            }
        }
        FilterAction::Remove { value } => {
            let candidates = collect(paths, app_directory)?.apps;
            let config = Config::load(&paths.config)?;
            let current = list_values(&config, scope, list);
            let selected =
                resolver::resolve_remove(&value, current, &candidates).map_err(resolution_error)?;
            let changed = persist(
                &paths.config,
                scope,
                list,
                FilterEdit::Remove,
                &selected.stored_value,
            )?;
            note_chrome_topology(
                &paths.config,
                scope,
                list,
                FilterEdit::Remove,
                &selected.stored_value,
                changed,
            )?;
            if !quiet {
                if changed {
                    println!("Removed {}", selected.stored_value);
                } else {
                    println!("No change: {}", selected.stored_value);
                }
                warn_private_browser(scope, list, FilterEdit::Remove, &selected);
            }
        }
    }
    Ok(())
}

fn note_chrome_topology(
    config_path: &Path,
    scope: FilterScope,
    list: FilterList,
    edit: FilterEdit,
    value: &str,
    changed: bool,
) -> Result<(), CliError> {
    let config = Config::load(config_path)?;
    if chrome_topology_note_required(&config, scope, list, edit, value, changed) {
        eprintln!(
            "note: if the daemon started while Chrome was excluded, run `zanei restart` so Chrome windows are tracked"
        );
    }
    Ok(())
}

fn chrome_topology_note_required(
    config: &Config,
    scope: FilterScope,
    list: FilterList,
    edit: FilterEdit,
    value: &str,
    changed: bool,
) -> bool {
    let admits_chrome = changed
        && value.eq_ignore_ascii_case(CHROME_BUNDLE_ID)
        && matches!(scope, FilterScope::AllEvents | FilterScope::ContentSnapshot)
        && matches!(
            (list, edit),
            (FilterList::ExcludeApps, FilterEdit::Remove)
                | (FilterList::IncludeOnlyApps, FilterEdit::Add)
        );
    let topology_may_exclude_chrome = !config.capture.text_content
        || !config.capture.sources.contains(&CaptureSource::Ui)
        || !config.capture.sources.contains(&CaptureSource::Input);
    admits_chrome && topology_may_exclude_chrome
}

fn resolve_add_with_lookup(
    value: &str,
    candidates: &mut Vec<AppCandidate>,
    app_directory: &dyn AppDirectory,
) -> Result<resolver::ResolvedApp, CliError> {
    match resolver::resolve_add(value, candidates) {
        Ok(resolved) => Ok(resolved),
        // A dotted value may be a bundle ID of an app outside the scanned folders; ask the
        // platform. A lookup failure is reported as "unresolved" with the platform's reason
        // (usage error, nothing saved) rather than as a command failure, because the outcome
        // is the same: the value was not verified, and --unverified is the escape hatch.
        Err(error) if value.contains('.') => match app_directory.installed_by_id(value) {
            Ok(Some(app)) => {
                candidates.push(resolver::candidate_from_info(app));
                resolver::resolve_add(value, candidates).map_err(resolution_error)
            }
            Ok(None) => Err(resolution_error(error)),
            Err(lookup) => Err(CliError::InvalidValue(format!(
                "No app matches \"{value}\": the installed-app lookup failed ({lookup}). \
                 Use --unverified to save it without verification."
            ))),
        },
        Err(error) => Err(resolution_error(error)),
    }
}

fn mutate_site(
    config_path: &Path,
    scope: FilterScope,
    list: FilterList,
    args: FilterMutationArgs,
    quiet: bool,
) -> Result<(), CliError> {
    let (edit, value) = match args.action {
        FilterAction::Add {
            value: Some(value),
            unverified: false,
        } => (FilterEdit::Add, value),
        FilterAction::Remove { value } => (FilterEdit::Remove, value),
        FilterAction::Add { value: None, .. } => {
            return Err(CliError::InvalidValue(
                "a website value is required".to_owned(),
            ));
        }
        FilterAction::Add {
            unverified: true, ..
        } => {
            return Err(CliError::InvalidValue(
                "--unverified applies only to app lists".to_owned(),
            ));
        }
    };
    let result = edit_filter(config_path, scope, list, edit, &value).map_err(config_edit_error)?;
    if !quiet {
        if result.changed {
            println!(
                "{} {value}",
                match edit {
                    FilterEdit::Add => "Added",
                    FilterEdit::Remove => "Removed",
                }
            );
        } else {
            println!("No change: {value}");
        }
        if result.public_suffix_warning {
            eprintln!(
                "warning: {value} is a public suffix; use a registrable domain such as example.{value}"
            );
        }
    }
    Ok(())
}

fn persist(
    config_path: &Path,
    scope: FilterScope,
    list: FilterList,
    edit: FilterEdit,
    value: &str,
) -> Result<bool, CliError> {
    edit_filter(config_path, scope, list, edit, value)
        .map(|result| result.changed)
        .map_err(config_edit_error)
}

fn config_edit_error(error: ConfigError) -> CliError {
    match error {
        ConfigError::DuplicateValue { .. } | ConfigError::InvalidListValue { .. } => {
            CliError::InvalidValue(error.to_string())
        }
        other => CliError::Config(other),
    }
}

fn resolution_error(error: resolver::ResolveError) -> CliError {
    CliError::InvalidValue(error.to_string())
}

fn list_values(config: &Config, scope: FilterScope, list: FilterList) -> &[String] {
    match (scope, list) {
        (FilterScope::AllEvents, FilterList::ExcludeApps) => &config.filter.exclude_apps,
        (FilterScope::AllEvents, FilterList::IncludeOnlyApps) => &config.filter.include_only_apps,
        (FilterScope::TextContent, FilterList::ExcludeApps) => {
            &config.filter.text_content.exclude_apps
        }
        (FilterScope::TextContent, FilterList::IncludeOnlyApps) => {
            &config.filter.text_content.include_only_apps
        }
        (FilterScope::ContentSnapshot, FilterList::ExcludeApps) => {
            &config.filter.content_snapshot.exclude_apps
        }
        (FilterScope::ContentSnapshot, FilterList::IncludeOnlyApps) => {
            &config.filter.content_snapshot.include_only_apps
        }
        (_, FilterList::ExcludeWebsites | FilterList::IncludeOnlyWebsites) => {
            unreachable!("app mutation requested website values")
        }
    }
}

fn warn_private_browser(
    scope: FilterScope,
    list: FilterList,
    edit: FilterEdit,
    app: &resolver::ResolvedApp,
) {
    let private_undetectable = PRIVATE_WINDOW_UNDETECTABLE_BROWSER_BUNDLE_IDS
        .iter()
        .any(|bundle_id| bundle_id.eq_ignore_ascii_case(&app.stored_value));
    let adding_only = list == FilterList::IncludeOnlyApps && edit == FilterEdit::Add;
    let removing_content_exclusion = list == FilterList::ExcludeApps
        && edit == FilterEdit::Remove
        && matches!(
            scope,
            FilterScope::TextContent | FilterScope::ContentSnapshot
        );
    if private_undetectable && (adding_only || removing_content_exclusion) {
        eprintln!(
            "warning: private-window detection is unavailable for {}; private-window content may be captured",
            app.name
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_topology_note_is_limited_to_admitting_content_mutations() {
        let config = Config::default();
        assert!(chrome_topology_note_required(
            &config,
            FilterScope::ContentSnapshot,
            FilterList::ExcludeApps,
            FilterEdit::Remove,
            CHROME_BUNDLE_ID,
            true,
        ));
        assert!(chrome_topology_note_required(
            &config,
            FilterScope::AllEvents,
            FilterList::IncludeOnlyApps,
            FilterEdit::Add,
            CHROME_BUNDLE_ID,
            true,
        ));
        assert!(!chrome_topology_note_required(
            &config,
            FilterScope::TextContent,
            FilterList::IncludeOnlyApps,
            FilterEdit::Add,
            CHROME_BUNDLE_ID,
            true,
        ));
        assert!(!chrome_topology_note_required(
            &config,
            FilterScope::ContentSnapshot,
            FilterList::ExcludeApps,
            FilterEdit::Remove,
            CHROME_BUNDLE_ID,
            false,
        ));
    }

    #[test]
    fn full_text_topology_does_not_need_the_restart_note() {
        let mut config = Config::default();
        config.capture.text_content = true;

        assert!(!chrome_topology_note_required(
            &config,
            FilterScope::ContentSnapshot,
            FilterList::IncludeOnlyApps,
            FilterEdit::Add,
            CHROME_BUNDLE_ID,
            true,
        ));
    }
}
