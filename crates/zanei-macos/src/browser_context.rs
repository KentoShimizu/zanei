//! Shared browser target identity and collector activation policy.

use std::collections::BTreeSet;

use zanei_collector::Capability;
use zanei_core::{
    config::{CaptureConfig, CaptureSource, FilterConfig, capture_policy::BrowserMode},
    privacy::{CHROME_BUNDLE_ID, PrivacyScope, app_is_allowed_for},
    schema::App,
};

use crate::permission::SAFARI_BUNDLE_ID;

/// A browser that has a native observation adapter.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum BrowserTarget {
    Chrome,
    Safari,
}

#[cfg_attr(not(test), allow(dead_code))]
impl BrowserTarget {
    pub(crate) fn from_bundle_id(bundle_id: Option<&str>) -> Option<Self> {
        [Self::Chrome, Self::Safari]
            .into_iter()
            .find(|target| Some(target.bundle_id()) == bundle_id)
    }

    pub const fn bundle_id(self) -> &'static str {
        match self {
            Self::Chrome => CHROME_BUNDLE_ID,
            Self::Safari => SAFARI_BUNDLE_ID,
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Chrome => "Google Chrome",
            Self::Safari => "Safari",
        }
    }

    #[allow(dead_code)]
    pub const fn capability(self) -> Capability {
        match self {
            Self::Chrome => Capability::AutomateBrowser,
            Self::Safari => Capability::AutomateSafari,
        }
    }
}

/// Returns browser adapters needed by the configured capture consumers.
///
/// A browser target remains required when its URL is needed to evaluate an
/// app-owned policy, including policies that block an unavailable URL. The
/// result is a pure description; it does not probe permissions or start a
/// collector.
#[must_use]
#[cfg_attr(not(test), allow(dead_code))]
pub fn required_browser_targets(
    capture: &CaptureConfig,
    filter: &FilterConfig,
) -> BTreeSet<BrowserTarget> {
    [BrowserTarget::Chrome, BrowserTarget::Safari]
        .into_iter()
        .filter(|target| {
            let app = App {
                name: target.display_name().to_owned(),
                bundle_id: Some(target.bundle_id().to_owned()),
                pid: None,
            };
            browser_query_allowed(*target, filter)
                && (capture.sources.contains(&CaptureSource::Browser)
                    || capture.text_content
                        && capture.sources.iter().any(|source| {
                            matches!(source, CaptureSource::Ui | CaptureSource::Input)
                        })
                        && app_is_allowed_for(PrivacyScope::TextContent, &app, filter)
                    || capture.content_snapshot
                        && app_is_allowed_for(PrivacyScope::ContentSnapshot, &app, filter))
        })
        .collect()
}

pub(crate) fn browser_query_allowed(target: BrowserTarget, filter: &FilterConfig) -> bool {
    let app = App {
        name: target.display_name().to_owned(),
        bundle_id: Some(target.bundle_id().to_owned()),
        pid: None,
    };
    if !app_is_allowed_for(PrivacyScope::AllEvents, &app, filter) {
        return false;
    }
    match filter.capture_policy.as_ref() {
        Some(policy) => {
            policy.browser.mode != BrowserMode::Off
                && policy
                    .allowed_apps
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(target.display_name()))
        }
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use zanei_core::config::capture_policy::{
        BrowserPolicy, CapturePolicyConfig, IdePolicy, PolicyAction,
    };

    use super::*;

    fn app_policy(mode: BrowserMode, allowed_apps: &[&str]) -> FilterConfig {
        FilterConfig {
            capture_policy: Some(CapturePolicyConfig {
                allowed_apps: allowed_apps.iter().map(|name| (*name).to_owned()).collect(),
                browser: BrowserPolicy {
                    mode,
                    default_policy: PolicyAction::Allow,
                    on_url_unavailable: PolicyAction::Block,
                    block_auth: false,
                    block_payments: false,
                    allow_list: Vec::new(),
                    block_list: Vec::new(),
                },
                ide: IdePolicy {
                    block_env_files: false,
                    on_file_name_unavailable: PolicyAction::Allow,
                },
            }),
            ..FilterConfig::default()
        }
    }

    #[test]
    fn app_owned_mode_requires_only_allowed_browser_consumers() {
        let capture = CaptureConfig {
            sources: vec![CaptureSource::Browser],
            ..CaptureConfig::default()
        };
        let targets =
            required_browser_targets(&capture, &app_policy(BrowserMode::AllSites, &["Safari"]));
        assert_eq!(targets, BTreeSet::from([BrowserTarget::Safari]));
    }

    #[test]
    fn app_owned_off_or_no_consumer_requires_no_target() {
        let no_consumer = CaptureConfig {
            sources: vec![CaptureSource::App],
            ..CaptureConfig::default()
        };
        assert!(
            required_browser_targets(
                &no_consumer,
                &app_policy(BrowserMode::AllSites, &["Google Chrome", "Safari"]),
            )
            .is_empty()
        );

        let browser = CaptureConfig {
            sources: vec![CaptureSource::Browser],
            ..CaptureConfig::default()
        };
        assert!(
            required_browser_targets(
                &browser,
                &app_policy(BrowserMode::Off, &["Google Chrome", "Safari"]),
            )
            .is_empty()
        );
    }

    #[test]
    fn app_owned_body_consumers_still_query_url_when_unavailable_is_blocked() {
        let capture = CaptureConfig {
            sources: vec![CaptureSource::Ui],
            text_content: true,
            content_snapshot: true,
        };
        let mut filter = app_policy(BrowserMode::Rules, &["Google Chrome", "Safari"]);
        filter.text_content.exclude_apps.clear();
        filter.content_snapshot.exclude_apps.clear();
        let targets = required_browser_targets(&capture, &filter);
        assert_eq!(
            targets,
            BTreeSet::from([BrowserTarget::Chrome, BrowserTarget::Safari])
        );
    }

    #[test]
    fn default_capture_requires_both_browser_adapters() {
        let capture = CaptureConfig::default();
        assert_eq!(
            required_browser_targets(&capture, &FilterConfig::default()),
            BTreeSet::from([BrowserTarget::Chrome, BrowserTarget::Safari])
        );
    }
}
