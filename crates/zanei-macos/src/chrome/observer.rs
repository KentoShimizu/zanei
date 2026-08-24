//! Non-blocking observation triggers shared with Chrome-dependent collectors.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
    mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
};

const OBSERVATION_TRIGGER_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ObservationTrigger {
    OnDemand { pid: i64, window_id: i64 },
    PageLoaded { pid: i64 },
}

#[derive(Default)]
struct ObserverState {
    sender: Option<SyncSender<ObservationTrigger>>,
}

/// Best-effort, non-blocking input to the transition-driven Chrome worker.
#[derive(Clone, Default)]
pub struct ChromeObserver {
    state: Arc<Mutex<ObserverState>>,
    dropped: Arc<AtomicU64>,
}

impl ChromeObserver {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests a fresh observation before a quarantined body is released.
    pub fn request_observation(&self, pid: i64, window_id: i64) {
        self.publish(ObservationTrigger::OnDemand { pid, window_id });
    }

    pub(crate) fn page_loaded(&self, pid: i64) {
        self.publish(ObservationTrigger::PageLoaded { pid });
    }

    #[must_use]
    pub fn dropped_triggers(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub(super) fn subscribe(&self) -> Receiver<ObservationTrigger> {
        let (sender, receiver) = sync_channel(OBSERVATION_TRIGGER_CAPACITY);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sender = Some(sender);
        receiver
    }

    fn publish(&self, trigger: ObservationTrigger) {
        let result = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sender
            .as_ref()
            .map(|sender| sender.try_send(trigger));
        if matches!(
            result,
            None | Some(Err(TrySendError::Full(_) | TrySendError::Disconnected(_)))
        ) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_non_blocking_and_count_missing_consumers() {
        let observer = ChromeObserver::new();
        observer.request_observation(7, 11);
        assert_eq!(observer.dropped_triggers(), 1);

        let receiver = observer.subscribe();
        observer.request_observation(7, 11);
        assert_eq!(
            receiver.try_recv(),
            Ok(ObservationTrigger::OnDemand {
                pid: 7,
                window_id: 11
            })
        );
    }
}
