//! Input-driven focused-field pulls.
//!
//! Fields that post no focus or value notifications are found by re-reading the focused element
//! shortly after user input. Nothing is read while the user is idle: every pull is armed by a
//! key-down, a click, or a focused-window change, and running a pull disarms it.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use time::OffsetDateTime;

use super::NativeAx;
use crate::{
    ffi::ax::NativeAxObservation,
    text_capture::{InputActivity, InputActivityKind},
};

/// Lets the application and the input method apply an edit before the field is read.
const PULL_QUIET: Duration = Duration::from_millis(150);
/// Continuous input is still read at least this often after the first unread trigger.
const PULL_MAX_HOLD: Duration = Duration::from_millis(500);
/// One follow-up read after Return/Enter observes the application's post-send clear or an input
/// method commit that the immediate read can race.
const SUBMIT_SETTLE: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PullTrigger {
    Edit,
    Submit,
    Click,
    Window,
}

impl PullTrigger {
    const fn name(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Submit => "submit",
            Self::Click => "click",
            Self::Window => "window",
        }
    }
}

// Design limit: one discovery read per debounced burst per pid; add per-pid backoff if traces
// show repeated AX timeouts on pulls.
#[derive(Default)]
pub(super) struct PullScheduler {
    pids: HashMap<i32, PidPulls>,
}

#[derive(Default)]
struct PidPulls {
    burst: Option<Burst>,
    submit: Option<Instant>,
    settle: Option<Instant>,
}

struct Burst {
    first: Instant,
    last: Instant,
    trigger: PullTrigger,
}

impl Burst {
    fn due(&self) -> Instant {
        (self.last + PULL_QUIET).min(self.first + PULL_MAX_HOLD)
    }
}

impl PidPulls {
    fn is_empty(&self) -> bool {
        self.burst.is_none() && self.submit.is_none() && self.settle.is_none()
    }

    /// Clears every armed read that is due and names the trigger that made the pid due.
    fn take_due(&mut self, now: Instant) -> Option<&'static str> {
        let submit = self.submit.take_if(|at| *at <= now).map(|_| "submit");
        let settle = self.settle.take_if(|at| *at <= now).map(|_| "settle");
        let burst = self
            .burst
            .take_if(|burst| burst.due() <= now)
            .map(|burst| burst.trigger.name());
        submit.or(settle).or(burst)
    }
}

impl PullScheduler {
    pub(super) fn arm(&mut self, pid: i32, trigger: PullTrigger, at: Instant) {
        let pulls = self.pids.entry(pid).or_default();
        match trigger {
            PullTrigger::Submit => {
                // The immediate read also covers the edits that preceded Return/Enter.
                pulls.burst = None;
                pulls.submit = Some(at);
                pulls.settle = Some(at + SUBMIT_SETTLE);
            }
            PullTrigger::Edit | PullTrigger::Click | PullTrigger::Window => {
                let burst = pulls.burst.get_or_insert(Burst {
                    first: at,
                    last: at,
                    trigger,
                });
                burst.first = burst.first.min(at);
                if at >= burst.last {
                    burst.last = at;
                    burst.trigger = trigger;
                }
            }
        }
    }

    pub(super) fn take_due(&mut self, now: Instant) -> Vec<(i32, &'static str)> {
        let due = self
            .pids
            .iter_mut()
            .filter_map(|(pid, pulls)| pulls.take_due(now).map(|trigger| (*pid, trigger)))
            .collect();
        self.pids.retain(|_, pulls| !pulls.is_empty());
        due
    }

    pub(super) fn remove(&mut self, pid: i32) {
        self.pids.remove(&pid);
    }
}

impl NativeAx {
    /// Arms pulls for key input reported by the EventTap. Input for an application without an
    /// observer is dropped.
    pub(super) fn arm_input_pulls(&mut self) {
        for InputActivity { pid, kind, at } in self.authorizations.take_activity() {
            let trigger = match kind {
                InputActivityKind::Edit => PullTrigger::Edit,
                InputActivityKind::Submit => PullTrigger::Submit,
            };
            self.arm_pull(pid, trigger, at);
        }
    }

    pub(super) fn arm_pull(&mut self, pid: i32, trigger: PullTrigger, at: Instant) {
        if self.observers.contains_key(&pid) {
            self.pulls.arm(pid, trigger, at);
        }
    }

    pub(super) fn run_due_pulls(
        &mut self,
        now: Instant,
        secure_input: bool,
    ) -> Vec<NativeAxObservation> {
        let mut observations = Vec::new();
        for (pid, trigger) in self.pulls.take_due(now) {
            if let Some(observer) = self.observers.get_mut(&pid) {
                observations.extend(observer.pull_focused_field(
                    trigger,
                    now,
                    OffsetDateTime::now_utc(),
                    secure_input,
                    &mut self.authorizations,
                ));
            }
        }
        observations
    }
}

#[cfg(test)]
mod tests;
