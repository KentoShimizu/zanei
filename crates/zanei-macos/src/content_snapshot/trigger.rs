//! Bounded, non-blocking AX-to-content-worker trigger delivery.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    time::{Duration, Instant},
};

use crate::{ax::NativeWindow, focus_context::FocusTransition, workspace::ApplicationInfo};

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

pub(crate) enum SnapshotTriggerMessage {
    Trigger(SnapshotTrigger),
    FocusTransition {
        transition: FocusTransition,
        observed_at: Instant,
    },
}

#[derive(Clone)]
pub struct SnapshotTriggerPublisher {
    sender: SyncSender<SnapshotTriggerMessage>,
    dropped: Arc<AtomicU64>,
}

pub struct SnapshotTriggerReceiver {
    receiver: Receiver<SnapshotTriggerMessage>,
}

#[must_use]
pub fn snapshot_trigger_channel() -> (SnapshotTriggerPublisher, SnapshotTriggerReceiver) {
    snapshot_trigger_channel_with_capacity(SNAPSHOT_TRIGGER_CAPACITY)
}

pub(crate) fn snapshot_trigger_channel_with_capacity(
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
        self.publish_message(SnapshotTriggerMessage::Trigger(trigger))
    }

    pub(crate) fn publish_focus_transition(&self, transition: FocusTransition) -> bool {
        self.publish_message(SnapshotTriggerMessage::FocusTransition {
            transition,
            observed_at: Instant::now(),
        })
    }

    fn publish_message(&self, message: SnapshotTriggerMessage) -> bool {
        match self.sender.try_send(message) {
            Ok(()) => true,
            Err(TrySendError::Full(message)) => {
                self.trace_drop(&message, "queue_full");
                false
            }
            Err(TrySendError::Disconnected(message)) => {
                self.trace_drop(&message, "queue_disconnected");
                false
            }
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn trace_drop(&self, message: &SnapshotTriggerMessage, reason: &'static str) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        let (kind, pid, window_id) = match message {
            SnapshotTriggerMessage::Trigger(trigger) => (
                trigger.kind.trace_name(),
                trigger.app.pid,
                trigger.window.id.unwrap_or_default(),
            ),
            SnapshotTriggerMessage::FocusTransition { transition, .. } => {
                let focus = transition.current.as_ref().or(transition.previous.as_ref());
                (
                    "focus_transition",
                    focus.map_or(0, |focus| focus.app.pid),
                    focus
                        .and_then(|focus| focus.window.as_ref())
                        .and_then(|window| window.id)
                        .unwrap_or_default(),
                )
            }
        };
        crate::trace::trace!(
            "component=content_snapshot phase=trigger action=drop kind={} pid={} window_id={} reason={}",
            kind,
            pid,
            window_id,
            reason
        );
    }
}

impl SnapshotTriggerReceiver {
    pub(crate) fn try_recv(&self) -> Result<SnapshotTriggerMessage, TryRecvError> {
        self.receiver.try_recv()
    }

    pub(crate) fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<SnapshotTriggerMessage, RecvTimeoutError> {
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
