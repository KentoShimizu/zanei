//! Bounded, non-blocking AX-to-content-worker trigger delivery.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    time::{Duration, Instant},
};

use crate::{ax::NativeWindow, workspace::ApplicationInfo};

const SNAPSHOT_TRIGGER_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotTriggerKind {
    Focus,
    FocusOut,
    Title,
}

impl SnapshotTriggerKind {
    const fn trace_name(self) -> &'static str {
        match self {
            Self::Focus => "focus",
            Self::FocusOut => "focus_out",
            Self::Title => "title",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotTrigger {
    pub app: ApplicationInfo,
    pub window: NativeWindow,
    pub kind: SnapshotTriggerKind,
    pub observed_at: Instant,
}

#[derive(Clone)]
pub struct SnapshotTriggerPublisher {
    sender: SyncSender<SnapshotTrigger>,
    dropped: Arc<AtomicU64>,
}

pub struct SnapshotTriggerReceiver {
    receiver: Receiver<SnapshotTrigger>,
}

#[must_use]
pub fn snapshot_trigger_channel() -> (SnapshotTriggerPublisher, SnapshotTriggerReceiver) {
    snapshot_trigger_channel_with_capacity(SNAPSHOT_TRIGGER_CAPACITY)
}

fn snapshot_trigger_channel_with_capacity(
    capacity: usize,
) -> (SnapshotTriggerPublisher, SnapshotTriggerReceiver) {
    let (sender, receiver) = sync_channel(capacity);
    (
        SnapshotTriggerPublisher {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
        },
        SnapshotTriggerReceiver { receiver },
    )
}

impl SnapshotTriggerPublisher {
    pub fn publish(&self, trigger: SnapshotTrigger) -> bool {
        match self.sender.try_send(trigger) {
            Ok(()) => true,
            Err(TrySendError::Full(trigger)) => {
                self.trace_drop(&trigger, "queue_full");
                false
            }
            Err(TrySendError::Disconnected(trigger)) => {
                self.trace_drop(&trigger, "queue_disconnected");
                false
            }
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn trace_drop(&self, trigger: &SnapshotTrigger, reason: &'static str) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        crate::trace::trace!(
            "component=content_snapshot phase=trigger action=drop kind={} pid={} window_id={} reason={}",
            trigger.kind.trace_name(),
            trigger.app.pid,
            trigger.window.id.unwrap_or_default(),
            reason
        );
    }
}

impl SnapshotTriggerReceiver {
    pub fn try_recv(&self) -> Result<SnapshotTrigger, TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<SnapshotTrigger, RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc::sync_channel,
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::workspace::ApplicationActivationPolicy;

    fn trigger() -> SnapshotTrigger {
        SnapshotTrigger {
            app: ApplicationInfo {
                name: "Example".to_owned(),
                bundle_id: Some("dev.example.App".to_owned()),
                pid: 7,
                activation_policy: ApplicationActivationPolicy::Regular,
            },
            window: NativeWindow {
                title: Some("Window".to_owned()),
                id: Some(11),
            },
            kind: SnapshotTriggerKind::Focus,
            observed_at: Instant::now(),
        }
    }

    #[test]
    fn full_channel_drops_without_waiting_for_the_receiver() {
        let (publisher, receiver) = snapshot_trigger_channel_with_capacity(1);
        assert!(publisher.publish(trigger()));
        let worker_publisher = publisher.clone();
        let (completed, completion) = sync_channel(1);
        let worker = thread::spawn(move || {
            let published = worker_publisher.publish(trigger());
            completed.send(published).expect("completion receiver");
        });

        assert_eq!(
            completion.recv_timeout(Duration::from_millis(100)),
            Ok(false),
            "try_send must return before queue capacity becomes available"
        );
        assert_eq!(publisher.dropped(), 1);
        assert!(receiver.try_recv().is_ok());
        worker.join().expect("publisher thread");
    }

    #[test]
    fn disconnected_channel_is_counted_as_a_drop() {
        let (publisher, receiver) = snapshot_trigger_channel_with_capacity(1);
        drop(receiver);

        assert!(!publisher.publish(trigger()));
        assert_eq!(publisher.dropped(), 1);
    }

    #[test]
    fn receiver_supports_bounded_waits() {
        let (publisher, receiver) = snapshot_trigger_channel_with_capacity(1);
        assert!(publisher.publish(trigger()));

        assert!(receiver.recv_timeout(Duration::ZERO).is_ok());
    }
}
