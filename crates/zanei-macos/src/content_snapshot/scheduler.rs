//! Pure settle, refresh, and focus-out scheduling state machine.
//!
//! Focus and title changes start a settle and reset the refresh backoff. Input in
//! the current window re-arms its settle and postpones the next refresh by the
//! current backoff interval without resetting it, so input never makes refreshes
//! more frequent than the settles it triggers itself.
//!
//! A candidate held back only by the global save interval is retried once when the
//! interval ends. A settle or refresh retry belongs to the current window, so newer
//! input or a change to that window, or a window change, discards it. A focus-out
//! retry is discarded by any newer trigger for its window.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use zanei_core::schema::ContentSnapshotTrigger;

use super::{
    budget::GLOBAL_SAVE_INTERVAL,
    state::SnapshotWindowKey,
    trigger::{SnapshotTrigger, SnapshotTriggerKind, SnapshotTriggerMessage},
};

// Design limit: settle after two seconds without a focus/title change or input.
pub(crate) const SETTLE_QUIET_INTERVAL: Duration = Duration::from_secs(2);
// Design limit: force settle no later than ten seconds after the first change.
pub(crate) const SETTLE_MAX_INTERVAL: Duration = Duration::from_secs(10);
// Design limit: focus-out within three seconds of a saved snapshot is omitted.
pub(crate) const FOCUS_OUT_MIN_INTERVAL: Duration = Duration::from_secs(3);
// Design limit: refresh backs off through 30, 60, 120, 240, then 600 seconds.
pub(crate) const REFRESH_INTERVALS: [Duration; 5] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
    Duration::from_secs(600),
];

#[derive(Clone, Debug)]
pub(crate) struct ScheduledSnapshot {
    pub(crate) target: SnapshotTrigger,
    pub(crate) trigger: ContentSnapshotTrigger,
    pub(crate) activity_window: Option<Duration>,
    /// Already retried once after the global save interval held it back.
    pub(crate) deferred: bool,
}

impl ScheduledSnapshot {
    pub(crate) fn key(&self) -> Option<SnapshotWindowKey> {
        self.target.window.id.map(|window_id| SnapshotWindowKey {
            pid: self.target.app.pid,
            window_id,
        })
    }
}

#[derive(Clone, Debug)]
struct CurrentWindow {
    target: SnapshotTrigger,
    first_change: Instant,
    last_change: Instant,
    settle_due: Option<Instant>,
    refresh_due: Instant,
    refresh_index: usize,
    /// A settle or refresh held back by the global save interval, due when it ends.
    retry: Option<(Instant, ScheduledSnapshot)>,
    /// Taken from the focus at worker start rather than observed as a change, so
    /// the first observed focus or title change replaces it.
    seeded: bool,
}

impl CurrentWindow {
    /// Rate-limited input can arrive after a later title or focus change with its
    /// own earlier time, so the last change only moves forward.
    fn arm_settle(&mut self, changed_at: Instant) {
        if self.settle_due.is_none() {
            self.first_change = changed_at;
        }
        self.retry = None;
        self.last_change = self.last_change.max(changed_at);
        self.settle_due = Some(
            (self.last_change + SETTLE_QUIET_INTERVAL).min(self.first_change + SETTLE_MAX_INTERVAL),
        );
    }
}

#[derive(Clone, Debug)]
struct FocusOut {
    target: SnapshotTrigger,
    due: Instant,
    deferred: bool,
}

pub(crate) struct SnapshotScheduler {
    current: Option<CurrentWindow>,
    focus_out: VecDeque<FocusOut>,
    active: bool,
}

impl Default for SnapshotScheduler {
    fn default() -> Self {
        Self {
            current: None,
            focus_out: VecDeque::new(),
            active: true,
        }
    }
}

impl SnapshotScheduler {
    pub(crate) fn observe_message(&mut self, message: SnapshotTriggerMessage) {
        match message {
            SnapshotTriggerMessage::Trigger(trigger) => self.observe(trigger),
            SnapshotTriggerMessage::FocusTransition {
                transition,
                observed_at,
            } => self.observe_focus_transition(transition, observed_at),
            SnapshotTriggerMessage::Input {
                window,
                observed_at,
            } => self.observe_input(window, observed_at),
        }
    }

    /// Input arms nothing on its own: window transitions belong to the focus path,
    /// so input for any window other than the current one is ignored.
    fn observe_input(&mut self, window: SnapshotWindowKey, observed_at: Instant) {
        if !self.active {
            return;
        }
        let Some(current) = self.current.as_mut().filter(|current| {
            current.target.app.pid == window.pid
                && current.target.window.id == Some(window.window_id)
        }) else {
            return;
        };
        current.seeded = false;
        current.arm_settle(observed_at);
        current.refresh_due = current
            .refresh_due
            .max(current.last_change + REFRESH_INTERVALS[current.refresh_index]);
    }

    pub(crate) fn observe(&mut self, trigger: SnapshotTrigger) {
        if !self.active || trigger.window.id.is_none() {
            return;
        }
        // Any newer trigger for a window supersedes its pending focus-out retry.
        self.focus_out.retain(|candidate| {
            !candidate.deferred
                || candidate.target.app.pid != trigger.app.pid
                || candidate.target.window.id != trigger.window.id
        });
        if trigger.kind == SnapshotTriggerKind::FocusOut {
            if self.current.as_ref().is_some_and(|current| {
                current.target.app.pid == trigger.app.pid
                    && current.target.window.id == trigger.window.id
            }) {
                self.current = None;
            }
            self.push_focus_out(FocusOut {
                due: trigger.observed_at,
                target: trigger,
                deferred: false,
            });
            return;
        }
        let same_window = self.current.as_ref().is_some_and(|current| {
            !current.seeded
                && current.target.app.pid == trigger.app.pid
                && current.target.window.id == trigger.window.id
        });
        if same_window {
            self.update_current(trigger);
            return;
        }

        self.current = Some(Self::new_current(trigger));
    }

    /// Starts from the window focused when the worker starts.
    pub(crate) fn seed(&mut self, trigger: SnapshotTrigger) {
        self.observe(trigger);
        if let Some(current) = self.current.as_mut() {
            current.seeded = true;
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if !self.active {
            return None;
        }
        let focus_out = self.focus_out.front().map(|candidate| candidate.due);
        let retry = self
            .current
            .as_ref()
            .and_then(|current| current.retry.as_ref().map(|(due, _)| *due));
        let settle = self.current.as_ref().and_then(|current| current.settle_due);
        let refresh = self.current.as_ref().map(|current| current.refresh_due);
        [focus_out, retry, settle, refresh]
            .into_iter()
            .flatten()
            .min()
    }

    pub(crate) fn take_due(&mut self, now: Instant) -> Option<ScheduledSnapshot> {
        if !self.active {
            return None;
        }
        if self
            .focus_out
            .front()
            .is_some_and(|candidate| candidate.due <= now)
        {
            let candidate = self.focus_out.pop_front()?;
            return Some(ScheduledSnapshot {
                target: candidate.target,
                trigger: ContentSnapshotTrigger::FocusOut,
                activity_window: None,
                deferred: candidate.deferred,
            });
        }
        let current = self.current.as_mut()?;
        if current.retry.as_ref().is_some_and(|(due, _)| *due <= now) {
            return current.retry.take().map(|(_, candidate)| candidate);
        }
        if current.settle_due.is_some_and(|deadline| deadline <= now) {
            current.settle_due = None;
            current.retry = None;
            return Some(ScheduledSnapshot {
                target: current.target.clone(),
                trigger: ContentSnapshotTrigger::Settle,
                activity_window: None,
                deferred: false,
            });
        }
        if current.refresh_due <= now {
            let interval = REFRESH_INTERVALS[current.refresh_index];
            current.refresh_index = (current.refresh_index + 1).min(REFRESH_INTERVALS.len() - 1);
            current.refresh_due = now + REFRESH_INTERVALS[current.refresh_index];
            current.retry = None;
            return Some(ScheduledSnapshot {
                target: current.target.clone(),
                trigger: ContentSnapshotTrigger::Refresh,
                activity_window: Some(interval),
                deferred: false,
            });
        }
        None
    }

    /// Retries a candidate held back only by the global save interval once, at `due`.
    /// Returns false when the candidate is dropped instead: it was already retried, or
    /// its window is no longer the current one.
    pub(crate) fn defer(&mut self, candidate: &ScheduledSnapshot, due: Instant) -> bool {
        if !self.active || candidate.deferred {
            return false;
        }
        if candidate.trigger == ContentSnapshotTrigger::FocusOut {
            self.push_focus_out(FocusOut {
                target: candidate.target.clone(),
                due,
                deferred: true,
            });
            return true;
        }
        let Some(current) = self.current.as_mut().filter(|current| {
            current.target.app.pid == candidate.target.app.pid
                && current.target.window.id == candidate.target.window.id
        }) else {
            return false;
        };
        current.retry = Some((
            due,
            ScheduledSnapshot {
                deferred: true,
                ..candidate.clone()
            },
        ));
        true
    }

    /// Keeps the queue ordered by due time, since a retry is due later than new focus-outs.
    fn push_focus_out(&mut self, candidate: FocusOut) {
        let index = self
            .focus_out
            .partition_point(|queued| queued.due <= candidate.due);
        self.focus_out.insert(index, candidate);
    }

    pub(crate) fn terminate_pid(&mut self, pid: i64) {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.target.app.pid == pid)
        {
            self.current = None;
        }
        self.focus_out
            .retain(|candidate| candidate.target.app.pid != pid);
    }

    pub(crate) fn did_wake(&mut self) {
        self.clear();
    }

    pub(crate) fn replace_filter(&mut self, now: Instant) -> Option<i64> {
        let current = self.current.as_mut()?;
        current.first_change = now;
        current.last_change = now;
        current.settle_due = Some(now + SETTLE_QUIET_INTERVAL);
        current.refresh_index = 0;
        current.refresh_due = now + REFRESH_INTERVALS[0];
        current.retry = None;
        Some(current.target.app.pid)
    }

    pub(crate) fn pause(&mut self) {
        self.active = false;
        self.clear();
    }

    pub(crate) fn stop(&mut self) {
        self.pause();
    }

    pub(crate) fn focus_out_allows(last_saved_at: Option<Instant>, now: Instant) -> bool {
        last_saved_at.is_none_or(|saved_at| {
            now.checked_duration_since(saved_at)
                .is_some_and(|elapsed| elapsed >= FOCUS_OUT_MIN_INTERVAL)
        })
    }

    pub(crate) fn global_interval_allows(last_saved_at: Option<Instant>, now: Instant) -> bool {
        last_saved_at.is_none_or(|saved_at| {
            now.checked_duration_since(saved_at)
                .is_some_and(|elapsed| elapsed >= GLOBAL_SAVE_INTERVAL)
        })
    }

    fn new_current(trigger: SnapshotTrigger) -> CurrentWindow {
        let observed_at = trigger.observed_at;
        CurrentWindow {
            target: trigger,
            first_change: observed_at,
            last_change: observed_at,
            settle_due: Some(observed_at + SETTLE_QUIET_INTERVAL),
            refresh_due: observed_at + REFRESH_INTERVALS[0],
            refresh_index: 0,
            retry: None,
            seeded: false,
        }
    }

    fn observe_focus_transition(
        &mut self,
        transition: crate::focus_context::FocusTransition,
        observed_at: Instant,
    ) {
        let same_window = matches!(
            (&transition.previous, &transition.current),
            (Some(previous), Some(current))
                if previous.app.pid == current.app.pid
                    && previous.window.as_ref().and_then(|window| window.id)
                        == current.window.as_ref().and_then(|window| window.id)
        );
        if !same_window
            && let Some(previous) = transition.previous
            && let Some(window) = previous.window
        {
            self.observe(SnapshotTrigger {
                app: previous.app,
                window,
                kind: SnapshotTriggerKind::FocusOut,
                observed_at,
            });
        }
        if let Some(current) = transition.current
            && let Some(window) = current.window
        {
            self.observe(SnapshotTrigger {
                app: current.app,
                window,
                kind: if same_window && !transition.resynced {
                    SnapshotTriggerKind::Title
                } else {
                    SnapshotTriggerKind::Focus
                },
                observed_at,
            });
        }
    }

    fn update_current(&mut self, trigger: SnapshotTrigger) {
        let Some(current) = self.current.as_mut() else {
            return;
        };
        let observed_at = trigger.observed_at;
        current.target = trigger;
        current.arm_settle(observed_at);
        current.refresh_index = 0;
        current.refresh_due = current.last_change + REFRESH_INTERVALS[0];
    }

    fn clear(&mut self) {
        self.current = None;
        self.focus_out.clear();
    }
}
