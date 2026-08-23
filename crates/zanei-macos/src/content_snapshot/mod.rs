//! Dedicated worker for opt-in, frontmost-window content snapshots.

pub(crate) mod budget;
mod policy;
mod role;
mod scheduler;
mod state;
mod trigger;
mod walker;
mod worker;

use std::{
    fmt,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, Sender, SyncSender, channel, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use zanei_collector::{Collector, CollectorError, Permission, RawEvent};
use zanei_core::config::FilterConfig;

use crate::{SecureInputProbe, chrome::ChromeEligibilityTracker, workspace::WorkspaceEvent};

use self::policy::SnapshotPolicy;
use self::state::SnapshotState;

pub use crate::ffi::activity::{ActivityError, seconds_since_last_input};
pub use crate::ffi::ax::{
    AxFrame, AxPoint, AxSize, AxTextRange, SnapshotAttribute, SnapshotAttributeResult,
    SnapshotAttributeValue, SnapshotAxApplication, SnapshotAxElement, SnapshotAxError,
};
pub use role::{SnapshotNodeClass, classify_role};
pub use trigger::{
    SnapshotTrigger, SnapshotTriggerKind, SnapshotTriggerPublisher, SnapshotTriggerReceiver,
    snapshot_trigger_channel,
};
pub use walker::{SnapshotCutoff, SnapshotWalkOutput};

const REQUIRED_PERMISSIONS: [Permission; 1] = [Permission::Accessibility];
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(25);
const FILTER_REPLACE_TIMEOUT: Duration = Duration::from_secs(2);

pub struct ContentSnapshotCollector {
    trigger_receiver: Option<SnapshotTriggerReceiver>,
    lifecycle_receiver: Option<Receiver<WorkspaceEvent>>,
    secure_input: SecureInputProbe,
    chrome: ChromeEligibilityTracker,
    filter: FilterConfig,
    state: Option<SnapshotState>,
    worker: Option<Worker>,
    health: SharedHealth,
}

struct Worker {
    stop: Arc<AtomicBool>,
    control: Sender<Control>,
    handle: JoinHandle<WorkerChannels>,
}

struct WorkerChannels {
    trigger: SnapshotTriggerReceiver,
    lifecycle: Receiver<WorkspaceEvent>,
    state: SnapshotState,
}

enum Control {
    ReplaceFilter {
        filter: Box<FilterConfig>,
        acknowledge: SyncSender<()>,
    },
    Stop,
}

#[derive(Clone, Default)]
struct SharedHealth {
    dropped: Arc<AtomicU64>,
    failures: Arc<AtomicU64>,
    degraded: Arc<RwLock<Option<String>>>,
}

#[derive(Debug)]
pub enum ContentSnapshotControlError {
    Disconnected,
    Timeout,
}

impl fmt::Display for ContentSnapshotControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => formatter.write_str("content snapshot worker is unavailable"),
            Self::Timeout => formatter.write_str("content snapshot filter replacement timed out"),
        }
    }
}

impl std::error::Error for ContentSnapshotControlError {}

impl ContentSnapshotCollector {
    #[must_use]
    pub fn new(
        trigger_receiver: SnapshotTriggerReceiver,
        lifecycle_receiver: Receiver<WorkspaceEvent>,
        secure_input: SecureInputProbe,
        chrome: ChromeEligibilityTracker,
        filter: FilterConfig,
    ) -> Self {
        Self {
            trigger_receiver: Some(trigger_receiver),
            lifecycle_receiver: Some(lifecycle_receiver),
            secure_input,
            chrome,
            filter,
            state: Some(SnapshotState::new(std::time::Instant::now())),
            worker: None,
            health: SharedHealth::default(),
        }
    }

    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.health.dropped.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn collector_failures(&self) -> u64 {
        self.health.failures.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn degraded_reason(&self) -> Option<String> {
        self.health.degraded.read().map_or_else(
            |_| Some("health state is unavailable".to_owned()),
            |value| value.clone(),
        )
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.worker.is_some()
    }

    pub fn replace_filter(
        &mut self,
        filter: FilterConfig,
    ) -> Result<(), ContentSnapshotControlError> {
        self.filter = filter.clone();
        let Some(worker) = self.worker.as_ref() else {
            return Ok(());
        };
        let (acknowledge, acknowledged) = sync_channel(0);
        worker
            .control
            .send(Control::ReplaceFilter {
                filter: Box::new(filter),
                acknowledge,
            })
            .map_err(|_| ContentSnapshotControlError::Disconnected)?;
        acknowledged
            .recv_timeout(FILTER_REPLACE_TIMEOUT)
            .map_err(|error| match error {
                std::sync::mpsc::RecvTimeoutError::Timeout => ContentSnapshotControlError::Timeout,
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    ContentSnapshotControlError::Disconnected
                }
            })
    }
}

impl Collector for ContentSnapshotCollector {
    fn name(&self) -> &str {
        "content_snapshot"
    }

    fn required_permissions(&self) -> &[Permission] {
        &REQUIRED_PERMISSIONS
    }

    fn start(&mut self, sender: SyncSender<RawEvent>) -> Result<(), CollectorError> {
        if self.worker.is_some() {
            return Err(CollectorError::AlreadyRunning {
                collector: self.name().to_owned(),
            });
        }
        let trigger = self
            .trigger_receiver
            .take()
            .ok_or_else(|| CollectorError::Start {
                collector: self.name().to_owned(),
                message: "snapshot trigger channel is unavailable".to_owned(),
            })?;
        let lifecycle = self
            .lifecycle_receiver
            .take()
            .ok_or_else(|| CollectorError::Start {
                collector: self.name().to_owned(),
                message: "snapshot lifecycle channel is unavailable".to_owned(),
            })?;
        let state = self.state.take().ok_or_else(|| CollectorError::Start {
            collector: self.name().to_owned(),
            message: "snapshot state is unavailable".to_owned(),
        })?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (control, controls) = channel();
        let policy = SnapshotPolicy::new(
            self.filter.clone(),
            self.chrome.clone(),
            self.secure_input.clone(),
        );
        let health = self.health.clone();
        let handle = thread::Builder::new()
            .name("zanei-content".to_owned())
            .spawn(move || {
                worker::run_worker(
                    trigger,
                    lifecycle,
                    controls,
                    thread_stop,
                    sender,
                    policy,
                    health,
                    state,
                )
            })
            .map_err(|error| CollectorError::Start {
                collector: self.name().to_owned(),
                message: error.to_string(),
            })?;
        self.worker = Some(Worker {
            stop,
            control,
            handle,
        });
        Ok(())
    }

    fn stop(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        worker.stop.store(true, Ordering::Release);
        let _ = worker.control.send(Control::Stop);
        if let Ok(channels) = worker.handle.join() {
            while channels.trigger.try_recv().is_ok() {}
            channels.lifecycle.try_iter().for_each(drop);
            self.trigger_receiver = Some(channels.trigger);
            self.lifecycle_receiver = Some(channels.lifecycle);
            self.state = Some(channels.state);
        }
        if let Ok(mut degraded) = self.health.degraded.write() {
            *degraded = None;
        }
    }
}

impl Drop for ContentSnapshotCollector {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
