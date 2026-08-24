use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use zanei_macos::chrome::ChromeFailureState;

use crate::daemon::collectors::{CollectorHealth, CollectorSet};

use super::{Managed, ManagedCollector};

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

impl CollectorSet {
    pub(crate) fn health(&self) -> CollectorHealth {
        let mut health = CollectorHealth::default();
        health.degraded.extend(self.start_errors.clone());
        add_restart_degradation(&mut health.degraded, self.content_snapshot.as_ref());
        add_restart_degradation(&mut health.degraded, self.ax.as_ref());
        add_restart_degradation(&mut health.degraded, self.chrome.as_ref());
        add_restart_degradation(&mut health.degraded, self.workspace.as_ref());
        add_restart_degradation(&mut health.degraded, self.eventtap.as_ref());
        for (name, counters) in &self.retained_collector_health {
            health.dropped = health.dropped.saturating_add(counters.dropped);
            add_failure_count(&mut health.collector_failures, name, counters.failures);
        }
        if let Some(workspace) = self.workspace.as_ref() {
            health.dropped = health
                .dropped
                .saturating_add(workspace.collector.dropped_events())
                .saturating_add(workspace.relay_dropped);
        }
        if let Some(ax) = self.ax.as_ref() {
            health.dropped = health.dropped.saturating_add(ax.collector.dropped_events());
            health.dropped = health.dropped.saturating_add(ax.relay_dropped);
            add_failure_count(
                &mut health.collector_failures,
                "ax",
                ax.collector.degraded_operations(),
            );
            let degraded_observers = ax.collector.degraded_observers();
            if degraded_observers > 0 {
                let applications = if degraded_observers == 1 {
                    "application"
                } else {
                    "applications"
                };
                health.degraded.insert(
                    "ax".to_owned(),
                    format!(
                        "observer unavailable for {degraded_observers} {applications} you used (retried on activation)"
                    ),
                );
            }
        }
        if let Some(eventtap) = self.eventtap.as_ref() {
            health.dropped = health
                .dropped
                .saturating_add(eventtap.collector.dropped_events())
                .saturating_add(eventtap.relay_dropped);
            add_failure_count(
                &mut health.collector_failures,
                "eventtap",
                eventtap.collector.degraded_operations(),
            );
            let eventtap_degraded = eventtap.collector.is_degraded();
            #[cfg(test)]
            let eventtap_degraded = self.eventtap_runtime_override.unwrap_or(eventtap_degraded);
            if eventtap_degraded {
                health.degraded.insert(
                    "eventtap".to_owned(),
                    "event capture or wake recovery is unavailable".to_owned(),
                );
            }
            let secure_input_enabled = eventtap.collector.secure_input_enabled();
            #[cfg(test)]
            let secure_input_enabled = self
                .secure_input_runtime_override
                .unwrap_or(secure_input_enabled);
            if secure_input_enabled {
                health.degraded.insert(
                    "secure_input".to_owned(),
                    "macOS Secure Input is active; input.key delivery is suspended".to_owned(),
                );
            }
        }
        if let Some(content) = self.content_snapshot.as_ref() {
            health.dropped = health
                .dropped
                .saturating_add(content.collector.dropped_events())
                .saturating_add(content.relay_dropped);
            add_failure_count(
                &mut health.collector_failures,
                "content_snapshot",
                content.collector.collector_failures(),
            );
            if let Some(reason) = content.collector.degraded_reason() {
                health
                    .degraded
                    .insert("content_snapshot".to_owned(), reason);
            }
        }
        if let Some(chrome) = self.chrome.as_ref() {
            health.dropped = health
                .dropped
                .saturating_add(chrome.collector.dropped_events())
                .saturating_add(chrome.relay_dropped);
            add_failure_count(
                &mut health.collector_failures,
                "chrome",
                chrome.collector.degraded_operations(),
            );
            if let Some(reason) = chrome
                .health()
                .degraded_reason(health.degraded.get("chrome").map(String::as_str))
            {
                health.degraded.insert("chrome".to_owned(), reason);
            }
        }
        for (collector, reason) in self.producer_failures.reasons() {
            health
                .degraded
                .insert(collector.to_owned(), reason.to_owned());
        }
        health
    }
}

fn add_failure_count(failures: &mut BTreeMap<String, u64>, collector: &str, count: u64) {
    if count > 0 {
        let total = failures.entry(collector.to_owned()).or_default();
        *total = total.saturating_add(count);
    }
}

pub(in crate::daemon) fn add_restart_degradation<C: ManagedCollector>(
    degraded: &mut BTreeMap<String, String>,
    managed: Option<&Managed<C>>,
) {
    let Some(managed) = managed else {
        return;
    };
    if let Some(reason) = managed.restart.degraded_reason() {
        degraded
            .entry(managed.collector.worker_name().to_owned())
            .or_insert_with(|| reason.to_owned());
    }
}
