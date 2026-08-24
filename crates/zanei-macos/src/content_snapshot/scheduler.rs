//! Pure settle, refresh, and focus-out scheduling state machine.

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

// Design limit: settle after two seconds without a focus/title change.
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
}

#[derive(Clone, Debug)]
struct FocusOut {
    target: SnapshotTrigger,
    due: Instant,
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
        }
    }

    pub(crate) fn observe(&mut self, trigger: SnapshotTrigger) {
        if !self.active || trigger.window.id.is_none() {
            return;
        }
        if trigger.kind == SnapshotTriggerKind::FocusOut {
            if self.current.as_ref().is_some_and(|current| {
                current.target.app.pid == trigger.app.pid
                    && current.target.window.id == trigger.window.id
            }) {
                self.current = None;
            }
            self.focus_out.push_back(FocusOut {
                due: trigger.observed_at,
                target: trigger,
            });
            return;
        }
        let same_window = self.current.as_ref().is_some_and(|current| {
            current.target.app.pid == trigger.app.pid
                && current.target.window.id == trigger.window.id
        });
        if same_window {
            self.update_current(trigger);
            return;
        }

        self.current = Some(Self::new_current(trigger));
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if !self.active {
            return None;
        }
        let focus_out = self.focus_out.front().map(|candidate| candidate.due);
        let settle = self.current.as_ref().and_then(|current| current.settle_due);
        let refresh = self.current.as_ref().map(|current| current.refresh_due);
        [focus_out, settle, refresh].into_iter().flatten().min()
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
            });
        }
        let current = self.current.as_mut()?;
        if current.settle_due.is_some_and(|deadline| deadline <= now) {
            current.settle_due = None;
            return Some(ScheduledSnapshot {
                target: current.target.clone(),
                trigger: ContentSnapshotTrigger::Settle,
                activity_window: None,
            });
        }
        if current.refresh_due <= now {
            let interval = REFRESH_INTERVALS[current.refresh_index];
            current.refresh_index = (current.refresh_index + 1).min(REFRESH_INTERVALS.len() - 1);
            current.refresh_due = now + REFRESH_INTERVALS[current.refresh_index];
            return Some(ScheduledSnapshot {
                target: current.target.clone(),
                trigger: ContentSnapshotTrigger::Refresh,
                activity_window: Some(interval),
            });
        }
        None
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
        let starts_new_settle_burst = current.settle_due.is_none();
        current.target = trigger;
        current.last_change = current.target.observed_at;
        if starts_new_settle_burst {
            current.first_change = current.last_change;
        }
        current.settle_due = Some(
            (current.last_change + SETTLE_QUIET_INTERVAL)
                .min(current.first_change + SETTLE_MAX_INTERVAL),
        );
        current.refresh_index = 0;
        current.refresh_due = current.last_change + REFRESH_INTERVALS[0];
    }

    fn clear(&mut self) {
        self.current = None;
        self.focus_out.clear();
    }
}
