//! Fixed resource limits for content snapshot collection.

use std::time::Duration;

// Design limit: one traversal may occupy the worker for at most 200 ms.
pub(crate) const WALK_WALL_TIME_LIMIT: Duration = Duration::from_millis(200);
// Design limit: one traversal may inspect at most 2,000 Accessibility nodes.
pub(crate) const WALK_NODE_LIMIT: usize = 2_000;
// Design limit: one snapshot body is at most 32 KiB of UTF-8.
pub(crate) const SNAPSHOT_TEXT_LIMIT_BYTES: usize = 32 * 1_024;
// Design limit: the AX process gets at most 100 ms for one messaging call.
pub(crate) const AX_CALL_TIMEOUT: Duration = Duration::from_millis(100);
// Design limit: successful snapshot bodies are capped at 128 MiB per 24 hours.
pub(crate) const DAILY_TEXT_BUDGET_BYTES: u64 = 128 * 1_024 * 1_024;
// Design limit: the in-memory daily accounting window lasts 24 hours.
pub(crate) const DAILY_BUDGET_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
// Design limit: all windows share a five-second minimum saved-event interval.
pub(crate) const GLOBAL_SAVE_INTERVAL: Duration = Duration::from_secs(5);
// Design limit: the first failed-process backoff lasts 30 seconds.
pub(crate) const PID_BACKOFF_INITIAL: Duration = Duration::from_secs(30);
// Design limit: process failure backoff never exceeds ten minutes.
pub(crate) const PID_BACKOFF_MAX: Duration = Duration::from_secs(10 * 60);
// Design limit: child arrays are materialized in bounded traversal-sized chunks.
pub(crate) const CHILDREN_BATCH_SIZE: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WalkBudget {
    pub(crate) wall_time: Duration,
    pub(crate) nodes: usize,
    pub(crate) text_bytes: usize,
}

impl WalkBudget {
    pub(crate) const DESIGN: Self = Self {
        wall_time: WALK_WALL_TIME_LIMIT,
        nodes: WALK_NODE_LIMIT,
        text_bytes: SNAPSHOT_TEXT_LIMIT_BYTES,
    };
}
