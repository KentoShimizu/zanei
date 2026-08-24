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
    unexpected_exit_pending: bool,
}

pub(super) struct RestartTransition {
    pub(super) state: RestartState,
    pub(super) clear_degraded: bool,
}

impl RestartState {
    pub(super) const fn new() -> Self {
        Self {
            next_attempt: None,
            delay_index: 0,
            waiting_for_permission: false,
            unexpected_exit_pending: false,
        }
    }

    pub(super) fn start_failed(mut self, now: Instant, permissions_granted: bool) -> Self {
        self.schedule(now, permissions_granted);
        self
    }

    pub(super) fn exited_unexpectedly(mut self, now: Instant, permissions_granted: bool) -> Self {
        self.unexpected_exit_pending = true;
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
            || self
                .next_attempt
                .is_some_and(|next_attempt| now >= next_attempt)
    }

    pub(super) fn started(mut self) -> RestartTransition {
        self.next_attempt = None;
        self.waiting_for_permission = false;
        RestartTransition {
            state: self,
            clear_degraded: !self.unexpected_exit_pending,
        }
    }

    pub(super) fn stable(self) -> RestartTransition {
        RestartTransition {
            state: Self::new(),
            clear_degraded: self.unexpected_exit_pending,
        }
    }
}

pub(in crate::daemon) fn chrome_failure_reason(state: ChromeFailureState) -> Option<String> {
    state
        .current()
        .map(|failure| format!("state=unavailable {failure}"))
}
