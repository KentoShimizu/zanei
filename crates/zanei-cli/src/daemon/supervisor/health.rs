use std::time::{Duration, Instant};

use zanei_macos::chrome::ChromeFailureState;

const RESTART_DELAYS: [Duration; 5] = [
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
    Duration::from_secs(40),
    Duration::from_secs(60),
];
pub(super) const RESTART_STABLE_AFTER: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug)]
pub(super) struct RestartState {
    next_attempt: Option<Instant>,
    delay_index: usize,
    waiting_for_permission: bool,
    unexpected_exit_reason: Option<&'static str>,
}

impl RestartState {
    pub(super) const fn new() -> Self {
        Self {
            next_attempt: None,
            delay_index: 0,
            waiting_for_permission: false,
            unexpected_exit_reason: None,
        }
    }

    pub(super) fn start_failed(mut self, now: Instant, permissions_granted: bool) -> Self {
        self.schedule(now, permissions_granted);
        self
    }

    pub(super) fn exited_unexpectedly(
        mut self,
        now: Instant,
        permissions_granted: bool,
        reason: &'static str,
    ) -> Self {
        self.unexpected_exit_reason = Some(reason);
        self.schedule(now, permissions_granted);
        self
    }

    fn schedule(&mut self, now: Instant, permissions_granted: bool) {
        if permissions_granted {
            let delay = RESTART_DELAYS[self.delay_index.min(RESTART_DELAYS.len() - 1)];
            self.delay_index = self.delay_index.saturating_add(1);
            self.next_attempt = Some(now + delay);
            self.waiting_for_permission = false;
        } else {
            self.next_attempt = None;
            self.waiting_for_permission = true;
        }
    }

    pub(super) fn ready(self, now: Instant, permissions_granted: bool) -> bool {
        (!self.waiting_for_permission && self.next_attempt.is_none())
            || (self.waiting_for_permission && permissions_granted)
            || (permissions_granted
                && self
                    .next_attempt
                    .is_some_and(|next_attempt| now >= next_attempt))
    }

    pub(super) fn started(mut self) -> Self {
        self.next_attempt = None;
        self.waiting_for_permission = false;
        self
    }

    pub(super) const fn stable(self) -> Self {
        Self::new()
    }

    pub(super) const fn degraded_reason(self) -> Option<&'static str> {
        self.unexpected_exit_reason
    }
}

#[derive(Clone, Copy)]
pub(super) struct ChromeHealth {
    running: bool,
    failure_state: ChromeFailureState,
}

impl ChromeHealth {
    pub(super) const fn new(running: bool, failure_state: ChromeFailureState) -> Self {
        Self {
            running,
            failure_state,
        }
    }

    pub(super) fn degraded_reason(self, lifecycle_reason: Option<&str>) -> Option<String> {
        if self.running {
            chrome_failure_reason(self.failure_state)
                .or_else(|| lifecycle_reason.map(str::to_owned))
        } else {
            lifecycle_reason.map(str::to_owned)
        }
    }
}

fn chrome_failure_reason(state: ChromeFailureState) -> Option<String> {
    state
        .current()
        .map(|failure| format!("state=unavailable {failure}"))
}
