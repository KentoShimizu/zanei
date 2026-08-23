//! AX output delivery with delayed Chrome text confirmation.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{SyncSender, TrySendError},
};

use zanei_collector::RawEvent;

use crate::text_capture::{ChromeWindowKey, TextContentPolicy, TextQuarantine};

pub(super) struct AxOutput<'a> {
    sender: &'a SyncSender<RawEvent>,
    dropped_events: &'a AtomicU64,
    text_policy: TextContentPolicy,
    chrome_tracker: crate::chrome::ChromeEligibilityTracker,
    quarantine: TextQuarantine,
}

impl<'a> AxOutput<'a> {
    pub(super) fn new(
        sender: &'a SyncSender<RawEvent>,
        dropped_events: &'a AtomicU64,
        text_policy: TextContentPolicy,
    ) -> Self {
        let chrome_tracker = text_policy.chrome_tracker();
        let quarantine = TextQuarantine::new(&chrome_tracker);
        Self {
            sender,
            dropped_events,
            text_policy,
            chrome_tracker,
            quarantine,
        }
    }

    pub(super) fn send_all(&mut self, events: Vec<RawEvent>) {
        for event in events {
            self.send(event);
        }
    }

    pub(super) fn send(&mut self, event: RawEvent) {
        let decision = self.text_policy.decision(
            &event.app,
            event.window.as_ref().and_then(|window| window.id),
        );
        let key = event
            .app
            .pid
            .zip(event.window.as_ref().and_then(|window| window.id))
            .map(|(pid, window_id)| ChromeWindowKey { pid, window_id });
        if event.event_type == "ui.value"
            && event_has_body(&event)
            && decision.is_allowed()
            && let (Some(version), Some(key), Some(observed_at)) =
                (decision.chrome_version(), key, event.observed_at)
        {
            self.quarantine.hold(event, key, version, observed_at);
            return;
        }
        self.send_now(event);
    }

    pub(super) fn release_due(&mut self) {
        for event in self
            .quarantine
            .release(time::OffsetDateTime::now_utc(), &self.chrome_tracker)
        {
            self.send_now(event);
        }
    }

    pub(super) fn flush(&mut self) {
        for event in self.quarantine.flush(&self.chrome_tracker) {
            self.send_now(event);
        }
    }

    fn send_now(&self, event: RawEvent) {
        match self.sender.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(event)) => self.drop(event, "output_full"),
            Err(TrySendError::Disconnected(event)) => self.drop(event, "output_disconnected"),
        }
    }

    fn drop(&self, event: RawEvent, reason: &str) {
        crate::trace::trace!(
            "component=ax phase=output action=drop event={} reason={}",
            event.event_type,
            reason
        );
        self.dropped_events.fetch_add(1, Ordering::Relaxed);
    }
}

fn event_has_body(event: &RawEvent) -> bool {
    let zanei_core::schema::EventData::UiValue(data) = &event.data else {
        return false;
    };
    data.text.is_some()
        || event
            .element
            .as_ref()
            .and_then(|element| element.value.as_ref())
            .is_some()
}
