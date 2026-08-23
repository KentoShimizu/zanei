//! Delayed confirmation for Chrome text bodies observed between AppleScript polls.

use time::OffsetDateTime;
use zanei_collector::RawEvent;
use zanei_core::privacy::suppress_text_content;

use crate::chrome::ChromeEligibilityTracker;

const POLL_LAG_MARGIN: time::Duration = time::Duration::milliseconds(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChromeWindowKey {
    pub(crate) pid: i64,
    pub(crate) window_id: i64,
}

pub(crate) struct TextQuarantine {
    held: Vec<HeldEvent>,
    confirmation_delay: time::Duration,
}

struct HeldEvent {
    event: RawEvent,
    key: ChromeWindowKey,
    version_at_decision: u64,
    due_at: OffsetDateTime,
}

impl TextQuarantine {
    pub(crate) fn new(tracker: &ChromeEligibilityTracker) -> Self {
        let poll_interval = time::Duration::try_from(tracker.poll_interval())
            .expect("Chrome poll interval must fit time::Duration");
        Self {
            held: Vec::new(),
            confirmation_delay: poll_interval + POLL_LAG_MARGIN,
        }
    }

    pub(crate) fn hold(
        &mut self,
        mut event: RawEvent,
        key: ChromeWindowKey,
        version_at_decision: u64,
        observed_at: OffsetDateTime,
    ) {
        event.observed_at = Some(observed_at);
        self.held.push(HeldEvent {
            event,
            key,
            version_at_decision,
            due_at: observed_at + self.confirmation_delay,
        });
    }

    pub(crate) fn release(
        &mut self,
        now: OffsetDateTime,
        tracker: &ChromeEligibilityTracker,
    ) -> Vec<RawEvent> {
        let mut due = Vec::new();
        let mut pending = Vec::with_capacity(self.held.len());
        for held in self.held.drain(..) {
            if held.due_at <= now {
                due.push(resolve(held, tracker));
            } else {
                pending.push(held);
            }
        }
        self.held = pending;
        due
    }

    pub(crate) fn flush(&mut self, tracker: &ChromeEligibilityTracker) -> Vec<RawEvent> {
        self.held
            .drain(..)
            .map(|held| resolve(held, tracker))
            .collect()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

fn resolve(mut held: HeldEvent, tracker: &ChromeEligibilityTracker) -> RawEvent {
    let unchanged =
        tracker.state_version(held.key.pid, held.key.window_id) == Some(held.version_at_decision);
    if !unchanged || !tracker.allows_text(held.key.pid, Some(held.key.window_id)) {
        suppress_text_content(&mut held.event.data, &mut held.event.element);
    }
    held.event
}

#[cfg(test)]
mod tests {
    use zanei_core::{
        config::{FilterConfig, ScopedFilterConfig},
        privacy::CHROME_BUNDLE_ID,
        schema::{App, EventData, FieldKind, InputKeyData, InputKeyKind, Window},
    };

    use super::*;
    use crate::chrome::{
        ChromeEligibilityObservation, ChromeEligibilityPublisher, chrome_eligibility_channel,
    };

    fn setup() -> (ChromeEligibilityPublisher, ChromeEligibilityTracker) {
        let filter = FilterConfig {
            text_content: ScopedFilterConfig {
                exclude_websites: vec!["denied.example".to_owned()],
                ..ScopedFilterConfig::default()
            },
            ..FilterConfig::default()
        };
        chrome_eligibility_channel(filter)
    }

    fn allow(publisher: &ChromeEligibilityPublisher, url: &str) {
        publisher.observe(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: url.to_owned(),
            },
        );
    }

    fn event(observed_at: OffsetDateTime) -> RawEvent {
        RawEvent {
            observed_at: Some(observed_at),
            source: "macos.eventtap".to_owned(),
            event_type: "input.key".to_owned(),
            app: App {
                name: "Google Chrome".to_owned(),
                bundle_id: Some(CHROME_BUNDLE_ID.to_owned()),
                pid: Some(7),
            },
            window: Some(Window {
                title: Some("Window".to_owned()),
                id: Some(11),
            }),
            element: None,
            data: EventData::InputKey(InputKeyData {
                kind: InputKeyKind::Text,
                modifiers: Vec::new(),
                combo: None,
                text: Some("private".to_owned()),
                field_kind: Some(FieldKind::Text),
                count: 1,
            }),
            capture_context: Default::default(),
        }
    }

    #[test]
    fn allowed_host_changing_to_denied_before_due_time_nulls_text() {
        let (publisher, tracker) = setup();
        allow(&publisher, "https://allowed.example");
        let version = tracker.state_version(7, 11).expect("version");
        let observed_at = OffsetDateTime::UNIX_EPOCH;
        let mut quarantine = TextQuarantine::new(&tracker);
        quarantine.hold(
            event(observed_at),
            ChromeWindowKey {
                pid: 7,
                window_id: 11,
            },
            version,
            observed_at,
        );

        allow(&publisher, "https://denied.example");
        assert!(!tracker.allows_text(7, Some(11)));
        let released = quarantine.release(observed_at + time::Duration::seconds(2), &tracker);

        let EventData::InputKey(data) = &released[0].data else {
            panic!("input.key");
        };
        assert_eq!(data.text, None);
    }

    #[test]
    fn unchanged_allowed_state_keeps_text_and_observation_time() {
        let (publisher, tracker) = setup();
        allow(&publisher, "https://allowed.example");
        let version = tracker.state_version(7, 11).expect("version");
        let observed_at = OffsetDateTime::UNIX_EPOCH;
        let mut quarantine = TextQuarantine::new(&tracker);
        quarantine.hold(
            event(observed_at),
            ChromeWindowKey {
                pid: 7,
                window_id: 11,
            },
            version,
            observed_at,
        );

        let released = quarantine.release(observed_at + time::Duration::seconds(2), &tracker);

        let EventData::InputKey(data) = &released[0].data else {
            panic!("input.key");
        };
        assert_eq!(data.text.as_deref(), Some("private"));
        assert_eq!(released[0].observed_at, Some(observed_at));

        let mut normalizer = zanei_core::normalize::Normalizer::new();
        normalizer
            .push(released[0].clone())
            .expect("released event normalizes");
        let normalized = normalizer.flush();
        assert_eq!(
            normalized[0].ts,
            zanei_core::normalize::format_timestamp(observed_at)
        );
    }

    #[test]
    fn denied_decisions_are_not_held_by_the_caller() {
        let (publisher, tracker) = setup();
        allow(&publisher, "https://denied.example");
        let decision_version = tracker
            .state_version(7, 11)
            .expect("denied state is versioned");
        let observed_at = OffsetDateTime::UNIX_EPOCH;
        let mut quarantine = TextQuarantine::new(&tracker);

        if tracker.allows_text(7, Some(11)) {
            quarantine.hold(
                event(observed_at),
                ChromeWindowKey {
                    pid: 7,
                    window_id: 11,
                },
                decision_version,
                observed_at,
            );
        }

        assert!(quarantine.is_empty());
    }

    #[test]
    fn stop_flush_rechecks_current_state() {
        let (publisher, tracker) = setup();
        allow(&publisher, "https://allowed.example");
        let version = tracker.state_version(7, 11).expect("version");
        let observed_at = OffsetDateTime::UNIX_EPOCH;
        let mut quarantine = TextQuarantine::new(&tracker);
        quarantine.hold(
            event(observed_at),
            ChromeWindowKey {
                pid: 7,
                window_id: 11,
            },
            version,
            observed_at,
        );
        publisher.observe(7, ChromeEligibilityObservation::Unavailable);

        let flushed = quarantine.flush(&tracker);

        let EventData::InputKey(data) = &flushed[0].data else {
            panic!("input.key");
        };
        assert_eq!(data.text, None);
    }
}
