use std::collections::BTreeSet;

use zanei_collector::Permission;
use zanei_core::config::{CaptureSource, Config};
use zanei_core::privacy::{CHROME_BUNDLE_ID, PrivacyFilter};
use zanei_core::schema::App;
use zanei_core::store::{DaemonPermissions, PermissionState};

const ACCESSIBILITY_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
const INPUT_MONITORING_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent";
const AUTOMATION_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation";

pub(super) fn estimated_permissions(config: &Config) -> BTreeSet<Permission> {
    let sources = &config.capture.sources;
    let capture_ui = sources.contains(&CaptureSource::Ui);
    let capture_input = sources.contains(&CaptureSource::Input);
    let capture_browser = sources.contains(&CaptureSource::Browser);
    let mut permissions = BTreeSet::new();
    if sources.contains(&CaptureSource::Window)
        || capture_ui
        || capture_input
        || capture_browser
        || config.capture.content_snapshot
    {
        permissions.insert(Permission::Accessibility);
    }
    if capture_input || capture_ui {
        permissions.insert(Permission::InputMonitoring);
    }
    let chrome = App {
        name: "Google Chrome".to_owned(),
        bundle_id: Some(CHROME_BUNDLE_ID.to_owned()),
        pid: None,
    };
    let chrome_required = sources.contains(&CaptureSource::Browser)
        || config.capture.text_content && (capture_ui || capture_input)
        || config.capture.content_snapshot
            && PrivacyFilter::new(config.filter.clone()).content_snapshot_app_is_allowed(&chrome);
    if chrome_required {
        permissions.insert(Permission::Automation {
            bundle_id: CHROME_BUNDLE_ID.to_owned(),
        });
    }
    permissions
}

pub(super) fn snapshot_status(
    snapshot: &DaemonPermissions,
    permission: &Permission,
) -> Option<PermissionState> {
    match permission {
        Permission::Accessibility => Some(snapshot.accessibility),
        Permission::InputMonitoring => Some(snapshot.input_monitoring),
        Permission::Automation { bundle_id } => snapshot.automation.get(bundle_id).copied(),
    }
}

pub(super) fn permission_name_and_pane(permission: &Permission) -> (&'static str, &'static str) {
    match permission {
        Permission::Accessibility => ("accessibility", ACCESSIBILITY_PANE),
        Permission::InputMonitoring => ("input_monitoring", INPUT_MONITORING_PANE),
        Permission::Automation { .. } => ("automation", AUTOMATION_PANE),
    }
}

pub(super) const fn status_name(status: PermissionState) -> &'static str {
    match status {
        PermissionState::Granted => "granted",
        PermissionState::Denied => "denied",
        PermissionState::NotDetermined => "not_determined",
    }
}

pub(super) fn accessibility_events(
    sources: &[CaptureSource],
    content_snapshot_enabled: bool,
) -> Vec<&'static str> {
    let mut events = Vec::new();
    if sources.contains(&CaptureSource::Window) {
        events.extend(["window.focus", "window.title"]);
    }
    if sources.contains(&CaptureSource::Ui) {
        events.extend(["ui.focus", "ui.click", "ui.value"]);
    }
    if sources.contains(&CaptureSource::Input) {
        events.extend([
            "input.key",
            "input.scroll",
            "clipboard.copy",
            "clipboard.paste",
        ]);
    }
    if sources.contains(&CaptureSource::Browser) {
        events.push("browser.navigate");
    }
    if content_snapshot_enabled {
        events.push("content.snapshot");
    }
    events
}

pub(super) fn input_events(sources: &[CaptureSource]) -> Vec<&'static str> {
    let mut events = Vec::new();
    if sources.contains(&CaptureSource::Input) {
        events.extend([
            "input.key",
            "input.scroll",
            "clipboard.copy",
            "clipboard.paste",
        ]);
    }
    if sources.contains(&CaptureSource::Ui) {
        events.push("ui.click");
    }
    events
}

#[cfg(test)]
pub(super) fn assert_estimate_matches_collector_matrix() {
    let sources = [
        CaptureSource::App,
        CaptureSource::Window,
        CaptureSource::Ui,
        CaptureSource::Input,
        CaptureSource::Browser,
    ];
    for source_mask in 0..(1 << sources.len()) {
        for text_content in [false, true] {
            for content_snapshot in [false, true] {
                for global_allows_chrome in [false, true] {
                    for snapshot_scope_allows_chrome in [false, true] {
                        let mut config = Config::default();
                        config.capture.sources = sources
                            .iter()
                            .enumerate()
                            .filter_map(|(index, source)| {
                                (source_mask & (1 << index) != 0).then_some(*source)
                            })
                            .collect();
                        config.capture.text_content = text_content;
                        config.capture.content_snapshot = content_snapshot;
                        if !global_allows_chrome {
                            config.filter.exclude_apps.push(CHROME_BUNDLE_ID.to_owned());
                        }
                        if !snapshot_scope_allows_chrome {
                            config
                                .filter
                                .content_snapshot
                                .exclude_apps
                                .push(CHROME_BUNDLE_ID.to_owned());
                        }
                        assert_eq!(
                            estimated_permissions(&config),
                            crate::daemon::required_permissions_for(&config),
                            "permission estimate drifted for {config:?}"
                        );
                    }
                }
            }
        }
    }
}
