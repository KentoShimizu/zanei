//! AX output delivery with delayed Chrome text confirmation.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{SyncSender, TrySendError},
};

use zanei_collector::RawEvent;
use zanei_core::privacy::PrivacyScope;

use crate::{
    capture_policy::CapturePolicy,
    chrome::ChromeObserver,
    text_capture::{TextBodyRoute, TextQuarantine, route_text_body},
};

pub(super) struct AxOutput<'a> {
    sender: &'a SyncSender<RawEvent>,
    dropped_events: &'a AtomicU64,
    capture_policy: CapturePolicy,
    quarantine: TextQuarantine,
}

impl<'a> AxOutput<'a> {
    pub(super) fn new(
        sender: &'a SyncSender<RawEvent>,
        dropped_events: &'a AtomicU64,
        capture_policy: CapturePolicy,
        chrome_observer: ChromeObserver,
    ) -> Self {
        let quarantine = TextQuarantine::new(chrome_observer);
        Self {
            sender,
            dropped_events,
            capture_policy,
            quarantine,
        }
    }

    pub(super) fn send_all(&mut self, events: Vec<RawEvent>) {
        for event in events {
            self.send(event);
        }
    }

    pub(super) fn send(&mut self, event: RawEvent) {
        let decision = self.capture_policy.decision(
            PrivacyScope::TextContent,
            &event.app,
            event.window.as_ref().and_then(|window| window.id),
        );
        match route_text_body(event, Some(&decision)) {
            TextBodyRoute::Send(event) => self.send_now(event),
            TextBodyRoute::Quarantine {
                event,
                key,
                version,
                observed_at,
            } => self.quarantine.hold_text(event, key, version, observed_at),
        }
    }

    pub(super) fn release_due(&mut self) {
        for event in self
            .quarantine
            .release(std::time::Instant::now(), &self.capture_policy)
        {
            let (event, _) = event.into_parts();
            self.send_now(event);
        }
    }

    pub(super) fn flush(&mut self) {
        for event in self.quarantine.flush() {
            let (event, _) = event.into_parts();
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

#[cfg(test)]
mod tests {
    use std::sync::{atomic::AtomicU64, mpsc::sync_channel};

    use time::OffsetDateTime;
    use zanei_core::{
        config::FilterConfig,
        schema::{App, Element, EventData, FieldKind, UiValueData, Window},
    };

    use super::*;
    use crate::chrome::chrome_eligibility_channel;

    #[test]
    fn v2_2_final_deny_suppresses_built_ax_body() {
        let filter = FilterConfig::default();
        let (_, tracker) = chrome_eligibility_channel(filter.clone());
        let policy = CapturePolicy::new(tracker, filter, None);
        let (sender, receiver) = sync_channel(1);
        let dropped = AtomicU64::new(0);
        let mut output = AxOutput::new(&sender, &dropped, policy.clone(), ChromeObserver::new());
        let event = RawEvent {
            observed_at: Some(OffsetDateTime::UNIX_EPOCH),
            source: "macos.ax".to_owned(),
            event_type: "ui.value".to_owned(),
            app: App {
                name: "Example".to_owned(),
                bundle_id: Some("dev.example.App".to_owned()),
                pid: Some(7),
            },
            window: Some(Window {
                title: Some("Window".to_owned()),
                id: Some(11),
            }),
            element: Some(Element {
                role: Some("AXTextArea".to_owned()),
                title: None,
                value: Some("private element value".to_owned()),
            }),
            data: EventData::UiValue(UiValueData {
                field_kind: Some(FieldKind::Text),
                value_len: Some(12),
                text: Some("private text".to_owned()),
            }),
            capture_context: Default::default(),
        };
        policy.replace_filter(FilterConfig {
            exclude_apps: vec!["dev.example.App".to_owned()],
            ..FilterConfig::default()
        });

        output.send(event);

        let event = receiver.try_recv().expect("metadata event is retained");
        let EventData::UiValue(data) = event.data else {
            panic!("ui.value");
        };
        assert_eq!(data.text, None);
        assert_eq!(event.element.and_then(|element| element.value), None);
    }
}
