//! Cross-collector authorization for text-bearing event fields.

use crate::{chrome::ChromeEligibilityTracker, focused_field::FocusedField};

const CHROME_BUNDLE_ID: &str = "com.google.Chrome";

#[derive(Clone)]
pub struct TextContentPolicy {
    chrome: ChromeEligibilityTracker,
}

impl TextContentPolicy {
    pub const fn new(chrome: ChromeEligibilityTracker) -> Self {
        Self { chrome }
    }

    pub(crate) fn allows_window(
        &self,
        bundle_id: Option<&str>,
        pid: i64,
        window_id: Option<i64>,
    ) -> bool {
        bundle_id != Some(CHROME_BUNDLE_ID) || self.chrome.allows_text(pid, window_id)
    }

    pub(crate) fn allows_input(
        &self,
        bundle_id: Option<&str>,
        pid: i64,
        window_id: Option<i64>,
        focused_field: Option<FocusedField>,
    ) -> bool {
        focused_field.is_some_and(|field| field.class.is_known_text())
            && self.allows_window(bundle_id, pid, window_id)
    }
}

#[cfg(test)]
mod tests {
    use zanei_core::config::FilterConfig;
    use zanei_core::schema::FieldKind;

    use super::*;
    use crate::chrome::chrome_eligibility_channel;
    use crate::focused_field::{FieldClass, FocusedField};

    #[test]
    fn chrome_requires_normal_exact_window_but_other_apps_are_unaffected() {
        let (publisher, tracker) = chrome_eligibility_channel(FilterConfig::default());
        let policy = TextContentPolicy::new(tracker);

        assert!(!policy.allows_window(Some(CHROME_BUNDLE_ID), 7, Some(11)));
        assert!(policy.allows_window(Some("dev.example.App"), 7, Some(11)));

        publisher.publish_incognito(7, Some(11));
        assert!(!policy.allows_window(Some(CHROME_BUNDLE_ID), 7, Some(11)));

        publisher.publish_normal(7, Some(11), "https://example.com");
        assert!(policy.allows_window(Some(CHROME_BUNDLE_ID), 7, Some(11)));
        assert!(!policy.allows_window(Some(CHROME_BUNDLE_ID), 7, Some(12)));
    }

    #[test]
    fn input_requires_a_tracked_known_text_field() {
        let (_, tracker) = chrome_eligibility_channel(FilterConfig::default());
        let policy = TextContentPolicy::new(tracker);
        let field = FocusedField {
            generation: 1,
            class: FieldClass::KnownText(FieldKind::Text),
        };

        assert!(policy.allows_input(Some("dev.example.App"), 7, Some(11), Some(field)));
        assert!(!policy.allows_input(Some("dev.example.App"), 7, Some(11), None));
        assert!(!policy.allows_input(
            Some("dev.example.App"),
            7,
            Some(11),
            Some(FocusedField {
                generation: 1,
                class: FieldClass::Unknown,
            }),
        ));
    }
}
