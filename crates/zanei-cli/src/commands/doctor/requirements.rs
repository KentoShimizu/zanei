use std::collections::BTreeSet;

use zanei_collector::Capability;
use zanei_core::config::{CaptureSource, Config};
#[cfg(test)]
use zanei_core::privacy::CHROME_BUNDLE_ID;

pub(super) fn required_capabilities(config: &Config) -> BTreeSet<Capability> {
    let sources = &config.capture.sources;
    let capture_ui = sources.contains(&CaptureSource::Ui);
    let capture_input = sources.contains(&CaptureSource::Input);
    let capture_browser = sources.contains(&CaptureSource::Browser);
    let mut capabilities = BTreeSet::new();
    if sources.contains(&CaptureSource::Window)
        || capture_ui
        || capture_input
        || capture_browser
        || config.capture.content_snapshot
    {
        capabilities.insert(Capability::ReadAccessibilityTree);
    }
    if capture_input || capture_ui {
        capabilities.insert(Capability::ObserveInput);
    }
    if crate::daemon::chrome_tracking_required(&config.capture, &config.filter) {
        capabilities.insert(Capability::AutomateBrowser);
    }
    capabilities
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
                    for text_scope_allows_chrome in [false, true] {
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
                            if !text_scope_allows_chrome {
                                config
                                    .filter
                                    .text_content
                                    .exclude_apps
                                    .push(CHROME_BUNDLE_ID.to_owned());
                            }
                            if !snapshot_scope_allows_chrome {
                                config
                                    .filter
                                    .content_snapshot
                                    .exclude_apps
                                    .push(CHROME_BUNDLE_ID.to_owned());
                            }
                            assert_eq!(
                                required_capabilities(&config),
                                crate::daemon::required_capabilities_for(&config),
                                "capability estimate drifted for {config:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}
