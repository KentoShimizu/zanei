//! Worker-start scheduling and candidate observation time.

use std::time::Instant;

use time::OffsetDateTime;

use crate::{
    content_snapshot::{SnapshotTrigger, SnapshotTriggerKind, scheduler::SnapshotScheduler},
    focus_context::FocusContext,
};

#[derive(Clone, Copy)]
pub(super) struct CandidateTime {
    pub(super) monotonic: Instant,
    pub(super) wall: OffsetDateTime,
}

impl CandidateTime {
    pub(super) fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall: OffsetDateTime::now_utc(),
        }
    }
}

pub(in crate::content_snapshot) fn seed_scheduler_from_focus(
    scheduler: &mut SnapshotScheduler,
    focus_context: &FocusContext,
    observed_at: Instant,
) {
    let Some(focus) = focus_context.current() else {
        return;
    };
    let Some(window) = focus.window else {
        return;
    };
    scheduler.observe(SnapshotTrigger {
        app: focus.app,
        window,
        kind: SnapshotTriggerKind::Focus,
        observed_at,
    });
}
