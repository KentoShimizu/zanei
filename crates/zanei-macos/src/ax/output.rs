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
        config::{
            CapturePolicyConfig, FilterConfig,
            capture_policy::{BrowserMode, BrowserPolicy, IdePolicy, PolicyAction},
        },
        privacy::{CHROME_BUNDLE_ID, PrivacyScope},
        schema::{App, Element, EventData, FieldKind, UiValueData, Window},
    };

    use super::*;
    use crate::permission::SAFARI_BUNDLE_ID;
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
    fn all_ax_bodies_are_bound_to_their_read_time_version() {
        let browsers = [
            ("Google Chrome", CHROME_BUNDLE_ID),
            ("Safari", SAFARI_BUNDLE_ID),
        ];
        let kinds = [
            "focus",
            "click",
            "value",
            "focus_metadata",
            "click_metadata",
        ];
        for (name, bundle_id, kind) in browsers.into_iter().flat_map(|(name, bundle_id)| {
            kinds.into_iter().map(move |kind| (name, bundle_id, kind))
        }) {
            let metadata_only = kind.ends_with("_metadata");
            let scenarios: &[&str] = if bundle_id == SAFARI_BUNDLE_ID && !metadata_only {
                &[
                    "same",
                    "url_changed",
                    "ordinary_same",
                    "ordinary_changed",
                    "to_app_owned",
                    "to_standalone",
                ]
            } else {
                &["same", "url_changed"]
            };
            for scenario in scenarios {
                let filter = if bundle_id == SAFARI_BUNDLE_ID
                    && !matches!(
                        *scenario,
                        "to_app_owned" | "ordinary_same" | "ordinary_changed"
                    ) {
                    safari_filter()
                } else {
                    standalone_filter(bundle_id)
                };
                let (publisher, tracker) = chrome_eligibility_channel(filter.clone());
                let policy = CapturePolicy::new(tracker, filter, None);
                let app = ApplicationInfo {
                    name: name.to_owned(),
                    bundle_id: Some(bundle_id.to_owned()),
                    pid: 7,
                    activation_policy: ApplicationActivationPolicy::Regular,
                };
                if *scenario != "to_app_owned" {
                    publisher.observe(7, browser_observation(bundle_id, "https://v1.example/"));
                }
                let read_decision =
                    policy.decision(PrivacyScope::TextContent, &app.raw_app(), Some(11), None);
                assert_eq!(read_decision.is_allowed(), *scenario != "to_app_owned");

                match *scenario {
                    "url_changed" | "ordinary_changed" => {
                        publisher.observe(7, browser_observation(bundle_id, "https://v2.example/"))
                    }
                    "to_app_owned" => {
                        policy.replace_filter(safari_filter());
                        publisher.observe(7, browser_observation(bundle_id, "https://v2.example/"));
                        let current = policy.decision(
                            PrivacyScope::TextContent,
                            &app.raw_app(),
                            Some(11),
                            None,
                        );
                        assert!(current.is_allowed());
                        assert!(current.chrome_version().is_some());
                    }
                    "to_standalone" => policy.replace_filter(standalone_filter(bundle_id)),
                    "same" | "ordinary_same" => {}
                    _ => unreachable!(),
                }
                if matches!(*scenario, "url_changed" | "ordinary_changed") {
                    assert_ne!(
                        read_decision.chrome_version(),
                        policy
                            .decision(PrivacyScope::TextContent, &app.raw_app(), Some(11), None,)
                            .chrome_version()
                    );
                }

                let mut builder = AxEventBuilder::new(policy.clone());
                builder.add_app(app);
                let element = NativeElement {
                    role: Some(
                        if kind == "value" {
                            "AXTextArea"
                        } else {
                            "AXStaticText"
                        }
                        .to_owned(),
                    ),
                    subrole: None,
                    title: None,
                    value: (kind != "value" && !metadata_only).then(|| "private".to_owned()),
                    value_len: Some(7),
                    capture_decision: (!metadata_only).then(|| Box::new(read_decision)),
                };
                let window = Some(NativeWindow {
                    title: Some("Window".to_owned()),
                    id: Some(11),
                });
                let observed_at = OffsetDateTime::UNIX_EPOCH;
                let event = match kind {
                    "focus" | "focus_metadata" => builder.event(NativeAxEvent::UiFocused {
                        pid: 7,
                        generation: 1,
                        window,
                        element: Some(element),
                        observed_at,
                    }),
                    "click" | "click_metadata" => builder.click_event(
                        crate::ffi::ax::NativeHitTest {
                            pid: 7,
                            window,
                            element,
                        },
                        crate::ax::ClickObservation {
                            pid: 7,
                            x: 0.0,
                            y: 0.0,
                            button: zanei_core::schema::ClickButton::Left,
                            click_count: 1,
                            observed_at,
                        },
                    ),
                    "value" => builder.event(NativeAxEvent::UiValueChanged(Box::new(
                        NativeUiValueEvent {
                            pid: 7,
                            window,
                            element,
                            text: Some("private".to_owned()),
                            observed_at,
                        },
                    ))),
                    _ => unreachable!(),
                }
                .expect("AX event");
                let (sender, receiver) = sync_channel(1);
                let dropped = AtomicU64::new(0);
                let mut output =
                    AxOutput::new(&sender, &dropped, policy.clone(), ChromeObserver::new());

                output.send(event);
                if *scenario == "to_app_owned" {
                    let event = receiver
                        .try_recv()
                        .expect("unobserved body leaves metadata only");
                    assert_eq!(event_body(&event), None, "{kind}, {scenario}");
                    assert_eq!(event.capture_context.url, None);
                    continue;
                }
                if metadata_only {
                    let event = receiver.try_recv().expect("metadata is immediate");
                    assert_eq!(
                        event.capture_context.url.as_deref(),
                        Some(if matches!(*scenario, "url_changed" | "ordinary_changed") {
                            "https://v2.example/"
                        } else {
                            "https://v1.example/"
                        })
                    );
                    assert!(event.element.and_then(|element| element.value).is_none());
                    continue;
                }
                if *scenario == "to_standalone" {
                    let event = receiver.try_recv().expect("reload suppresses stale body");
                    assert_eq!(event_body(&event), None);
                    continue;
                }
                assert!(receiver.try_recv().is_err(), "body remains quarantined");
                publisher.observe(
                    7,
                    browser_observation(
                        bundle_id,
                        if matches!(*scenario, "url_changed" | "ordinary_changed") {
                            "https://v2.example/"
                        } else {
                            "https://v1.example/"
                        },
                    ),
                );
                output.release_due();

                let event = receiver.try_recv().expect("metadata event is released");
                assert_eq!(
                    event_body(&event),
                    matches!(*scenario, "same" | "ordinary_same").then_some("private"),
                    "{kind}, {scenario}"
                );
                assert_eq!(
                    event.capture_context.url.as_deref(),
                    Some("https://v1.example/")
                );
            }
        }
    }

    fn event_body(event: &RawEvent) -> Option<&str> {
        match &event.data {
            EventData::UiValue(data) => data.text.as_deref(),
            _ => event
                .element
                .as_ref()
                .and_then(|element| element.value.as_deref()),
        }
    }

    fn standalone_filter(bundle_id: &str) -> FilterConfig {
        let mut filter = FilterConfig::default();
        if bundle_id == SAFARI_BUNDLE_ID {
            filter.text_content.exclude_apps.clear();
        }
        filter
    }

    fn browser_observation(bundle_id: &str, url: &str) -> ChromeEligibilityObservation {
        if bundle_id == SAFARI_BUNDLE_ID {
            ChromeEligibilityObservation::Safari {
                window_id: Some(11),
                url: Some(url.to_owned()),
            }
        } else {
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: url.to_owned(),
            }
        }
    }

    fn safari_filter() -> FilterConfig {
        let mut filter = FilterConfig {
            capture_policy: Some(CapturePolicyConfig {
                allowed_apps: Some(vec!["Safari".to_owned()]),
                browser: BrowserPolicy {
                    mode: BrowserMode::AllSites,
                    default_policy: PolicyAction::Allow,
                    on_url_unavailable: PolicyAction::Block,
                    block_auth: false,
                    block_payments: false,
                    allow_list: Vec::new(),
                    block_list: Vec::new(),
                },
                ide: IdePolicy {
                    block_env_files: false,
                    on_file_name_unavailable: PolicyAction::Allow,
                },
            }),
            ..FilterConfig::default()
        };
        filter.text_content.exclude_apps.clear();
        filter
    }
}
