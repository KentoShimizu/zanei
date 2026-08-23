//! Snapshot reservation state, daily accounting, and per-process backoff.

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    time::{Duration, Instant},
};

use super::budget::{
    DAILY_BUDGET_WINDOW, DAILY_TEXT_BUDGET_BYTES, PID_BACKOFF_INITIAL, PID_BACKOFF_MAX,
};
use super::scheduler::SnapshotScheduler;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SnapshotWindowKey {
    pub(crate) pid: i64,
    pub(crate) window_id: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SaveBlock {
    Duplicate,
    GlobalInterval,
    DailyBudget,
}

#[derive(Clone, Copy, Debug)]
struct SavedWindow {
    hash: u64,
    saved_at: Instant,
}

#[derive(Clone, Copy, Debug)]
struct PidBackoff {
    consecutive_timeouts: u32,
    retry_at: Instant,
}

pub(crate) struct SnapshotState {
    windows: HashMap<SnapshotWindowKey, SavedWindow>,
    global_saved_at: Option<Instant>,
    daily_started_at: Instant,
    daily_bytes: u64,
    daily_exhausted: bool,
    backoffs: HashMap<i64, PidBackoff>,
}

impl SnapshotState {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            windows: HashMap::new(),
            global_saved_at: None,
            daily_started_at: now,
            daily_bytes: 0,
            daily_exhausted: false,
            backoffs: HashMap::new(),
        }
    }

    pub(crate) fn last_saved_at(&self, key: SnapshotWindowKey) -> Option<Instant> {
        self.windows.get(&key).map(|saved| saved.saved_at)
    }

    pub(crate) fn global_interval_allows(&self, now: Instant) -> bool {
        SnapshotScheduler::global_interval_allows(self.global_saved_at, now)
    }

    pub(crate) fn daily_budget_allows(&mut self, now: Instant) -> bool {
        self.roll_daily_window(now);
        !self.daily_exhausted && self.daily_bytes < DAILY_TEXT_BUDGET_BYTES
    }

    pub(crate) fn backoff_allows(&self, pid: i64, now: Instant) -> bool {
        self.backoffs
            .get(&pid)
            .is_none_or(|backoff| now >= backoff.retry_at)
    }

    pub(crate) fn backoff_remaining(&self, now: Instant) -> Option<Duration> {
        self.backoffs
            .values()
            .filter_map(|backoff| backoff.retry_at.checked_duration_since(now))
            .max()
    }

    pub(crate) fn evaluate_save(
        &mut self,
        key: SnapshotWindowKey,
        hash: u64,
        bytes: usize,
        now: Instant,
    ) -> Result<(), SaveBlock> {
        self.roll_daily_window(now);
        if self
            .windows
            .get(&key)
            .is_some_and(|saved| saved.hash == hash)
        {
            return Err(SaveBlock::Duplicate);
        }
        if !self.global_interval_allows(now) {
            return Err(SaveBlock::GlobalInterval);
        }
        let Ok(bytes) = u64::try_from(bytes) else {
            return Err(SaveBlock::DailyBudget);
        };
        if self.daily_bytes.saturating_add(bytes) > DAILY_TEXT_BUDGET_BYTES {
            self.daily_exhausted = true;
            return Err(SaveBlock::DailyBudget);
        }
        Ok(())
    }

    pub(crate) fn commit_save(
        &mut self,
        key: SnapshotWindowKey,
        hash: u64,
        bytes: usize,
        now: Instant,
    ) {
        let bytes =
            u64::try_from(bytes).expect("the 32 KiB snapshot design limit always fits in u64");
        self.windows.insert(
            key,
            SavedWindow {
                hash,
                saved_at: now,
            },
        );
        self.global_saved_at = Some(now);
        self.daily_bytes = self.daily_bytes.saturating_add(bytes);
        self.backoffs.remove(&key.pid);
    }

    pub(crate) fn record_failure(&mut self, pid: i64, now: Instant, timed_out: bool) {
        let consecutive_timeouts = if timed_out {
            self.backoffs
                .get(&pid)
                .map_or(1, |backoff| backoff.consecutive_timeouts.saturating_add(1))
        } else {
            0
        };
        let shift = consecutive_timeouts.saturating_sub(1).min(31);
        let multiplier = 1_u32.checked_shl(shift).unwrap_or(u32::MAX);
        let delay = PID_BACKOFF_INITIAL
            .checked_mul(multiplier)
            .unwrap_or(PID_BACKOFF_MAX)
            .min(PID_BACKOFF_MAX);
        self.backoffs.insert(
            pid,
            PidBackoff {
                consecutive_timeouts,
                retry_at: now + delay,
            },
        );
    }

    pub(crate) fn record_scan_success(&mut self, pid: i64) {
        self.backoffs.remove(&pid);
    }

    pub(crate) fn terminate_pid(&mut self, pid: i64) {
        self.windows.retain(|key, _| key.pid != pid);
        self.backoffs.remove(&pid);
    }

    pub(crate) fn text_hash(text: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        hasher.finish()
    }

    #[cfg(test)]
    pub(crate) fn daily_bytes(&mut self, now: Instant) -> u64 {
        self.roll_daily_window(now);
        self.daily_bytes
    }

    fn roll_daily_window(&mut self, now: Instant) {
        if now
            .checked_duration_since(self.daily_started_at)
            .is_some_and(|elapsed| elapsed >= DAILY_BUDGET_WINDOW)
        {
            self.daily_started_at = now;
            self.daily_bytes = 0;
            self.daily_exhausted = false;
        }
    }
}
