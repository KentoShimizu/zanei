//! Click, scroll, and key-down input that re-arms the frontmost window's content snapshot.

use std::time::{Duration, Instant};

use crate::{
    content_snapshot::{SnapshotTriggerPublisher, SnapshotWindowKey},
    ffi::eventtap::{NativeContext, NativeEvent},
};

// Design limit: at most two input messages per second enter the snapshot channel. The
// latest held-back input is published once the interval has passed, so a settle still
// waits for the full quiet interval after the last input.
const INPUT_PUBLISH_INTERVAL: Duration = Duration::from_millis(500);

/// The frontmost window the input went to. A key-down carries the focus captured with it
/// when its target process owned the focus; a click or scroll goes to the window under the
/// pointer, so it counts only when its target process owns the current focus.
fn input_window(event: &NativeEvent, focus: Option<&NativeContext>) -> Option<SnapshotWindowKey> {
    let context = match event {
        NativeEvent::Key { target, .. } => &target.as_ref()?.context,
        NativeEvent::Scroll { target_pid, .. } | NativeEvent::MouseDown { target_pid, .. } => {
            focus.filter(|focus| Some(focus.app.pid) == *target_pid)?
        }
        NativeEvent::Disabled(_) => return None,
    };
    Some(SnapshotWindowKey {
        pid: context.app.pid,
        window_id: context.window.as_ref()?.id?,
    })
}

pub(super) struct SnapshotInputTriggers {
    publisher: SnapshotTriggerPublisher,
    published: Option<(SnapshotWindowKey, Instant)>,
    pending: Option<(SnapshotWindowKey, Instant)>,
}

impl SnapshotInputTriggers {
    pub(super) const fn new(publisher: SnapshotTriggerPublisher) -> Self {
        Self {
            publisher,
            published: None,
            pending: None,
        }
    }

    /// Observes one drained batch of events, then publishes held-back input that is due.
    pub(super) fn observe(
        &mut self,
        events: &[NativeEvent],
        focus: Option<&NativeContext>,
        now: Instant,
    ) {
        for window in events.iter().filter_map(|event| input_window(event, focus)) {
            let held_back = self.published.is_some_and(|(published, published_at)| {
                published == window && now.duration_since(published_at) < INPUT_PUBLISH_INTERVAL
            });
            if held_back {
                self.pending = Some((window, now));
            } else {
                self.publish(window, now);
            }
        }
        if let Some((window, observed_at)) = self.pending
            && self.published.is_none_or(|(_, published_at)| {
                now.duration_since(published_at) >= INPUT_PUBLISH_INTERVAL
            })
        {
            self.publish(window, observed_at);
        }
    }

    fn publish(&mut self, window: SnapshotWindowKey, observed_at: Instant) {
        self.pending = None;
        self.published = Some((window, observed_at));
        // The publisher counts and traces a dropped message; input arriving later re-arms.
        self.publisher.publish_input(window, observed_at);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{INPUT_PUBLISH_INTERVAL, SnapshotInputTriggers};
    use crate::{
        content_snapshot::{
            SnapshotTriggerMessage, SnapshotTriggerReceiver, SnapshotWindowKey,
            snapshot_trigger_channel_with_capacity,
        },
        eventtap::logic::{KeyModifiers, KeyObservation},
        ffi::eventtap::{NativeApp, NativeContext, NativeEvent, NativeInputTarget, NativeWindow},
    };

    const FRONTMOST: i64 = 501;
    const BACKGROUND: i64 = 777;

    fn focus(window_id: Option<i64>) -> NativeContext {
        NativeContext {
            app: NativeApp {
                name: "Example".to_owned(),
                bundle_id: Some("dev.example.App".to_owned()),
                pid: FRONTMOST,
            },
            window: Some(NativeWindow {
                title: None,
                id: window_id,
            }),
        }
    }

    fn key(target: Option<NativeContext>) -> NativeEvent {
        NativeEvent::Key {
            observation: KeyObservation {
                key_code: 0,
                modifiers: KeyModifiers::default(),
                text: None,
            },
            target: target.map(|context| NativeInputTarget {
                context,
                focused_field: None,
                focus_generation: 1,
                field_generation: 1,
            }),
            authorization: None,
            secure_input: false,
            ime_active: false,
            observed_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn scroll(target_pid: Option<i64>) -> NativeEvent {
        NativeEvent::Scroll {
            target_pid,
            vertical: 1.0,
            horizontal: 0.0,
            observed_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn click(target_pid: Option<i64>) -> NativeEvent {
        NativeEvent::MouseDown {
            target_pid,
            x: 1.0,
            y: 2.0,
            button: 0,
            click_count: 1,
            observed_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn published(receiver: &SnapshotTriggerReceiver) -> Vec<(SnapshotWindowKey, Instant)> {
        std::iter::from_fn(|| receiver.try_recv().ok())
            .map(|message| match message {
                SnapshotTriggerMessage::Input {
                    window,
                    observed_at,
                } => (window, observed_at),
                _ => panic!("only input messages are published"),
            })
            .collect()
    }

    #[test]
    fn only_input_routed_to_the_frontmost_window_arms_a_snapshot() {
        // Catches a scroll or click over a background window, a key-down whose target did
        // not own the focus, or a window without an id re-arming the frontmost snapshot.
        let window = SnapshotWindowKey {
            pid: FRONTMOST,
            window_id: 11,
        };
        let focused = focus(Some(11));
        let at = Instant::now();
        for (event, current, armed) in [
            (key(Some(focus(Some(11)))), None, true),
            (key(None), Some(&focused), false),
            (key(Some(focus(None))), Some(&focused), false),
            (scroll(Some(FRONTMOST)), Some(&focused), true),
            (scroll(Some(BACKGROUND)), Some(&focused), false),
            (scroll(None), Some(&focused), false),
            (scroll(Some(FRONTMOST)), None, false),
            (click(Some(FRONTMOST)), Some(&focused), true),
            (click(Some(BACKGROUND)), Some(&focused), false),
        ] {
            let (publisher, receiver) = snapshot_trigger_channel_with_capacity(4);
            SnapshotInputTriggers::new(publisher).observe(
                std::slice::from_ref(&event),
                current,
                at,
            );
            let expected = if armed {
                vec![(window, at)]
            } else {
                Vec::new()
            };
            assert_eq!(published(&receiver), expected, "{event:?}");
        }
    }

    #[test]
    fn a_burst_is_rate_limited_but_its_last_input_is_still_published() {
        // Catches unbounded per-event publishing that floods the snapshot channel, and a
        // throttle that loses the last input so the settle fires before input went quiet.
        let (publisher, receiver) = snapshot_trigger_channel_with_capacity(16);
        let mut triggers = SnapshotInputTriggers::new(publisher);
        let focused = focus(Some(11));
        let window = SnapshotWindowKey {
            pid: FRONTMOST,
            window_id: 11,
        };
        let base = Instant::now();
        for millis in [0, 100, 200, 300] {
            let at = base + Duration::from_millis(millis);
            triggers.observe(&[scroll(Some(FRONTMOST))], Some(&focused), at);
        }
        assert_eq!(published(&receiver), vec![(window, base)]);

        triggers.observe(&[], Some(&focused), base + INPUT_PUBLISH_INTERVAL);
        let last = base + Duration::from_millis(300);
        assert_eq!(published(&receiver), vec![(window, last)]);
        triggers.observe(&[], Some(&focused), last + INPUT_PUBLISH_INTERVAL);
        assert!(published(&receiver).is_empty());

        let other = focus(Some(12));
        let switched = base + Duration::from_millis(900);
        triggers.observe(&[key(Some(other))], None, switched);
        assert_eq!(
            published(&receiver),
            vec![(
                SnapshotWindowKey {
                    pid: FRONTMOST,
                    window_id: 12,
                },
                switched
            )]
        );
    }
}
