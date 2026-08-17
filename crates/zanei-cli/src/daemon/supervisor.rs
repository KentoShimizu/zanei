use std::{
    collections::{BTreeMap, BTreeSet},
    sync::mpsc::SyncSender,
    time::{Duration, Instant},
};

use zanei_collector::{Collector, Permission, RawEvent};
use zanei_core::store::{DaemonPermissions, PermissionState};

use super::{
    DaemonError,
    collectors::{
        CollectorHealth, CollectorSet,
        relay::{Relay, RelayExit},
    },
};

const RESTART_DELAYS: [Duration; 5] = [
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
    Duration::from_secs(40),
    Duration::from_secs(60),
];
const RESTART_STABLE_AFTER: Duration = Duration::from_secs(60);

impl CollectorSet {
    pub(crate) fn start(&mut self, sender: &SyncSender<RawEvent>) {
        let now = Instant::now();
        // AX Secure Input probes are served by the EventTap worker.
        start_collector(&mut self.eventtap, sender, &mut self.start_errors, now);
        start_collector(&mut self.ax, sender, &mut self.start_errors, now);
        start_collector(&mut self.chrome, sender, &mut self.start_errors, now);
        start_collector(&mut self.workspace, sender, &mut self.start_errors, now);
    }

    pub(crate) fn stop(&mut self) {
        stop_collector(&mut self.workspace, StopMode::Drain);
        stop_collector(&mut self.chrome, StopMode::Drain);
        stop_collector(&mut self.ax, StopMode::Drain);
        stop_collector(&mut self.eventtap, StopMode::Drain);
        self.start_errors.clear();
    }

    pub(crate) fn suspend(&mut self) {
        stop_collector(&mut self.workspace, StopMode::Discard);
        stop_collector(&mut self.chrome, StopMode::Discard);
        stop_collector(&mut self.ax, StopMode::Discard);
        stop_collector(&mut self.eventtap, StopMode::Discard);
        self.start_errors.clear();
    }

    pub(crate) fn supervise(
        &mut self,
        sender: &SyncSender<RawEvent>,
        permissions: &DaemonPermissions,
        now: Instant,
    ) -> Result<(), DaemonError> {
        // Preserve startup ordering because AX Secure Input probes are served
        // by EventTap and browser/AX collectors consume workspace lifecycle.
        supervise_collector(
            &mut self.eventtap,
            sender,
            permissions,
            &mut self.start_errors,
            now,
        )?;
        supervise_collector(
            &mut self.ax,
            sender,
            permissions,
            &mut self.start_errors,
            now,
        )?;
        supervise_collector(
            &mut self.chrome,
            sender,
            permissions,
            &mut self.start_errors,
            now,
        )?;
        supervise_collector(
            &mut self.workspace,
            sender,
            permissions,
            &mut self.start_errors,
            now,
        )
    }

    pub(crate) fn health(&self) -> CollectorHealth {
        let mut health = CollectorHealth {
            degraded: self.start_errors.clone(),
            ..CollectorHealth::default()
        };
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
            if eventtap.collector.is_degraded() {
                health.degraded.insert(
                    "eventtap".to_owned(),
                    "event capture or wake recovery is unavailable".to_owned(),
                );
            }
            if eventtap.collector.secure_input_enabled() {
                health.degraded.insert(
                    "secure_input".to_owned(),
                    "macOS Secure Input is active; input.key delivery is suspended".to_owned(),
                );
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
        }
        health
    }
}

impl Drop for CollectorSet {
    fn drop(&mut self) {
        self.suspend();
    }
}

pub(super) struct Managed<C> {
    pub(super) collector: C,
    running: bool,
    pub(super) relay: Option<Relay>,
    restart: RestartState,
    started_at: Option<Instant>,
    pub(super) relay_dropped: u64,
}

impl<C> Managed<C> {
    pub(super) const fn new(collector: C) -> Self {
        Self {
            collector,
            running: false,
            relay: None,
            restart: RestartState::new(),
            started_at: None,
            relay_dropped: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RestartState {
    next_attempt: Option<Instant>,
    delay_index: usize,
    waiting_for_permission: bool,
}

impl RestartState {
    const fn new() -> Self {
        Self {
            next_attempt: None,
            delay_index: 0,
            waiting_for_permission: false,
        }
    }

    fn failed(&mut self, now: Instant, permissions_granted: bool) {
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

    fn ready(self, now: Instant, permissions_granted: bool) -> bool {
        (self.waiting_for_permission && permissions_granted)
            || self
                .next_attempt
                .is_some_and(|next_attempt| now >= next_attempt)
    }

    fn restarted(&mut self) {
        self.next_attempt = None;
        self.waiting_for_permission = false;
    }

    fn stable(&mut self) {
        *self = Self::new();
    }
}

pub(super) trait ManagedCollector {
    fn worker_name(&self) -> &str;
    fn worker_permissions(&self) -> BTreeSet<Permission>;
    fn start_worker(&mut self, sender: SyncSender<RawEvent>) -> Result<(), String>;
    fn stop_worker(&mut self);
}

impl<C: Collector> ManagedCollector for C {
    fn worker_name(&self) -> &str {
        Collector::name(self)
    }

    fn worker_permissions(&self) -> BTreeSet<Permission> {
        Collector::required_permissions(self)
            .iter()
            .cloned()
            .collect()
    }

    fn start_worker(&mut self, sender: SyncSender<RawEvent>) -> Result<(), String> {
        Collector::start(self, sender).map_err(|error| error.to_string())
    }

    fn stop_worker(&mut self) {
        Collector::stop(self);
    }
}

pub(super) fn start_collector<C: ManagedCollector>(
    managed: &mut Option<Managed<C>>,
    sender: &SyncSender<RawEvent>,
    errors: &mut BTreeMap<String, String>,
    now: Instant,
) {
    let Some(managed) = managed.as_mut() else {
        return;
    };
    start_managed(managed, sender, errors, now, true);
}

fn start_managed<C: ManagedCollector>(
    managed: &mut Managed<C>,
    sender: &SyncSender<RawEvent>,
    errors: &mut BTreeMap<String, String>,
    now: Instant,
    permissions_granted: bool,
) {
    let name = managed.collector.worker_name().to_owned();
    if managed.running {
        return;
    }
    let (mut relay, collector_sender) = match Relay::spawn(&name, sender.clone()) {
        Ok(relay) => relay,
        Err(error) => {
            errors.insert(name, error.to_string());
            managed.restart.failed(now, permissions_granted);
            return;
        }
    };
    match managed.collector.start_worker(collector_sender) {
        Ok(()) => {
            managed.running = true;
            managed.relay = Some(relay);
            managed.started_at = Some(now);
            managed.restart.restarted();
            errors.remove(&name);
        }
        Err(error) => {
            let _ = relay.stop();
            errors.insert(name, error);
            managed.restart.failed(now, permissions_granted);
        }
    }
}

#[derive(Clone, Copy)]
enum StopMode {
    Drain,
    Discard,
}

fn stop_collector<C: ManagedCollector>(managed: &mut Option<Managed<C>>, mode: StopMode) {
    let Some(managed) = managed.as_mut() else {
        return;
    };
    if managed.running {
        managed.collector.stop_worker();
        managed.running = false;
    }
    if let Some(mut relay) = managed.relay.take() {
        let dropped = match mode {
            StopMode::Drain => relay.join().map(|_| 0),
            StopMode::Discard => relay.stop(),
        };
        if let Ok(dropped) = dropped {
            managed.relay_dropped = managed.relay_dropped.saturating_add(dropped);
        }
    }
    managed.started_at = None;
}

pub(super) fn supervise_collector<C: ManagedCollector>(
    managed: &mut Option<Managed<C>>,
    sender: &SyncSender<RawEvent>,
    permissions: &DaemonPermissions,
    errors: &mut BTreeMap<String, String>,
    now: Instant,
) -> Result<(), DaemonError> {
    let Some(managed) = managed.as_mut() else {
        return Ok(());
    };
    let name = managed.collector.worker_name().to_owned();
    let required = managed.collector.worker_permissions();
    let granted = permissions_granted(&required, permissions);

    if managed.running && managed.relay.as_ref().is_some_and(Relay::is_finished) {
        managed.collector.stop_worker();
        managed.running = false;
        managed.started_at = None;
        let exit = managed
            .relay
            .as_mut()
            .ok_or(DaemonError::ThreadTerminated {
                thread: "collector relay",
            })?
            .join()?;
        managed.relay = None;
        let reason = match exit {
            RelayExit::SourceClosed => "collector worker terminated unexpectedly",
            RelayExit::PipelineClosed => "collector relay lost the pipeline",
            RelayExit::Stopped { dropped } => {
                managed.relay_dropped = managed.relay_dropped.saturating_add(dropped);
                "collector relay stopped unexpectedly"
            }
        };
        errors.insert(name.clone(), reason.to_owned());
        managed.restart.failed(now, granted);
    }

    if managed.running {
        if managed
            .started_at
            .is_some_and(|started_at| now.duration_since(started_at) >= RESTART_STABLE_AFTER)
        {
            managed.restart.stable();
        }
        return Ok(());
    }

    if managed.restart.ready(now, granted) {
        start_managed(managed, sender, errors, now, granted);
    }
    Ok(())
}

fn permissions_granted(required: &BTreeSet<Permission>, permissions: &DaemonPermissions) -> bool {
    required.iter().all(|permission| match permission {
        Permission::Accessibility => permissions.accessibility == PermissionState::Granted,
        Permission::InputMonitoring => permissions.input_monitoring == PermissionState::Granted,
        Permission::Automation { bundle_id } => {
            permissions.automation.get(bundle_id) == Some(&PermissionState::Granted)
        }
    })
}

fn add_failure_count(failures: &mut BTreeMap<String, u64>, collector: &str, count: u64) {
    if count > 0 {
        failures.insert(collector.to_owned(), count);
    }
}
