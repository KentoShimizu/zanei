use zanei_core::config::{Config, FilterScope, ScopedFilterConfig};
use zanei_core::privacy::{BUILT_IN_EXCLUDED_APP_NAMES, BUILT_IN_EXCLUDED_BUNDLE_IDS};

use super::super::apps::AppCandidate;

const SUMMARY_PREVIEW_COUNT: usize = 6;
const BROWSER_NAMES: [(&str, &str); 6] = [
    ("com.apple.Safari", "Safari"),
    ("org.mozilla.firefox", "Firefox"),
    ("com.brave.Browser", "Brave"),
    ("com.microsoft.edgemac", "Edge"),
    ("com.vivaldi.Vivaldi", "Vivaldi"),
    ("company.thebrowser.Browser", "Arc"),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScopeMode {
    Exclude,
    Only,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SummaryEntry {
    pub value: String,
    pub name: String,
    pub installed: bool,
}

impl SummaryEntry {
    fn prompt_name(&self) -> &str {
        &self.name
    }

    fn show_label(&self) -> String {
        let label = if self.name.eq_ignore_ascii_case(&self.value) {
            self.value.clone()
        } else {
            format!("{} ({})", self.name, self.value)
        };
        if self.installed {
            label
        } else {
            format!("{label} (not installed)")
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AxisSummary {
    pub mode: ScopeMode,
    pub entries: Vec<SummaryEntry>,
    pub excluded_in_only_mode: Vec<SummaryEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScopeSummary {
    pub apps: AxisSummary,
    pub sites: AxisSummary,
}

impl ScopeSummary {
    pub(crate) fn for_scope(
        config: &Config,
        scope: FilterScope,
        candidates: &[AppCandidate],
    ) -> Self {
        let values = scope_values(config, scope);
        Self {
            apps: axis_summary(&values.include_only_apps, &values.exclude_apps, |value| {
                app_entry(value, candidates)
            }),
            sites: axis_summary(
                &values.include_only_websites,
                &values.exclude_websites,
                |value| SummaryEntry {
                    value: value.to_owned(),
                    name: value.to_owned(),
                    installed: true,
                },
            ),
        }
    }

    pub(crate) fn prompt_apps(&self) -> String {
        prompt_axis(&self.apps, "app")
    }

    pub(crate) fn prompt_sites(&self) -> String {
        prompt_axis(&self.sites, "site")
    }
}

pub(super) fn show(config: &Config, candidates: &[AppCandidate]) {
    for (apps_label, sites_label, scope) in [
        (
            "Apps (all events)",
            "Sites (all events)",
            FilterScope::AllEvents,
        ),
        (
            "Text content — apps",
            "Text content — sites",
            FilterScope::TextContent,
        ),
        (
            "Content snapshots — apps",
            "Content snapshots — sites",
            FilterScope::ContentSnapshot,
        ),
    ] {
        let summary = ScopeSummary::for_scope(config, scope, candidates);
        let built_in = if scope == FilterScope::AllEvents {
            format!(" (+ {} built-in)", BUILT_IN_EXCLUDED_BUNDLE_IDS.len())
        } else {
            String::new()
        };
        println!("{apps_label}: {}{built_in}", show_axis(&summary.apps));
        println!("{sites_label}: {}", show_axis(&summary.sites));
    }
    println!("Built-in excluded apps:");
    for (name, bundle_id) in BUILT_IN_EXCLUDED_APP_NAMES
        .iter()
        .zip(BUILT_IN_EXCLUDED_BUNDLE_IDS)
    {
        println!("  - {name} ({bundle_id})");
    }
}

pub(super) fn browser_name(bundle_id: &str) -> Option<&'static str> {
    BROWSER_NAMES
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(bundle_id))
        .map(|(_, name)| *name)
}

fn scope_values(config: &Config, scope: FilterScope) -> ScopedFilterConfig {
    match scope {
        FilterScope::AllEvents => ScopedFilterConfig {
            exclude_apps: config.filter.exclude_apps.clone(),
            include_only_apps: config.filter.include_only_apps.clone(),
            exclude_websites: config.filter.exclude_websites.clone(),
            include_only_websites: config.filter.include_only_websites.clone(),
        },
        FilterScope::TextContent => config.filter.text_content.clone(),
        FilterScope::ContentSnapshot => config.filter.content_snapshot.clone(),
    }
}

fn axis_summary(
    include_only: &[String],
    exclude: &[String],
    entry: impl Fn(&str) -> SummaryEntry,
) -> AxisSummary {
    if include_only.is_empty() {
        AxisSummary {
            mode: ScopeMode::Exclude,
            entries: exclude.iter().map(|value| entry(value)).collect(),
            excluded_in_only_mode: Vec::new(),
        }
    } else {
        AxisSummary {
            mode: ScopeMode::Only,
            entries: include_only.iter().map(|value| entry(value)).collect(),
            excluded_in_only_mode: exclude.iter().map(|value| entry(value)).collect(),
        }
    }
}

fn app_entry(value: &str, candidates: &[AppCandidate]) -> SummaryEntry {
    if let Some(candidate) = candidates.iter().find(|candidate| {
        candidate
            .bundle_id
            .as_ref()
            .is_some_and(|bundle_id| bundle_id.eq_ignore_ascii_case(value))
            || (candidate.bundle_id.is_none() && candidate.name.eq_ignore_ascii_case(value))
    }) {
        return SummaryEntry {
            value: value.to_owned(),
            name: candidate.name.clone(),
            installed: candidate.installed || candidate.running,
        };
    }
    SummaryEntry {
        value: value.to_owned(),
        name: browser_name(value).unwrap_or(value).to_owned(),
        installed: false,
    }
}

fn prompt_axis(axis: &AxisSummary, noun: &str) -> String {
    match axis.mode {
        ScopeMode::Exclude if axis.entries.is_empty() => format!("every {noun}"),
        ScopeMode::Exclude => format!(
            "every {noun} except {} excluded ({})",
            axis.entries.len(),
            preview_names(&axis.entries)
        ),
        ScopeMode::Only => format!(
            "only {} ({})",
            axis.entries.len(),
            preview_names(&axis.entries)
        ),
    }
}

fn show_axis(axis: &AxisSummary) -> String {
    let mode = match axis.mode {
        ScopeMode::Exclude => "exclude",
        ScopeMode::Only => "only",
    };
    let mut output = format!("{mode} {}", axis.entries.len());
    if !axis.entries.is_empty() {
        output.push_str("    ");
        output.push_str(
            &axis
                .entries
                .iter()
                .map(SummaryEntry::show_label)
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if !axis.excluded_in_only_mode.is_empty() {
        output.push_str("    [also excludes: ");
        output.push_str(
            &axis
                .excluded_in_only_mode
                .iter()
                .map(SummaryEntry::show_label)
                .collect::<Vec<_>>()
                .join(", "),
        );
        output.push(']');
    }
    output
}

fn preview_names(entries: &[SummaryEntry]) -> String {
    let mut names: Vec<_> = entries
        .iter()
        .take(SUMMARY_PREVIEW_COUNT)
        .map(SummaryEntry::prompt_name)
        .collect();
    if entries.len() > SUMMARY_PREVIEW_COUNT {
        names.push("…");
    }
    names.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_content_prompt_uses_friendly_browser_names() {
        let summary =
            ScopeSummary::for_scope(&Config::default(), FilterScope::ContentSnapshot, &[]);
        assert_eq!(
            summary.prompt_apps(),
            "every app except 6 excluded (Safari, Firefox, Brave, Edge, Vivaldi, Arc)"
        );
        assert_eq!(summary.prompt_sites(), "every site");
    }
}
