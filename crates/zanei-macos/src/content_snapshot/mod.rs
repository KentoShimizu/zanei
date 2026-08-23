//! Dedicated worker for opt-in, frontmost-window content snapshots.

pub(crate) mod budget;
mod output;
mod role;
mod scheduler;
mod state;
mod trigger;
mod walker;
mod worker;

use std::{
    fmt,
    sync::{
        Arc, Mutex, MutexGuard, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, Sender, SyncSender, channel, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::{
    CapturePolicy, chrome::ChromeObserver, focus_context::FocusContext, workspace::WorkspaceEvent,
};
use zanei_collector::{Collector, CollectorError, Permission, RawEvent};

use self::state::SnapshotState;

pub use crate::ffi::activity::{ActivityError, seconds_since_last_input};
pub use crate::ffi::ax::{
    AxFrame, AxPoint, AxSize, AxTextRange, SnapshotAttribute, SnapshotAttributeResult,
    SnapshotAttributeValue, SnapshotAxApplication, SnapshotAxElement, SnapshotAxError,
};
pub use role::{SnapshotNodeClass, classify_role};
#[cfg(test)]
pub(crate) use scheduler::SnapshotScheduler;
#[cfg(test)]
pub(crate) use trigger::SnapshotTriggerMessage;
#[cfg(test)]
pub(crate) use trigger::snapshot_trigger_channel_with_capacity;
pub use trigger::{
    SnapshotTrigger, SnapshotTriggerKind, SnapshotTriggerPublisher, SnapshotTriggerReceiver,
    snapshot_trigger_channel,
};
pub use walker::{SnapshotCutoff, SnapshotWalkOutput};

const REQUIRED_PERMISSIONS: [Permission; 1] = [Permission::Accessibility];
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(25);
const FILTER_REPLACE_TIMEOUT: Duration = Duration::from_secs(2);

pub struct ContentSnapshotCollector {
    channels: Arc<Mutex<WorkerChannels>>,
    capture_policy: CapturePolicy,
    chrome_observer: ChromeObserver,
    focus_context: FocusContext,
    worker: Option<Worker>,
    health: SharedHealth,
    #[cfg(test)]
    panic_next_worker: Arc<AtomicBool>,
}

struct Worker {
    stop: Arc<AtomicBool>,
    control: Sender<Control>,
    handle: JoinHandle<()>,
}

struct WorkerChannels {
    trigger: SnapshotTriggerReceiver,
    lifecycle: Receiver<WorkspaceEvent>,
    state: SnapshotState,
}

enum Control {
    ReplaceFilter { acknowledge: SyncSender<()> },
    Stop,
}

#[derive(Clone, Default)]
struct SharedHealth {
    dropped: Arc<AtomicU64>,
    failures: Arc<AtomicU64>,
    degraded: Arc<RwLock<Option<String>>>,
    processed_triggers: Arc<AtomicU64>,
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
        capture_policy: CapturePolicy,
        chrome_observer: ChromeObserver,
        focus_context: FocusContext,
    ) -> Self {
        Self {
            channels: Arc::new(Mutex::new(WorkerChannels {
                trigger: trigger_receiver,
                lifecycle: lifecycle_receiver,
                state: SnapshotState::new(std::time::Instant::now()),
            })),
            capture_policy,
            chrome_observer,
            focus_context,
            worker: None,
            health: SharedHealth::default(),
            #[cfg(test)]
            panic_next_worker: Arc::new(AtomicBool::new(false)),
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

    pub fn filter_replaced(&mut self) -> Result<(), ContentSnapshotControlError> {
        let Some(worker) = self.worker.as_ref() else {
            return Ok(());
        };
        let (acknowledge, acknowledged) = sync_channel(0);
        worker
            .control
            .send(Control::ReplaceFilter { acknowledge })
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

    #[cfg(test)]
    pub(crate) fn panic_next_worker_for_test(&self) {
        self.panic_next_worker.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn processed_triggers_for_test(&self) -> u64 {
        self.health.processed_triggers.load(Ordering::Acquire)
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
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (control, controls) = channel();
        let capture_policy = self.capture_policy.clone();
        let chrome_observer = self.chrome_observer.clone();
        let health = self.health.clone();
        let focus_context = self.focus_context.clone();
        let channels = Arc::clone(&self.channels);
        #[cfg(test)]
        let panic_next_worker = Arc::clone(&self.panic_next_worker);
        let handle = thread::Builder::new()
            .name("zanei-content".to_owned())
            .spawn(move || {
                let mut channels = lock_channels(&channels);
                #[cfg(test)]
                if panic_next_worker.swap(false, Ordering::AcqRel) {
                    panic!("injected content snapshot worker panic");
                }
                let WorkerChannels {
                    trigger,
                    lifecycle,
                    state,
                } = &mut *channels;
                worker::run_worker(
                    trigger,
                    lifecycle,
                    controls,
                    thread_stop,
                    sender,
                    capture_policy,
                    chrome_observer,
                    health,
                    state,
                    focus_context,
                );
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
        let _ = worker.handle.join();
        let channels = lock_channels(&self.channels);
        while channels.trigger.try_recv().is_ok() {}
        channels.lifecycle.try_iter().for_each(drop);
        if let Ok(mut degraded) = self.health.degraded.write() {
            *degraded = None;
        }
    }
}

fn lock_channels(channels: &Mutex<WorkerChannels>) -> MutexGuard<'_, WorkerChannels> {
    channels
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for ContentSnapshotCollector {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
