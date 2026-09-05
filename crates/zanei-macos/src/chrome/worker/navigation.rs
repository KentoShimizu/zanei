//! Navigation deduplication independent from per-window eligibility state.

use zanei_core::schema::BrowserTransition;

use super::{ChromeSnapshot, SnapshotError, validate_snapshot};
use crate::chrome::BrowserPage;
use crate::ffi::applescript::AppleScriptWindowId;

#[derive(Default)]
pub(in crate::chrome) struct NavigationTracker {
    pub(in crate::chrome) previous: Option<ObservedPage>,
}

impl NavigationTracker {
    pub(in crate::chrome) fn observe(
        &mut self,
        snapshot: ChromeSnapshot,
    ) -> Result<Option<Navigation>, SnapshotError> {
        validate_snapshot(&snapshot)?;
        let Some(url) = snapshot.page.url() else {
            self.reset_page();
            return Ok(None);
        };
        let current = ObservedPage {
            window_id: snapshot.applescript_window_id.clone(),
            page: snapshot.page.clone(),
        };
        let transition = match self.previous.as_ref() {
            None => None,
            Some(previous) if previous.page.target() != current.page.target() => None,
            // Safari exposes no tab identity: URL/window changes cannot classify a tab transition.
            Some(previous)
                if matches!(current.page, BrowserPage::Safari { .. })
                    && (previous.window_id != current.window_id
                        || previous.page.url() != Some(url)) =>
            {
                None
            }
            Some(previous)
                if matches!(current.page, BrowserPage::Chrome { .. })
                    && (previous.window_id != current.window_id
                        || previous.page.tab_key() != current.page.tab_key()) =>
            {
                Some(BrowserTransition::TabSwitch)
            }
            Some(previous) if previous.page.url() != Some(url) => Some(BrowserTransition::Navigate),
            Some(_) => {
                self.previous = Some(current);
                return Ok(None);
            }
        };
        self.previous = Some(current);
        Ok(Some(Navigation {
            url: url.to_owned(),
            snapshot,
            transition,
        }))
    }

    pub(in crate::chrome) fn reset_page(&mut self) {
        self.previous = None;
    }

    pub(in crate::chrome) fn clear(&mut self) {
        self.reset_page();
    }
}

pub(in crate::chrome) struct ObservedPage {
    window_id: AppleScriptWindowId,
    page: BrowserPage,
}

pub(in crate::chrome) struct Navigation {
    pub(in crate::chrome) url: String,
    pub(in crate::chrome) transition: Option<BrowserTransition>,
    pub(in crate::chrome) snapshot: ChromeSnapshot,
}
