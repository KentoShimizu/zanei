//! FocusContext guard for attributing front-window AppleScript results.

use zanei_core::privacy::CHROME_BUNDLE_ID;

use crate::focus_context::{FocusContext, FocusSnapshot};

use super::ChromeQuery;

pub(super) struct FrontWindowAttribution {
    requested: Option<(i64, Option<i64>)>,
    generation_before: Option<u64>,
}

impl FrontWindowAttribution {
    pub(super) fn capture(query: ChromeQuery, focus_context: &FocusContext) -> Self {
        let requested = match query {
            ChromeQuery::FrontWindow { pid, window_id } => Some((pid, window_id)),
            ChromeQuery::Window { .. } => None,
        };
        let generation_before = requested.and_then(|(pid, window_id)| {
            focus_context
                .current()
                .filter(|focus| focus_matches(focus, pid, window_id))
                .map(|focus| focus.generation)
        });
        Self {
            requested,
            generation_before,
        }
    }

    pub(super) fn allows(&self, focus_context: &FocusContext) -> bool {
        let Some((pid, window_id)) = self.requested else {
            return true;
        };
        let Some(generation_before) = self.generation_before else {
            return false;
        };
        focus_context.current().is_some_and(|focus| {
            focus.generation == generation_before && focus_matches(&focus, pid, window_id)
        })
    }
}

fn focus_matches(focus: &FocusSnapshot, pid: i64, window_id: Option<i64>) -> bool {
    focus.app.bundle_id.as_deref() == Some(CHROME_BUNDLE_ID)
        && focus.app.pid == pid
        && focus.window.as_ref().and_then(|window| window.id) == window_id
}
