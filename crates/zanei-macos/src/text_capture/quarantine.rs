//! Confirm-then-write quarantine for Chrome-dependent bodies.

use std::time::{Duration, Instant};

use time::OffsetDateTime;
use zanei_collector::RawEvent;
use zanei_core::privacy::{PrivacyScope, suppress_text_content};

use crate::{capture_policy::CapturePolicy, chrome::ChromeObserver};

const CONFIRMATION_SAFETY_CAP: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChromeWindowKey {
    pub(crate) pid: i64,
    pub(crate) window_id: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeldBodyKind {
    Text,
    Snapshot { hash: u64 },
}

pub(crate) struct ReleasedEvent {
    event: RawEvent,
    snapshot_hash: Option<(ChromeWindowKey, u64)>,
}

impl ReleasedEvent {
    pub(crate) fn into_parts(self) -> (RawEvent, Option<(ChromeWindowKey, u64)>) {
        (self.event, self.snapshot_hash)
    }
}

impl std::ops::Deref for ReleasedEvent {
    type Target = RawEvent;

    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

pub(crate) struct TextQuarantine {
    held: Vec<HeldEvent>,
    observer: ChromeObserver,
}

struct HeldEvent {
    event: RawEvent,
    key: ChromeWindowKey,
    version_at_decision: u64,
    held_at: Instant,
    expires_at: Instant,
    kind: HeldBodyKind,
}

impl TextQuarantine {
    pub(crate) fn new(observer: ChromeObserver) -> Self {
        Self {
            held: Vec::new(),
            observer,
        }
    }

    pub(crate) fn hold_text(
        &mut self,
        mut event: RawEvent,
        key: ChromeWindowKey,
        version_at_decision: u64,
        observed_at: OffsetDateTime,
    ) {
        event.observed_at = Some(observed_at);
        self.hold_at(
            event,
            key,
            version_at_decision,
            HeldBodyKind::Text,
            Instant::now(),
        );
    }

    pub(crate) fn hold_snapshot(
        &mut self,
        event: RawEvent,
        key: ChromeWindowKey,
        version_at_decision: u64,
        hash: u64,
        held_at: Instant,
    ) {
        self.hold_at(
            event,
            key,
            version_at_decision,
            HeldBodyKind::Snapshot { hash },
            held_at,
        );
    }

    fn hold_at(
        &mut self,
        event: RawEvent,
        key: ChromeWindowKey,
        version_at_decision: u64,
        kind: HeldBodyKind,
        held_at: Instant,
    ) {
        self.observer.request_observation(key.pid, key.window_id);
        self.held.push(HeldEvent {
            event,
            key,
            version_at_decision,
            held_at,
            expires_at: held_at + CONFIRMATION_SAFETY_CAP,
            kind,
        });
    }

    pub(crate) fn release(&mut self, now: Instant, policy: &CapturePolicy) -> Vec<ReleasedEvent> {
        let tracker = policy.chrome_tracker();
        let mut released = Vec::new();
        let mut pending = Vec::with_capacity(self.held.len());
        for held in self.held.drain(..) {
            if tracker
                .observed_at(held.key.pid, held.key.window_id)
                .is_some_and(|observed_at| observed_at > held.held_at)
            {
                released.extend(resolve_confirmed(held, policy));
            } else if now >= held.expires_at {
                released.extend(resolve_unresponsive(held));
            } else {
                self.observer
                    .request_observation(held.key.pid, held.key.window_id);
                pending.push(held);
            }
        }
        self.held = pending;
        released
    }

    pub(crate) fn flush(&mut self) -> Vec<ReleasedEvent> {
        self.held
            .drain(..)
            .filter_map(resolve_unresponsive)
            .collect()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

fn resolve_confirmed(mut held: HeldEvent, policy: &CapturePolicy) -> Option<ReleasedEvent> {
    let scope = match held.kind {
        HeldBodyKind::Text => PrivacyScope::TextContent,
        HeldBodyKind::Snapshot { .. } => PrivacyScope::ContentSnapshot,
    };
    let decision = policy.decision(scope, &held.event.app, Some(held.key.window_id));
    held.event.capture_context = decision.capture_context();
    if !decision.is_allowed() && matches!(held.kind, HeldBodyKind::Snapshot { .. }) {
        return drop_snapshot(&held, "denied");
    }
    let unchanged = decision.chrome_version() == Some(held.version_at_decision);
    if !unchanged && matches!(held.kind, HeldBodyKind::Snapshot { .. }) {
        return drop_snapshot(&held, "version_changed");
    }
    if !decision.is_allowed() || !unchanged {
        suppress_text_content(&mut held.event.data, &mut held.event.element);
    }
    Some(released(held))
}

fn resolve_unresponsive(mut held: HeldEvent) -> Option<ReleasedEvent> {
    if matches!(held.kind, HeldBodyKind::Snapshot { .. }) {
        return drop_snapshot(&held, "unresponsive");
    }
    suppress_text_content(&mut held.event.data, &mut held.event.element);
    Some(released(held))
}

fn released(held: HeldEvent) -> ReleasedEvent {
    let snapshot_hash = match held.kind {
        HeldBodyKind::Text => None,
        HeldBodyKind::Snapshot { hash } => Some((held.key, hash)),
    };
    ReleasedEvent {
        event: held.event,
        snapshot_hash,
    }
}

fn drop_snapshot(held: &HeldEvent, reason: &str) -> Option<ReleasedEvent> {
    crate::trace::trace!(
        "component=content_snapshot gate=unconfirmed action=drop reason={} pid={} window_id={}",
        reason,
        held.key.pid,
        held.key.window_id
    );
    None
}

#[cfg(test)]
mod tests {
    use zanei_core::{
        config::{FilterConfig, ScopedFilterConfig},
        privacy::CHROME_BUNDLE_ID,
        schema::{
            App, ContentSnapshotData, ContentSnapshotTrigger, EventData, FieldKind, InputKeyData,
            InputKeyKind, Window,
        },
    };

    use super::*;
    use crate::chrome::{
        ChromeEligibilityObservation, ChromeEligibilityPublisher, ChromeEligibilityTracker,
        chrome_eligibility_channel,
    };

    fn setup() -> (
        ChromeEligibilityPublisher,
        ChromeEligibilityTracker,
        CapturePolicy,
        ChromeObserver,
    ) {
        let filter = FilterConfig {
            text_content: ScopedFilterConfig {
                exclude_websites: vec!["denied.example".to_owned()],
                ..ScopedFilterConfig::default()
            },
            content_snapshot: ScopedFilterConfig {
                exclude_websites: vec!["denied.example".to_owned()],
                ..ScopedFilterConfig::default()
            },
            ..FilterConfig::default()
        };
        let (publisher, tracker) = chrome_eligibility_channel(filter.clone());
        let policy = CapturePolicy::new(tracker.clone(), filter, None);
        (publisher, tracker, policy, ChromeObserver::new())
    }

    fn observe(publisher: &ChromeEligibilityPublisher, url: &str, at: Instant) {
        publisher.observe_at(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: url.to_owned(),
            },
            at,
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

    fn snapshot_event() -> RawEvent {
        let mut event = event(OffsetDateTime::UNIX_EPOCH);
        event.source = "macos.ax".to_owned();
        event.event_type = "content.snapshot".to_owned();
        event.data = EventData::ContentSnapshot(ContentSnapshotData::new(
            Some("private snapshot".to_owned()),
            16,
            None,
            ContentSnapshotTrigger::FocusOut,
        ));
        event
    }

    #[test]
    fn allowed_to_excluded_switch_then_input_within_200ms_nulls_text() {
        let (publisher, tracker, policy, observer) = setup();
        let before = Instant::now();
        observe(&publisher, "https://allowed.example", before);
        let version = tracker.state_version(7, 11).expect("version");
        let held_at = before + Duration::from_millis(10);
        let mut quarantine = TextQuarantine::new(observer);
        quarantine.hold_at(
            event(OffsetDateTime::UNIX_EPOCH),
            ChromeWindowKey {
                pid: 7,
                window_id: 11,
            },
            version,
            HeldBodyKind::Text,
            held_at,
        );
        observe(
            &publisher,
            "https://denied.example",
            held_at + Duration::from_millis(50),
        );

        let released = quarantine.release(held_at + Duration::from_millis(51), &policy);
        let EventData::InputKey(data) = &released[0].data else {
            panic!("input.key");
        };
        assert_eq!(data.text, None);
    }

    #[test]
    fn post_input_unchanged_observation_keeps_text_and_input_time() {
        let (publisher, tracker, policy, observer) = setup();
        let before = Instant::now();
        observe(&publisher, "https://allowed.example", before);
        let version = tracker.state_version(7, 11).expect("version");
        let held_at = before + Duration::from_millis(10);
        let observed_at = OffsetDateTime::UNIX_EPOCH;
        let mut quarantine = TextQuarantine::new(observer);
        quarantine.hold_at(
            event(observed_at),
            ChromeWindowKey {
                pid: 7,
                window_id: 11,
            },
            version,
            HeldBodyKind::Text,
            held_at,
        );
        observe(
            &publisher,
            "https://allowed.example",
            held_at + Duration::from_millis(50),
        );

        let released = quarantine.release(held_at + Duration::from_millis(51), &policy);
        let EventData::InputKey(data) = &released[0].data else {
            panic!("input.key");
        };
        assert_eq!(data.text.as_deref(), Some("private"));
        assert_eq!(released[0].observed_at, Some(observed_at));
    }

    #[test]
    fn unresponsive_chrome_nulls_after_safety_cap() {
        let (publisher, tracker, policy, observer) = setup();
        let before = Instant::now();
        observe(&publisher, "https://allowed.example", before);
        let version = tracker.state_version(7, 11).expect("version");
        let held_at = before + Duration::from_millis(10);
        let mut quarantine = TextQuarantine::new(observer);
        quarantine.hold_at(
            event(OffsetDateTime::UNIX_EPOCH),
            ChromeWindowKey {
                pid: 7,
                window_id: 11,
            },
            version,
            HeldBodyKind::Text,
            held_at,
        );

        assert!(
            quarantine
                .release(held_at + Duration::from_secs(1), &policy)
                .is_empty()
        );
        let released = quarantine.release(held_at + CONFIRMATION_SAFETY_CAP, &policy);
        let EventData::InputKey(data) = &released[0].data else {
            panic!("input.key");
        };
        assert_eq!(data.text, None);
        assert!(quarantine.is_empty());
    }

    #[test]
    fn focus_out_snapshot_is_dropped_after_confirming_an_excluded_tab() {
        let (publisher, tracker, policy, observer) = setup();
        let before = Instant::now();
        observe(&publisher, "https://allowed.example", before);
        let version = tracker.state_version(7, 11).expect("version");
        let held_at = before + Duration::from_millis(10);
        let mut quarantine = TextQuarantine::new(observer);
        quarantine.hold_at(
            snapshot_event(),
            ChromeWindowKey {
                pid: 7,
                window_id: 11,
            },
            version,
            HeldBodyKind::Snapshot { hash: 1 },
            held_at,
        );
        observe(
            &publisher,
            "https://denied.example",
            held_at + Duration::from_millis(50),
        );

        assert!(
            quarantine
                .release(held_at + Duration::from_millis(51), &policy)
                .is_empty()
        );
        assert!(quarantine.is_empty());
    }

    #[test]
    fn unresponsive_chrome_drops_the_snapshot() {
        let (publisher, tracker, policy, observer) = setup();
        let before = Instant::now();
        observe(&publisher, "https://allowed.example", before);
        let version = tracker.state_version(7, 11).expect("version");
        let held_at = before + Duration::from_millis(10);
        let mut quarantine = TextQuarantine::new(observer);
        quarantine.hold_at(
            snapshot_event(),
            ChromeWindowKey {
                pid: 7,
                window_id: 11,
            },
            version,
            HeldBodyKind::Snapshot { hash: 1 },
            held_at,
        );

        assert!(
            quarantine
                .release(held_at + CONFIRMATION_SAFETY_CAP, &policy)
                .is_empty()
        );
        assert!(quarantine.is_empty());
    }

    #[test]
    fn changed_chrome_state_version_drops_the_snapshot() {
        let (publisher, tracker, policy, observer) = setup();
        let before = Instant::now();
        observe(&publisher, "https://allowed.example", before);
        let version = tracker.state_version(7, 11).expect("version");
        let held_at = before + Duration::from_millis(10);
        let mut quarantine = TextQuarantine::new(observer);
        quarantine.hold_at(
            snapshot_event(),
            ChromeWindowKey {
                pid: 7,
                window_id: 11,
            },
            version,
            HeldBodyKind::Snapshot { hash: 1 },
            held_at,
        );
        observe(
            &publisher,
            "https://changed.example",
            held_at + Duration::from_millis(50),
        );
        assert_ne!(tracker.state_version(7, 11), Some(version));

        assert!(
            quarantine
                .release(held_at + Duration::from_millis(51), &policy)
                .is_empty()
        );
        assert!(quarantine.is_empty());
    }
}
