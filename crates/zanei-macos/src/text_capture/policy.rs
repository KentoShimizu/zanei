//! Cross-collector authorization for text-bearing event fields.

use std::sync::{Arc, RwLock};

use zanei_core::{
    config::FilterConfig,
    privacy::{CHROME_BUNDLE_ID, PrivacyScope, app_is_allowed_for},
    schema::{App, CaptureContext},
};

use crate::{chrome::ChromeEligibilityTracker, focused_field::FocusedField};

/// A capture-time decision made before a collector reads optional body text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextContentDecision {
    allowed: bool,
    capture_context: CaptureContext,
    chrome_version: Option<u64>,
}

impl TextContentDecision {
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        self.allowed
    }

    #[must_use]
    pub fn capture_context(&self) -> CaptureContext {
        self.capture_context.clone()
    }

    #[must_use]
    pub const fn chrome_version(&self) -> Option<u64> {
        self.chrome_version
    }
}

#[derive(Clone)]
pub struct TextContentPolicy {
    chrome: ChromeEligibilityTracker,
    filter: Arc<RwLock<FilterConfig>>,
}

impl TextContentPolicy {
    pub fn new(chrome: ChromeEligibilityTracker, filter: FilterConfig) -> Self {
        Self {
            chrome,
            filter: Arc::new(RwLock::new(filter)),
        }
    }

    pub fn replace_filter(&self, filter: FilterConfig) {
        match self.filter.write() {
            Ok(mut current) => *current = filter,
            Err(_) => crate::trace::trace!(
                "component=text_capture phase=policy action=replace_filter result=poisoned"
            ),
        }
    }

    #[must_use]
    pub(crate) fn chrome_tracker(&self) -> ChromeEligibilityTracker {
        self.chrome.clone()
    }

    pub(crate) fn decision(&self, app: &App, window_id: Option<i64>) -> TextContentDecision {
        let is_chrome = app.bundle_id.as_deref() == Some(CHROME_BUNDLE_ID);
        let capture_context = if is_chrome {
            app.pid
                .map(|pid| self.chrome.capture_context(pid, window_id))
                .unwrap_or_default()
        } else {
            CaptureContext::default()
        };
        let app_allowed = self
            .filter
            .read()
            .is_ok_and(|filter| app_is_allowed_for(PrivacyScope::TextContent, app, &filter));
        let chrome_allowed = !is_chrome
            || app
                .pid
                .is_some_and(|pid| self.chrome.allows_text(pid, window_id));
        TextContentDecision {
            allowed: app_allowed && chrome_allowed,
            capture_context,
            chrome_version: is_chrome
                .then(|| {
                    app.pid
                        .zip(window_id)
                        .and_then(|(pid, window_id)| self.chrome.state_version(pid, window_id))
                })
                .flatten(),
        }
    }

    pub(crate) fn input_decision(
        &self,
        app: &App,
        window_id: Option<i64>,
        focused_field: Option<FocusedField>,
    ) -> TextContentDecision {
        let mut decision = self.decision(app, window_id);
        decision.allowed &= focused_field.is_some_and(|field| field.class.is_known_text());
        decision
    }
}

#[cfg(test)]
mod tests {
    use zanei_core::{
        config::{FilterConfig, ScopedFilterConfig},
        privacy::CHROME_BUNDLE_ID,
        schema::FieldKind,
    };

    use super::*;
    use crate::chrome::{ChromeEligibilityObservation, chrome_eligibility_channel};
    use crate::focused_field::{FieldClass, FocusedField};

    fn app(name: &str, bundle_id: &str, pid: i64) -> App {
        App {
            name: name.to_owned(),
            bundle_id: Some(bundle_id.to_owned()),
            pid: Some(pid),
        }
    }

    fn known_text_field() -> FocusedField {
        FocusedField {
            generation: 1,
            class: FieldClass::KnownText(FieldKind::Text),
        }
    }

    #[test]
    fn global_and_text_app_scopes_are_both_required() {
        let filter = FilterConfig {
            exclude_apps: vec!["dev.example.GlobalDenied".to_owned()],
            text_content: ScopedFilterConfig {
                exclude_apps: vec!["dev.example.TextDenied".to_owned()],
                ..ScopedFilterConfig::default()
            },
            ..FilterConfig::default()
        };
        let (_, chrome) = chrome_eligibility_channel(filter.clone());
        let policy = TextContentPolicy::new(chrome, filter);

        assert!(
            policy
                .decision(&app("Allowed", "dev.example.Allowed", 7), Some(11))
                .is_allowed()
        );
        assert!(
            !policy
                .decision(&app("Global", "dev.example.GlobalDenied", 7), Some(11))
                .is_allowed()
        );
        assert!(
            !policy
                .decision(&app("Text", "dev.example.TextDenied", 7), Some(11))
                .is_allowed()
        );
    }

    #[test]
    fn chrome_requires_a_normal_allowed_host_and_preserves_context() {
        let filter = FilterConfig::default();
        let (publisher, chrome) = chrome_eligibility_channel(filter.clone());
        let policy = TextContentPolicy::new(chrome, filter);
        let chrome_app = app("Google Chrome", CHROME_BUNDLE_ID, 7);

        assert!(!policy.decision(&chrome_app, Some(11)).is_allowed());
        publisher.observe(
            7,
            ChromeEligibilityObservation::Incognito {
                window_id: Some(11),
            },
        );
        assert!(!policy.decision(&chrome_app, Some(11)).is_allowed());
        publisher.observe(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: "https://example.com".to_owned(),
            },
        );
        let decision = policy.decision(&chrome_app, Some(11));
        assert!(decision.is_allowed());
        assert_eq!(
            decision.capture_context().website_host.as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn chrome_combines_global_and_text_website_scopes() {
        let filter = FilterConfig {
            exclude_websites: vec!["global.example".to_owned()],
            text_content: ScopedFilterConfig {
                exclude_websites: vec!["text.example".to_owned()],
                ..ScopedFilterConfig::default()
            },
            ..FilterConfig::default()
        };
        let (publisher, chrome) = chrome_eligibility_channel(filter.clone());
        let policy = TextContentPolicy::new(chrome, filter);
        let chrome_app = app("Google Chrome", CHROME_BUNDLE_ID, 7);

        publisher.observe(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: "https://allowed.example".to_owned(),
            },
        );
        assert!(policy.decision(&chrome_app, Some(11)).is_allowed());
        publisher.observe(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: "https://global.example".to_owned(),
            },
        );
        assert!(!policy.decision(&chrome_app, Some(11)).is_allowed());
        publisher.observe(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: "https://text.example".to_owned(),
            },
        );
        assert!(!policy.decision(&chrome_app, Some(11)).is_allowed());
    }

    #[test]
    fn input_requires_a_tracked_known_text_field() {
        let filter = FilterConfig::default();
        let (_, chrome) = chrome_eligibility_channel(filter.clone());
        let policy = TextContentPolicy::new(chrome, filter);
        let app = app("Example", "dev.example.App", 7);

        assert!(
            policy
                .input_decision(&app, Some(11), Some(known_text_field()))
                .is_allowed()
        );
        assert!(!policy.input_decision(&app, Some(11), None).is_allowed());
        assert!(
            !policy
                .input_decision(
                    &app,
                    Some(11),
                    Some(FocusedField {
                        generation: 1,
                        class: FieldClass::Unknown,
                    }),
                )
                .is_allowed()
        );
    }

    #[test]
    fn replacement_updates_the_app_decision_without_recreating_the_policy() {
        let filter = FilterConfig::default();
        let (_, chrome) = chrome_eligibility_channel(filter.clone());
        let policy = TextContentPolicy::new(chrome, filter);
        let app = app("Example", "dev.example.App", 7);
        assert!(policy.decision(&app, Some(11)).is_allowed());

        policy.replace_filter(FilterConfig {
            text_content: ScopedFilterConfig {
                exclude_apps: vec!["dev.example.App".to_owned()],
                ..ScopedFilterConfig::default()
            },
            ..FilterConfig::default()
        });

        assert!(!policy.decision(&app, Some(11)).is_allowed());
    }
}
