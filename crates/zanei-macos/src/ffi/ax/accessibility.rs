use std::sync::{Arc, RwLock};

use zanei_core::{
    config::FilterConfig,
    privacy::{PrivacyScope, app_is_allowed_for},
    schema::App,
};

use super::{cf::CfRef, element::set_boolean_attribute};

const MANUAL_ACCESSIBILITY: &str = "AXManualAccessibility";

#[derive(Clone)]
pub(crate) struct ManualAccessibilityPolicy {
    capture_text_content: bool,
    capture_content_snapshot: bool,
    filter: Arc<RwLock<FilterConfig>>,
}

impl ManualAccessibilityPolicy {
    pub(crate) fn new(
        capture_text_content: bool,
        capture_content_snapshot: bool,
        filter: FilterConfig,
    ) -> Self {
        Self {
            capture_text_content,
            capture_content_snapshot,
            filter: Arc::new(RwLock::new(filter)),
        }
    }

    pub(crate) fn replace_filter(&self, filter: FilterConfig) {
        match self.filter.write() {
            Ok(mut current) => *current = filter,
            Err(_) => crate::trace::trace!(
                "component=ax phase=manual_accessibility action=replace_filter result=poisoned"
            ),
        }
    }

    pub(crate) fn allows(&self, app: &App) -> bool {
        self.filter.read().is_ok_and(|filter| {
            self.capture_text_content && app_is_allowed_for(PrivacyScope::TextContent, app, &filter)
                || self.capture_content_snapshot
                    && app_is_allowed_for(PrivacyScope::ContentSnapshot, app, &filter)
        })
    }
}

pub(super) fn set_manual_accessibility(
    application: CfRef,
    pid: i32,
    enabled_for_app: bool,
    enabled: bool,
) {
    let Some(enabled) = manual_accessibility_setting(enabled_for_app, enabled) else {
        return;
    };
    // AXEnhancedUserInterface is intentionally avoided because it can cause window-resize jank.
    if let Err(error) = set_boolean_attribute(application, MANUAL_ACCESSIBILITY, enabled)
        && enabled
        && !error.is_attribute_unsupported()
    {
        crate::trace::trace!(
            "component=ax phase=attach action=manual_accessibility pid={} operation={} code={}",
            pid,
            error.operation(),
            error.code()
        );
    }
}

const fn manual_accessibility_setting(enabled_for_app: bool, enabled: bool) -> Option<bool> {
    if enabled_for_app { Some(enabled) } else { None }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zanei_core::{
        config::{FilterConfig, ScopedFilterConfig},
        schema::App,
    };

    use super::{ManualAccessibilityPolicy, manual_accessibility_setting};

    fn app(bundle_id: &str) -> App {
        App {
            name: "Example".to_owned(),
            bundle_id: Some(bundle_id.to_owned()),
            pid: Some(7),
        }
    }

    #[test]
    fn lifecycle_is_gated_by_text_capture() {
        assert_eq!(manual_accessibility_setting(false, true), None);
        assert_eq!(manual_accessibility_setting(false, false), None);
        assert_eq!(manual_accessibility_setting(true, true), Some(true));
        assert_eq!(manual_accessibility_setting(true, false), Some(false));
    }

    #[test]
    fn either_enabled_scope_can_request_manual_accessibility() {
        let filter = FilterConfig {
            text_content: ScopedFilterConfig {
                include_only_apps: vec!["dev.example.Text".to_owned()],
                ..ScopedFilterConfig::default()
            },
            content_snapshot: ScopedFilterConfig {
                include_only_apps: vec!["dev.example.Snapshot".to_owned()],
                ..ScopedFilterConfig::default()
            },
            ..FilterConfig::default()
        };
        let policy = ManualAccessibilityPolicy::new(true, true, filter);

        assert!(policy.allows(&app("dev.example.Text")));
        assert!(policy.allows(&app("dev.example.Snapshot")));
        assert!(!policy.allows(&app("dev.example.Other")));
    }

    #[test]
    fn disabled_opt_ins_and_poisoned_filter_fail_closed() {
        let policy = ManualAccessibilityPolicy::new(false, false, FilterConfig::default());
        assert!(!policy.allows(&app("dev.example.App")));

        let poisoned = policy.clone();
        let filter = Arc::clone(&poisoned.filter);
        let _ = std::thread::spawn(move || {
            let _guard = filter.write().expect("manual accessibility filter lock");
            panic!("poison filter state");
        })
        .join();
        assert!(!poisoned.allows(&app("dev.example.App")));
    }

    #[test]
    fn replacement_changes_the_decision_used_by_the_next_attach() {
        let policy = ManualAccessibilityPolicy::new(true, false, FilterConfig::default());
        let target = app("dev.example.App");
        assert!(policy.allows(&target));

        policy.replace_filter(FilterConfig {
            text_content: ScopedFilterConfig {
                exclude_apps: vec!["dev.example.App".to_owned()],
                ..ScopedFilterConfig::default()
            },
            ..FilterConfig::default()
        });

        assert!(!policy.allows(&target));
    }
}
