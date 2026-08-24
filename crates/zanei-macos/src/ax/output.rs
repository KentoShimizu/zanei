//! AX output delivery with delayed Chrome text confirmation.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{SyncSender, TrySendError},
};

use zanei_collector::RawEvent;

use crate::{
    capture_policy::CapturePolicy,
    chrome::ChromeObserver,
    text_capture::{TextBodyRoute, TextQuarantine, route_text_body},
};

use super::event::AxEvent;

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

    pub(super) fn send_all(&mut self, events: Vec<AxEvent>) {
        for event in events {
            self.send(event);
        }
    }

    pub(super) fn send(&mut self, event: AxEvent) {
        let (event, read_decision) = event.into_parts();
        match route_text_body(event, &self.capture_policy, read_decision.as_ref()) {
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
        privacy::{CHROME_BUNDLE_ID, PrivacyScope},
        schema::{App, Element, EventData, FieldKind, UiValueData, Window},
    };

    use super::*;
    use crate::{
        ax::event::AxEventBuilder,
        chrome::{ChromeEligibilityObservation, chrome_eligibility_channel},
        ffi::ax::{NativeAxEvent, NativeElement, NativeUiValueEvent, NativeWindow},
        workspace::{ApplicationActivationPolicy, ApplicationInfo},
    };

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

        output.send(AxEvent::new(event));

        let event = receiver.try_recv().expect("metadata event is retained");
        let EventData::UiValue(data) = event.data else {
            panic!("ui.value");
        };
        assert_eq!(data.text, None);
        assert_eq!(event.element.and_then(|element| element.value), None);
    }

    #[test]
    fn v4_1_ui_value_body_is_bound_to_its_read_time_version() {
        let filter = FilterConfig::default();
        let (publisher, tracker) = chrome_eligibility_channel(filter.clone());
        let policy = CapturePolicy::new(tracker, filter, None);
        let app = ApplicationInfo {
            name: "Google Chrome".to_owned(),
            bundle_id: Some(CHROME_BUNDLE_ID.to_owned()),
            pid: 7,
            activation_policy: ApplicationActivationPolicy::Regular,
        };
        publisher.observe(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: "https://v1.example/".to_owned(),
            },
        );
        let read_decision = policy.decision(PrivacyScope::TextContent, &app.raw_app(), Some(11));
        assert!(read_decision.is_allowed());

        publisher.observe(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: "https://v2.example/".to_owned(),
            },
        );
        let send_decision = policy.decision(PrivacyScope::TextContent, &app.raw_app(), Some(11));
        assert_ne!(
            read_decision.chrome_version(),
            send_decision.chrome_version()
        );

        let mut builder = AxEventBuilder::new(policy.clone());
        builder.add_app(app);
        let event = builder
            .event(NativeAxEvent::UiValueChanged(Box::new(
                NativeUiValueEvent {
                    pid: 7,
                    window: Some(NativeWindow {
                        title: Some("Window".to_owned()),
                        id: Some(11),
                    }),
                    element: NativeElement {
                        role: Some("AXTextArea".to_owned()),
                        subrole: None,
                        title: None,
                        value: None,
                        value_len: Some(7),
                    },
                    text: Some("private".to_owned()),
                    capture_decision: Some(read_decision),
                    observed_at: OffsetDateTime::UNIX_EPOCH,
                },
            )))
            .expect("ui.value event");
        let (sender, receiver) = sync_channel(1);
        let dropped = AtomicU64::new(0);
        let mut output = AxOutput::new(&sender, &dropped, policy.clone(), ChromeObserver::new());

        output.send(event);
        assert!(receiver.try_recv().is_err(), "body remains quarantined");
        publisher.observe(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: "https://v2.example/".to_owned(),
            },
        );
        output.release_due();

        let event = receiver.try_recv().expect("metadata event is released");
        let EventData::UiValue(data) = event.data else {
            panic!("ui.value");
        };
        assert_eq!(data.text, None);
        assert_eq!(
            event.capture_context.website_host.as_deref(),
            Some("v2.example")
        );
    }
}
