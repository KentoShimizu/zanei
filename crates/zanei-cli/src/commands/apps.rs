use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use zanei_collector::{AppDirectory, AppInfo};
use zanei_core::config::Config;
use zanei_core::store::{QueryFilter, StoreError, StoreFailureKind};

use super::EXIT_SUCCESS;
use crate::cli::AppsArgs;
use crate::error::CliError;
use crate::paths::Paths;
use crate::store_access::{self, KeyPrompt};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct AppCandidate {
    pub name: String,
    pub bundle_id: Option<String>,
    pub path: Option<PathBuf>,
    pub installed: bool,
    pub running: bool,
    pub last_used: Option<String>,
}

impl AppCandidate {
    pub(super) fn matches(&self, query: &str) -> bool {
        let query = query.to_lowercase();
        self.name.to_lowercase().contains(&query)
            || self
                .bundle_id
                .as_ref()
                .is_some_and(|bundle_id| bundle_id.to_lowercase().contains(&query))
    }

    pub(super) fn display(&self) -> String {
        match &self.bundle_id {
            Some(bundle_id) => format!("{} ({bundle_id})", self.name),
            None => self.name.clone(),
        }
    }

    fn sources(&self) -> String {
        [
            self.installed.then_some("installed"),
            self.running.then_some("running"),
            self.last_used.as_ref().map(|_| "recent"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
    }
}

#[derive(Debug)]
pub(super) struct AppCollection {
    pub apps: Vec<AppCandidate>,
    pub recent_unavailable: Option<RecentUnavailable>,
    pub installed_unreadable: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RecentUnavailable {
    Missing,
    Locked(String),
    Corrupt(String),
    Unavailable(String),
}

impl std::fmt::Display for RecentUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => formatter.write_str("event store does not exist"),
            Self::Locked(reason) => write!(formatter, "event store is locked: {reason}"),
            Self::Corrupt(reason) => write!(formatter, "event store is corrupt: {reason}"),
            Self::Unavailable(reason) => write!(formatter, "event store is unavailable: {reason}"),
        }
    }
}

#[derive(Serialize)]
struct AppsOutput<'a> {
    apps: &'a [AppCandidate],
    recent_unavailable: Option<String>,
    installed_unreadable: usize,
}

pub fn run(
    paths: &Paths,
    app_directory: &dyn AppDirectory,
    args: AppsArgs,
    json: bool,
) -> Result<u8, CliError> {
    let mut collection = collect(paths, app_directory)?;
    if collection.installed_unreadable > 0 {
        eprintln!(
            "warning: {} app bundles could not be read",
            collection.installed_unreadable
        );
    }
    if let Some(query) = args.query.as_deref() {
        collection.apps.retain(|app| app.matches(query));
        if collection.apps.is_empty() {
            eprintln!("No apps match \"{query}\".");
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string(&AppsOutput {
                apps: &collection.apps,
                recent_unavailable: collection
                    .recent_unavailable
                    .map(|reason| reason.to_string()),
                installed_unreadable: collection.installed_unreadable,
            })?
        );
    } else {
        render_table(&collection.apps)?;
        if let Some(reason) = collection.recent_unavailable {
            eprintln!("Recent apps unavailable: {reason}.");
        }
    }
    Ok(EXIT_SUCCESS)
}

pub(super) fn collect(
    paths: &Paths,
    app_directory: &dyn AppDirectory,
) -> Result<AppCollection, CliError> {
    let installed = app_directory.installed()?;
    let running = app_directory.running()?;
    let (recent, recent_unavailable) = recent_apps(&paths.config, &paths.store);
    let mut apps = Vec::new();
    for app in installed.apps {
        merge(&mut apps, app, AppSource::Installed, None);
    }
    for app in running {
        merge(&mut apps, app, AppSource::Running, None);
    }
    for (app, last_used) in recent {
        merge(&mut apps, app, AppSource::Recent, Some(last_used));
    }
    apps.sort_by(compare_candidates);
    Ok(AppCollection {
        apps,
        recent_unavailable,
        installed_unreadable: installed.unreadable,
    })
}

fn recent_apps(
    config_path: &Path,
    store_path: &Path,
) -> (Vec<(AppInfo, String)>, Option<RecentUnavailable>) {
    match store_path.try_exists() {
        Ok(false) => return (Vec::new(), Some(RecentUnavailable::Missing)),
        Err(error) => {
            return (
                Vec::new(),
                Some(RecentUnavailable::Unavailable(error.to_string())),
            );
        }
        Ok(true) => {}
    }
    let config = match Config::load(config_path) {
        Ok(config) => config,
        Err(error) => {
            return (
                Vec::new(),
                Some(RecentUnavailable::Unavailable(error.to_string())),
            );
        }
    };
    let reader = match store_access::open_reader(store_path, KeyPrompt::Allowed) {
        Ok(reader) => reader,
        Err(error) => return (Vec::new(), Some(recent_failure(error))),
    };
    let result = match reader.query(
        &QueryFilter {
            types: vec!["app.activate".to_owned()],
            ..QueryFilter::default()
        },
        config.output.retention_hours,
    ) {
        Ok(result) => result,
        Err(error) => return (Vec::new(), Some(recent_failure(error))),
    };
    let mut recent = Vec::new();
    for event in result.events {
        if let Err(error) = OffsetDateTime::parse(&event.ts, &Rfc3339) {
            return (
                Vec::new(),
                Some(RecentUnavailable::Corrupt(format!(
                    "invalid app.activate timestamp {:?}: {error}",
                    event.ts
                ))),
            );
        }
        let app = AppInfo {
            name: event.app.name,
            bundle_id: event.app.bundle_id,
            path: None,
        };
        if let Some((_, last_used)) = recent
            .iter_mut()
            .find(|(candidate, _)| same_app(candidate, &app))
        {
            *last_used = event.ts;
        } else {
            recent.push((app, event.ts));
        }
    }
    let unavailable = (!reader.skipped_retired().is_empty()).then(|| {
        RecentUnavailable::Corrupt(
            reader
                .skipped_retired()
                .iter()
                .map(zanei_core::store::SkippedRetired::describe)
                .collect::<Vec<_>>()
                .join("; "),
        )
    });
    (recent, unavailable)
}

fn recent_failure(error: StoreError) -> RecentUnavailable {
    let reason = error.to_string();
    match error.failure_kind() {
        StoreFailureKind::Locked => RecentUnavailable::Locked(reason),
        StoreFailureKind::Corrupt => RecentUnavailable::Corrupt(reason),
        StoreFailureKind::Unavailable => RecentUnavailable::Unavailable(reason),
    }
}

#[derive(Clone, Copy)]
enum AppSource {
    Installed,
    Running,
    Recent,
}

fn merge(
    apps: &mut Vec<AppCandidate>,
    incoming: AppInfo,
    source: AppSource,
    last_used: Option<String>,
) {
    let existing = apps.iter_mut().find(|candidate| {
        same_identity(
            candidate.bundle_id.as_deref(),
            &candidate.name,
            incoming.bundle_id.as_deref(),
            &incoming.name,
        )
    });
    let candidate = match existing {
        Some(candidate) => candidate,
        None => {
            let index = apps.len();
            apps.push(AppCandidate {
                name: incoming.name.clone(),
                bundle_id: incoming.bundle_id.clone(),
                path: None,
                installed: false,
                running: false,
                last_used: None,
            });
            &mut apps[index]
        }
    };
    if candidate.bundle_id.is_none() {
        candidate.bundle_id = incoming.bundle_id;
    }
    match source {
        AppSource::Installed => {
            candidate.name = incoming.name;
            candidate.path = incoming.path;
            candidate.installed = true;
        }
        AppSource::Running => {
            candidate.running = true;
        }
        AppSource::Recent => {
            candidate.last_used = last_used;
        }
    }
}

fn same_app(left: &AppInfo, right: &AppInfo) -> bool {
    same_identity(
        left.bundle_id.as_deref(),
        &left.name,
        right.bundle_id.as_deref(),
        &right.name,
    )
}

fn same_identity(
    left_id: Option<&str>,
    left_name: &str,
    right_id: Option<&str>,
    right_name: &str,
) -> bool {
    match (left_id, right_id) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        _ => left_name.eq_ignore_ascii_case(right_name),
    }
}

fn compare_candidates(left: &AppCandidate, right: &AppCandidate) -> Ordering {
    source_rank(left)
        .cmp(&source_rank(right))
        .then_with(|| match (&left.last_used, &right.last_used) {
            (Some(left), Some(right)) => right.cmp(left),
            _ => Ordering::Equal,
        })
        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
}

fn source_rank(candidate: &AppCandidate) -> u8 {
    if candidate.last_used.is_some() {
        0
    } else if candidate.running {
        1
    } else {
        2
    }
}

fn render_table(apps: &[AppCandidate]) -> Result<(), CliError> {
    if apps.is_empty() {
        return Ok(());
    }
    let name_width = apps
        .iter()
        .map(|app| app.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    let id_width = apps
        .iter()
        .filter_map(|app| app.bundle_id.as_ref())
        .map(|bundle_id| bundle_id.chars().count())
        .max()
        .unwrap_or(9)
        .max(9);
    println!(
        "{:<name_width$}  {:<id_width$}  SOURCES  LAST USED",
        "NAME", "BUNDLE ID"
    );
    let now = OffsetDateTime::now_utc();
    for app in apps {
        let last_used = app
            .last_used
            .as_deref()
            .map(|timestamp| relative_time(timestamp, now))
            .transpose()?
            .unwrap_or_default();
        println!(
            "{:<name_width$}  {:<id_width$}  {}  {}",
            app.name,
            app.bundle_id.as_deref().unwrap_or("-"),
            app.sources(),
            last_used,
        );
    }
    Ok(())
}

fn relative_time(timestamp: &str, now: OffsetDateTime) -> Result<String, CliError> {
    let at = OffsetDateTime::parse(timestamp, &Rfc3339).map_err(|source| {
        CliError::RecentAppTimestamp {
            value: timestamp.to_owned(),
            source,
        }
    })?;
    let seconds = (now - at).whole_seconds().max(0);
    Ok(if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn merge_prefers_installed_name_and_recent_sort_order() {
        let mut apps = Vec::new();
        merge(
            &mut apps,
            AppInfo {
                name: "Recent Name".to_owned(),
                bundle_id: Some("dev.example.app".to_owned()),
                path: None,
            },
            AppSource::Recent,
            Some("2026-08-23T00:00:00Z".to_owned()),
        );
        merge(
            &mut apps,
            AppInfo {
                name: "Installed Name".to_owned(),
                bundle_id: Some("DEV.EXAMPLE.APP".to_owned()),
                path: Some(PathBuf::from("/Applications/App.app")),
            },
            AppSource::Installed,
            None,
        );

        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "Installed Name");
        assert!(apps[0].installed);
        assert_eq!(source_rank(&apps[0]), 0);
    }

    #[test]
    fn json_contract_reports_unreadable_installed_bundles() {
        let apps = Vec::new();
        let value = serde_json::to_value(AppsOutput {
            apps: &apps,
            recent_unavailable: None,
            installed_unreadable: 2,
        })
        .expect("apps output serializes");

        assert_eq!(value["installed_unreadable"], json!(2));
        assert_eq!(value["recent_unavailable"], serde_json::Value::Null);
    }
}
