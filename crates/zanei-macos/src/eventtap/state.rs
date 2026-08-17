//! OS-independent EventTap lifecycle state machine.

use std::time::Duration;

pub(crate) const WATCHDOG_INTERVAL: Duration = Duration::from_secs(10);
pub(crate) const RETRY_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DisableReason {
    Timeout,
    UserInput,
    Watchdog,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecreateCause {
    Startup,
    Disabled(DisableReason),
    Wake,
    Retry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct MonotonicTime(Duration);

impl MonotonicTime {
    pub(crate) const fn from_duration(value: Duration) -> Self {
        Self(value)
    }

    fn after(self, delay: Duration) -> Self {
        Self(self.0.saturating_add(delay))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TapState {
    Enabled,
    Disabled(DisableReason),
    Recreating(RecreateCause),
    Degraded {
        cause: RecreateCause,
        retry_at: MonotonicTime,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Effect {
    Enable,
    Recreate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Input {
    Disabled(DisableReason),
    Watchdog {
        tap_enabled: bool,
        secure_input: bool,
        now: MonotonicTime,
    },
    Wake,
    EnableFinished {
        enabled: bool,
    },
    RecreateFinished {
        enabled: bool,
        now: MonotonicTime,
    },
    RetryDue(MonotonicTime),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Lifecycle {
    state: TapState,
    secure_input: bool,
}

impl Lifecycle {
    pub(crate) const fn new() -> (Self, Effect) {
        (
            Self {
                state: TapState::Recreating(RecreateCause::Startup),
                secure_input: false,
            },
            Effect::Recreate,
        )
    }

    pub(crate) const fn state(self) -> TapState {
        self.state
    }

    #[cfg(test)]
    pub(crate) const fn secure_input(self) -> bool {
        self.secure_input
    }

    pub(crate) fn transition(&mut self, input: Input) -> Option<Effect> {
        match input {
            Input::Disabled(reason) if matches!(self.state, TapState::Enabled) => {
                self.state = TapState::Disabled(reason);
                Some(Effect::Enable)
            }
            Input::Disabled(_) => None,
            Input::Watchdog {
                tap_enabled,
                secure_input,
                now,
            } => {
                self.secure_input = secure_input;
                if tap_enabled || !matches!(self.state, TapState::Enabled) {
                    self.retry_if_due(now)
                } else {
                    self.state = TapState::Disabled(DisableReason::Watchdog);
                    Some(Effect::Enable)
                }
            }
            Input::Wake => {
                self.state = TapState::Recreating(RecreateCause::Wake);
                Some(Effect::Recreate)
            }
            Input::EnableFinished { enabled } => {
                let TapState::Disabled(reason) = self.state else {
                    return None;
                };
                if enabled {
                    self.state = TapState::Enabled;
                    None
                } else {
                    self.state = TapState::Recreating(RecreateCause::Disabled(reason));
                    Some(Effect::Recreate)
                }
            }
            Input::RecreateFinished { enabled, now } => {
                let TapState::Recreating(cause) = self.state else {
                    return None;
                };
                self.state = if enabled {
                    TapState::Enabled
                } else {
                    TapState::Degraded {
                        cause,
                        retry_at: now.after(RETRY_BACKOFF),
                    }
                };
                None
            }
            Input::RetryDue(now) => self.retry_if_due(now),
        }
    }

    fn retry_if_due(&mut self, now: MonotonicTime) -> Option<Effect> {
        let TapState::Degraded { retry_at, .. } = self.state else {
            return None;
        };
        if now < retry_at {
            return None;
        }
        self.state = TapState::Recreating(RecreateCause::Retry);
        Some(Effect::Recreate)
    }
}

pub(crate) trait EventTapApi {
    fn enable(&mut self);
    fn is_enabled(&self) -> bool;
    fn recreate(&mut self) -> bool;
    fn secure_input_enabled(&self) -> bool;
}

pub(crate) struct Driver<A> {
    lifecycle: Lifecycle,
    api: A,
    degraded_entries: u64,
}

impl<A: EventTapApi> Driver<A> {
    pub(crate) fn start(api: A, now: MonotonicTime) -> Self {
        let (lifecycle, effect) = Lifecycle::new();
        let mut driver = Self {
            lifecycle,
            api,
            degraded_entries: 0,
        };
        driver.run_effect(effect, now);
        driver
    }

    pub(crate) fn state(&self) -> TapState {
        self.lifecycle.state()
    }

    pub(crate) fn is_degraded(&self) -> bool {
        matches!(self.state(), TapState::Degraded { .. })
    }

    #[cfg(test)]
    pub(crate) fn secure_input(&self) -> bool {
        self.lifecycle.secure_input()
    }

    pub(crate) const fn degraded_entries(&self) -> u64 {
        self.degraded_entries
    }

    pub(crate) fn disabled(&mut self, reason: DisableReason, now: MonotonicTime) {
        self.apply(Input::Disabled(reason), now);
    }

    pub(crate) fn watchdog(&mut self, now: MonotonicTime) {
        let input = Input::Watchdog {
            tap_enabled: self.api.is_enabled(),
            secure_input: self.api.secure_input_enabled(),
            now,
        };
        self.apply(input, now);
    }

    pub(crate) fn wake(&mut self, now: MonotonicTime) {
        self.apply(Input::Wake, now);
    }

    pub(crate) fn retry(&mut self, now: MonotonicTime) {
        self.apply(Input::RetryDue(now), now);
    }

    pub(crate) fn api_mut(&mut self) -> &mut A {
        &mut self.api
    }

    fn apply(&mut self, input: Input, now: MonotonicTime) {
        if let Some(effect) = self.transition(input) {
            self.run_effect(effect, now);
        }
    }

    fn transition(&mut self, input: Input) -> Option<Effect> {
        let was_degraded = matches!(self.lifecycle.state(), TapState::Degraded { .. });
        let effect = self.lifecycle.transition(input);
        if !was_degraded && matches!(self.lifecycle.state(), TapState::Degraded { .. }) {
            self.degraded_entries = self.degraded_entries.saturating_add(1);
        }
        effect
    }

    fn run_effect(&mut self, effect: Effect, now: MonotonicTime) {
        let mut next = Some(effect);
        while let Some(effect) = next.take() {
            let result = match effect {
                Effect::Enable => {
                    self.api.enable();
                    Input::EnableFinished {
                        enabled: self.api.is_enabled(),
                    }
                }
                Effect::Recreate => Input::RecreateFinished {
                    enabled: self.api.recreate() && self.api.is_enabled(),
                    now,
                },
            };
            next = self.transition(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque};

    use super::*;

    #[derive(Default)]
    struct FakeApi {
        calls: Vec<&'static str>,
        enabled_results: RefCell<VecDeque<bool>>,
        recreate_results: VecDeque<bool>,
        secure_input: bool,
    }

    impl FakeApi {
        fn with_results(
            enabled_results: impl IntoIterator<Item = bool>,
            recreate_results: impl IntoIterator<Item = bool>,
        ) -> Self {
            Self {
                enabled_results: RefCell::new(enabled_results.into_iter().collect()),
                recreate_results: recreate_results.into_iter().collect(),
                ..Self::default()
            }
        }
    }

    impl EventTapApi for FakeApi {
        fn enable(&mut self) {
            self.calls.push("enable");
        }

        fn is_enabled(&self) -> bool {
            self.enabled_results
                .borrow_mut()
                .pop_front()
                .unwrap_or(false)
        }

        fn recreate(&mut self) -> bool {
            self.calls.push("recreate");
            self.recreate_results.pop_front().unwrap_or(false)
        }

        fn secure_input_enabled(&self) -> bool {
            self.secure_input
        }
    }

    fn at(seconds: u64) -> MonotonicTime {
        MonotonicTime::from_duration(Duration::from_secs(seconds))
    }

    #[test]
    fn startup_recreate_success_enables_the_tap() {
        let api = FakeApi::with_results([true], [true]);
        let driver = Driver::start(api, at(0));
        assert_eq!(driver.state(), TapState::Enabled);
        assert_eq!(driver.api.calls, ["recreate"]);
    }

    #[test]
    fn failed_startup_retries_after_exact_backoff() {
        let api = FakeApi::with_results([], [false, false]);
        let mut driver = Driver::start(api, at(5));
        assert_eq!(
            driver.state(),
            TapState::Degraded {
                cause: RecreateCause::Startup,
                retry_at: at(35),
            }
        );
        driver.retry(at(34));
        assert_eq!(driver.api.calls, ["recreate"]);
        driver.retry(at(35));
        assert_eq!(driver.api.calls, ["recreate", "recreate"]);
        assert_eq!(
            driver.state(),
            TapState::Degraded {
                cause: RecreateCause::Retry,
                retry_at: at(65),
            }
        );
    }

    #[test]
    fn disabled_tap_returns_to_enabled_without_recreation_when_enable_succeeds() {
        let api = FakeApi::with_results([true, true], [true]);
        let mut driver = Driver::start(api, at(0));
        driver.disabled(DisableReason::Timeout, at(1));

        assert_eq!(driver.api.calls, ["recreate", "enable"]);
        assert_eq!(driver.state(), TapState::Enabled);
    }

    #[test]
    fn disable_escalates_from_enable_to_successful_recreation() {
        let api = FakeApi::with_results([true, false, true], [true, true]);
        let mut driver = Driver::start(api, at(0));
        driver.disabled(DisableReason::UserInput, at(1));

        assert_eq!(driver.api.calls, ["recreate", "enable", "recreate"]);
        assert_eq!(driver.state(), TapState::Enabled);
    }

    #[test]
    fn failed_enable_and_recreation_enters_degraded_state() {
        let api = FakeApi::with_results([true, false], [true, false]);
        let mut driver = Driver::start(api, at(0));
        driver.disabled(DisableReason::Timeout, at(7));

        assert_eq!(
            driver.state(),
            TapState::Degraded {
                cause: RecreateCause::Disabled(DisableReason::Timeout),
                retry_at: at(37),
            }
        );
    }

    #[test]
    fn watchdog_uses_the_same_enable_then_recreate_escalation() {
        let api = FakeApi::with_results([true, false, false, true], [true, true]);
        let mut driver = Driver::start(api, at(0));
        driver.watchdog(at(10));

        assert_eq!(driver.api.calls, ["recreate", "enable", "recreate"]);
        assert_eq!(driver.state(), TapState::Enabled);
    }

    #[test]
    fn wake_skips_enable_and_recreates_directly() {
        let api = FakeApi::with_results([true, true], [true, true]);
        let mut driver = Driver::start(api, at(0));
        driver.wake(at(10));
        assert_eq!(driver.api.calls, ["recreate", "recreate"]);
        assert_eq!(driver.state(), TapState::Enabled);
    }

    #[test]
    fn secure_input_is_orthogonal_to_tap_health() {
        let mut api = FakeApi::with_results([true, true], [true]);
        api.secure_input = true;
        let mut driver = Driver::start(api, at(0));
        driver.watchdog(at(10));
        assert_eq!(driver.state(), TapState::Enabled);
        assert!(driver.secure_input());
    }

    #[test]
    fn duplicate_disable_does_not_start_another_recovery() {
        let (mut lifecycle, _) = Lifecycle::new();
        lifecycle.state = TapState::Disabled(DisableReason::Timeout);
        assert_eq!(
            lifecycle.transition(Input::Disabled(DisableReason::UserInput)),
            None
        );
        assert_eq!(
            lifecycle.state(),
            TapState::Disabled(DisableReason::Timeout)
        );
    }

    #[test]
    fn degraded_entries_increment_once_per_entry_not_per_poll() {
        let api = FakeApi::with_results([], [false, false]);
        let mut driver = Driver::start(api, at(0));
        assert_eq!(driver.degraded_entries(), 1);
        driver.retry(at(29));
        driver.retry(at(29));
        assert_eq!(driver.degraded_entries(), 1);
        driver.retry(at(30));
        assert_eq!(driver.degraded_entries(), 2);
    }

    #[test]
    fn successful_retry_clears_current_degradation_without_erasing_history() {
        let api = FakeApi::with_results([true, true], [false, true]);
        let mut driver = Driver::start(api, at(0));
        assert!(driver.is_degraded());
        assert_eq!(driver.degraded_entries(), 1);

        driver.retry(at(30));

        assert!(!driver.is_degraded());
        assert_eq!(driver.state(), TapState::Enabled);
        assert_eq!(driver.degraded_entries(), 1);
    }
}
