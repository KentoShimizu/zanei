//! Navigation deduplication independent from per-window eligibility state.

use zanei_core::schema::BrowserTransition;

use super::{ChromeSnapshot, SnapshotError, validate_snapshot};
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
        let current = ObservedPage {
            window_id: snapshot.applescript_window_id.clone(),
            tab_key: snapshot.tab_key.clone(),
            url: snapshot.url.clone(),
        };
        let transition = match self.previous.as_ref() {
            None => None,
            Some(previous)
                if previous.window_id != current.window_id
                    || previous.tab_key != current.tab_key =>
            {
                Some(BrowserTransition::TabSwitch)
            }
            Some(previous) if previous.url != current.url => Some(BrowserTransition::Navigate),
            Some(_) => {
                self.previous = Some(current);
                return Ok(None);
            }
        };
        self.previous = Some(current);
        Ok(Some(Navigation {
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
    tab_key: String,
    url: String,
}

pub(in crate::chrome) struct Navigation {
    pub(in crate::chrome) transition: Option<BrowserTransition>,
    pub(in crate::chrome) snapshot: ChromeSnapshot,
}
