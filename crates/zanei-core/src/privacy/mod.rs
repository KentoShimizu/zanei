//! Capture-time filtering and free-text redaction.

mod matcher;
mod redactor;

use crate::config::FilterConfig;
use crate::normalize::{NormalizedEvent, enforce_size_limits};
use crate::schema::{App, Element, Event, EventData};

pub use matcher::{BUILT_IN_EXCLUDED_APP_NAMES, BUILT_IN_EXCLUDED_BUNDLE_IDS};

use matcher::{app_is_allowed, extract_url_host, host_is_allowed};
use redactor::redact_event;

/// Chrome is the only browser whose window mode and URL Zanei can read, so website
/// scope rules apply to it alone.
pub const CHROME_BUNDLE_ID: &str = "com.google.Chrome";

#[derive(Clone, Debug)]
pub struct PrivacyFilter {
    config: FilterConfig,
}

impl PrivacyFilter {
    pub fn new(config: FilterConfig) -> Self {
        Self { config }
    }

    /// Applies the capture-time privacy boundary.
    ///
    /// Browser events without a strict, hierarchical URL host are rejected rather than
    /// bypassing website policy.
    pub fn process(&self, normalized: NormalizedEvent) -> Option<Event> {
        let NormalizedEvent {
            mut event,
            capture_context,
        } = normalized;
        if !app_is_allowed(
            &event.app,
            &self.config.include_only_apps,
            &self.config.exclude_apps,
        ) {
            return None;
        }

        let website_host = if event.event_type.starts_with("browser.") {
            Some(extract_url_host(browser_url(&event)?)?)
        } else {
            capture_context.website_host
        };
        let global_host_allowed = host_scope_is_allowed(
            &event.app,
            website_host.as_deref(),
            &self.config.include_only_websites,
            &self.config.exclude_websites,
        );
        if event.event_type.starts_with("browser.") && !global_host_allowed {
            return None;
        }

        if !global_host_allowed
            || !scoped_event_is_allowed(
                &event.app,
                website_host.as_deref(),
                &self.config.text_content,
            )
        {
            suppress_text_content(&mut event.data, &mut event.element);
        }
        if event.event_type.starts_with("content.")
            && !self.content_snapshot_is_allowed(&event.app, website_host.as_deref())
        {
            return None;
        }

        event = redact_event(event, &self.config.redactors);
        enforce_size_limits(&mut event);
        Some(event)
    }

    /// Rechecks whether a snapshot may cross the capture-time privacy boundary.
    #[must_use]
    pub fn content_snapshot_is_allowed(&self, app: &App, website_host: Option<&str>) -> bool {
        app_is_allowed(
            app,
            &self.config.include_only_apps,
            &self.config.exclude_apps,
        ) && host_scope_is_allowed(
            app,
            website_host,
            &self.config.include_only_websites,
            &self.config.exclude_websites,
        ) && scoped_event_is_allowed(app, website_host, &self.config.content_snapshot)
    }
}

/// Removes optional text bodies while preserving the event and its factual metadata.
pub fn suppress_text_content(data: &mut EventData, element: &mut Option<Element>) {
    if let Some(element) = element {
        element.value = None;
    }
    match data {
        EventData::InputKey(value) => value.text = None,
        EventData::UiValue(value) => value.text = None,
        EventData::ClipboardCopy(value) => {
            value.text = None;
            value.size_bytes = None;
        }
        EventData::ClipboardPaste(value) => {
            value.text = None;
            value.size_bytes = None;
        }
        EventData::AppActivate(_)
        | EventData::AppLaunch(_)
        | EventData::AppTerminate(_)
        | EventData::WindowFocus(_)
        | EventData::WindowTitle(_)
        | EventData::UiFocus(_)
        | EventData::UiClick(_)
        | EventData::InputScroll(_)
        | EventData::BrowserNavigate(_) => {}
    }
}

/// Applies the configured website rules to an absolute hierarchical URL.
///
/// Invalid or hostless URLs are denied so callers cannot bypass website policy
/// when the URL cannot be classified.
#[must_use]
pub fn website_is_allowed(url: &str, config: &FilterConfig) -> bool {
    extract_url_host(url).is_some_and(|host| {
        host_is_allowed(
            &host,
            &config.include_only_websites,
            &config.exclude_websites,
        )
    })
}

/// Extracts a normalized host for capture-only website policy context.
#[must_use]
pub fn website_host(url: &str) -> Option<String> {
    extract_url_host(url)
}

fn host_scope_is_allowed(
    app: &App,
    host: Option<&str>,
    include_only: &[String],
    exclude: &[String],
) -> bool {
    if app.bundle_id.as_deref() != Some(CHROME_BUNDLE_ID) {
        return true;
    }
    host.is_some_and(|host| host_is_allowed(host, include_only, exclude))
}

fn scoped_event_is_allowed(
    app: &App,
    host: Option<&str>,
    config: &crate::config::ScopedFilterConfig,
) -> bool {
    app_is_allowed(app, &config.include_only_apps, &config.exclude_apps)
        && host_scope_is_allowed(
            app,
            host,
            &config.include_only_websites,
            &config.exclude_websites,
        )
}

fn browser_url(event: &Event) -> Option<&str> {
    match &event.data {
        EventData::BrowserNavigate(data) => data.url.as_deref(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{FilterConfig, RedactorKind};
    use crate::normalize::{URL_TITLE_FIELD_MAX_BYTES, enforce_size_limits};
    use crate::schema::{
        App, BrowserMode, BrowserNavigateData, Event, EventData, Redaction, Window,
    };

    use super::*;

    #[test]
    fn filters_apps_using_bundle_id_before_display_name() {
        let config = FilterConfig {
            include_only_apps: vec!["Safari".to_owned()],
            ..FilterConfig::default()
        };

        assert!(
            PrivacyFilter::new(config)
                .process(normalized(browser_event("https://example.com")))
                .is_none()
        );
    }

    #[test]
    fn built_in_exclusion_cannot_be_overridden_by_include_only() {
        let mut event = browser_event("https://example.com");
        event.app.name = "Different Name".to_owned();
        event.app.bundle_id = Some("com.1password.1password".to_owned());
        let config = FilterConfig {
            include_only_apps: vec!["com.1password.1password".to_owned()],
            exclude_apps: Vec::new(),
            ..FilterConfig::default()
        };

        assert!(
            PrivacyFilter::new(config)
                .process(normalized(event))
                .is_none()
        );
    }

    #[test]
    fn browser_filter_uses_dot_boundary_suffix_matching() {
        let filter = PrivacyFilter::new(FilterConfig {
            exclude_websites: vec!["example.com".to_owned()],
            ..FilterConfig::default()
        });

        assert!(
            filter
                .process(normalized(browser_event("https://api.example.com/path")))
                .is_none()
        );
        assert!(
            filter
                .process(normalized(browser_event("https://evil-example.com")))
                .is_some()
        );
    }

    #[test]
    fn browser_event_with_malformed_or_hostless_url_is_dropped() {
        let filter = PrivacyFilter::new(FilterConfig::default());
        assert!(
            filter
                .process(normalized(browser_event("not a url")))
                .is_none()
        );
        assert!(
            filter
                .process(normalized(browser_event("file:///private/path")))
                .is_none()
        );
    }

    #[test]
    fn browser_event_without_a_url_after_size_limiting_is_dropped() {
        let mut event = browser_event(&format!(
            "https://example.com/{}",
            "x".repeat(URL_TITLE_FIELD_MAX_BYTES)
        ));
        enforce_size_limits(&mut event);

        assert!(event.is_truncated());
        assert!(
            PrivacyFilter::new(FilterConfig::default())
                .process(normalized(event))
                .is_none()
        );
    }

    #[test]
    fn redaction_runs_only_after_the_event_passes_filters() {
        let filter = PrivacyFilter::new(FilterConfig {
            redactors: vec![RedactorKind::Email],
            ..FilterConfig::default()
        });
        let redacted = filter
            .process(normalized(browser_event(
                "https://example.com/alice@example.com",
            )))
            .expect("event should pass");

        let EventData::BrowserNavigate(data) = redacted.data else {
            panic!("expected browser data");
        };
        assert_eq!(data.url, "https://example.com/[REDACTED:email]");
        assert_eq!(redacted.redaction.rules, ["email"]);
        assert!(redacted.redaction.applied);
    }

    #[test]
    fn app_filter_uses_raw_name_before_redacting_app_name() {
        let raw_app_name = "Mail alice@example.com";
        let filter = PrivacyFilter::new(FilterConfig {
            include_only_apps: vec![raw_app_name.to_owned()],
            redactors: vec![RedactorKind::Email],
            ..FilterConfig::default()
        });
        let mut event = browser_event("https://example.com");
        event.app.name = raw_app_name.to_owned();
        event.app.bundle_id = None;

        let redacted = filter
            .process(normalized(event))
            .expect("raw app name should pass before redaction");

        assert_eq!(redacted.app.name, "Mail [REDACTED:email]");
        assert!(redacted.redaction.applied);
        assert_eq!(redacted.redaction.rules, ["email"]);
    }

    #[test]
    fn redaction_expansion_is_limited_before_the_event_leaves_privacy() {
        let filter = PrivacyFilter::new(FilterConfig {
            redactors: vec![RedactorKind::Email],
            ..FilterConfig::default()
        });
        let mut event = browser_event("https://example.com");
        event.window.as_mut().expect("window").title = Some("a@b.co ".repeat(500));

        let processed = filter
            .process(normalized(event))
            .expect("event should pass");

        assert_eq!(
            processed
                .window
                .as_ref()
                .and_then(|window| window.title.as_ref()),
            None
        );
        assert!(processed.is_truncated());
        assert_eq!(processed.redaction.rules, ["size_limit", "email"]);
    }

    fn browser_event(url: &str) -> Event {
        Event {
            version: 1,
            id: "evt_01J00000000000000000000000".to_owned(),
            ts: "2026-08-16T00:00:00.000Z".to_owned(),
            mono_ns: 1,
            source: "macos.applescript".to_owned(),
            event_type: "browser.navigate".to_owned(),
            app: App {
                name: "Google Chrome".to_owned(),
                bundle_id: Some("com.google.Chrome".to_owned()),
                pid: Some(1),
            },
            window: Some(Window {
                title: Some("Window".to_owned()),
                id: Some(2),
            }),
            element: None,
            data: EventData::BrowserNavigate(BrowserNavigateData {
                url: url.to_owned().into(),
                tab_title: Some("Tab".to_owned()),
                mode: BrowserMode::Normal,
                transition: None,
            }),
            redaction: Redaction {
                applied: false,
                rules: Vec::new(),
            },
        }
    }

    fn normalized(event: Event) -> NormalizedEvent {
        NormalizedEvent {
            event,
            capture_context: crate::schema::CaptureContext::default(),
        }
    }
}
