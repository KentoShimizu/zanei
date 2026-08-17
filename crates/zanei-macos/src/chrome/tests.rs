use std::collections::VecDeque;

use zanei_core::config::FilterConfig;

use super::*;

#[test]
fn first_snapshot_emits_once_and_identical_page_does_not_repeat() {
    let mut tracker = NavigationTracker::default();
    let first = tracker
        .observe(snapshot(
            "window-1",
            "tab-1",
            "https://example.com",
            "First",
        ))
        .expect("valid snapshot")
        .expect("first snapshot should emit");
    assert_eq!(first.transition, None);

    let repeated = tracker
        .observe(snapshot(
            "window-1",
            "tab-1",
            "https://example.com",
            "Changed title",
        ))
        .expect("valid snapshot");
    assert!(repeated.is_none());
}

#[test]
fn classifies_url_and_identity_changes() {
    let cases = [
        (
            snapshot("window-1", "tab-1", "https://example.com/next", "Next"),
            BrowserTransition::Navigate,
        ),
        (
            snapshot("window-1", "tab-2", "https://example.com", "Same URL"),
            BrowserTransition::TabSwitch,
        ),
        (
            snapshot("window-2", "tab-1", "https://other.example", "Other"),
            BrowserTransition::TabSwitch,
        ),
        (
            snapshot("window-2", "tab-2", "https://other.example", "Other"),
            BrowserTransition::TabSwitch,
        ),
    ];
    for (changed, expected) in cases {
        let mut tracker = tracker_with_initial_snapshot();
        let navigation = tracker
            .observe(changed)
            .expect("valid snapshot")
            .expect("change should emit");
        assert_eq!(navigation.transition, Some(expected));
    }
}

#[test]
fn incognito_resets_tracker_and_next_normal_snapshot_has_no_transition() {
    let mut tracker = tracker_with_initial_snapshot();
    let mut api = FakeApi::new([Ok(ChromeObservation::Incognito { window_id: Some(7) })]);
    let (sender, receiver) = sync_channel(2);
    let (eligibility, _) = chrome_eligibility_channel(FilterConfig::default());

    let outcome = poll_once(
        &mut api,
        &mut tracker,
        &chrome_app(),
        &sender,
        &ChromeMetrics::default(),
        &eligibility,
    );

    assert!(matches!(outcome, PollOutcome::Continue));
    assert!(tracker.previous.is_none());
    assert!(receiver.try_recv().is_err());
    let navigation = tracker
        .observe(snapshot(
            "window-1",
            "tab-1",
            "https://example.com",
            "Normal again",
        ))
        .expect("valid snapshot")
        .expect("normal snapshot after incognito");
    assert_eq!(navigation.transition, None);
}

#[test]
fn no_window_resets_but_not_frontmost_preserves_the_previous_page() {
    let (sender, _) = sync_channel(1);
    let (eligibility, _) = chrome_eligibility_channel(FilterConfig::default());
    let mut tracker = tracker_with_initial_snapshot();
    let mut background = FakeApi::new([Ok(ChromeObservation::NotFrontmost)]);
    assert!(matches!(
        poll_once(
            &mut background,
            &mut tracker,
            &chrome_app(),
            &sender,
            &ChromeMetrics::default(),
            &eligibility,
        ),
        PollOutcome::Inactive
    ));
    assert!(tracker.previous.is_some());

    let mut no_window = FakeApi::new([Ok(ChromeObservation::NoWindow)]);
    assert!(matches!(
        poll_once(
            &mut no_window,
            &mut tracker,
            &chrome_app(),
            &sender,
            &ChromeMetrics::default(),
            &eligibility,
        ),
        PollOutcome::Continue
    ));
    assert!(tracker.previous.is_none());
}

#[test]
fn rejects_empty_identity_and_non_absolute_url() {
    let mut tracker = NavigationTracker::default();
    assert!(matches!(
        tracker.observe(snapshot("", "tab-1", "https://example.com", "Title")),
        Err(SnapshotError::EmptyWindowIdentity)
    ));
    assert!(matches!(
        tracker.observe(snapshot("window-1", "", "https://example.com", "Title")),
        Err(SnapshotError::EmptyTabIdentity)
    ));
    assert!(matches!(
        tracker.observe(snapshot("window-1", "tab-1", "not a URL", "Title")),
        Err(SnapshotError::InvalidUrl)
    ));
}

#[test]
fn chrome_activation_queries_immediately() {
    let mut api = FakeApi::new([Ok(ChromeObservation::NoWindow)]);
    let (sender, _) = sync_channel(1);
    let mut state = ChromeWorkerState::default();
    let (eligibility, _) = chrome_eligibility_channel(FilterConfig::default());

    assert!(handle_workspace_event(
        WorkspaceEvent::Activated(chrome_app()),
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));
    assert_eq!(api.query_count, 1);
    assert!(state.frontmost.is_some());
    assert!(state.next_poll.is_some());
}

#[test]
fn wake_invalidates_text_eligibility_until_the_next_poll() {
    let mut api = FakeApi::new([]);
    let (sender, _) = sync_channel(1);
    let mut state = ChromeWorkerState {
        navigation: NavigationTracker::default(),
        frontmost: Some(chrome_app()),
        next_poll: Some(Instant::now()),
    };
    let (eligibility, text) = chrome_eligibility_channel(FilterConfig::default());
    eligibility.publish_normal(42, Some(7), "https://example.com");
    assert!(text.allows_text(42, Some(7)));

    assert!(handle_workspace_event(
        WorkspaceEvent::DidWake,
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));

    assert!(!text.allows_text(42, Some(7)));
}

#[test]
fn snapshot_produces_contract_aligned_raw_event() {
    let mut tracker = NavigationTracker::default();
    let mut api = FakeApi::new([Ok(ChromeObservation::Snapshot(snapshot(
        "window-1",
        "tab-1",
        "chrome://settings/privacy",
        "Privacy",
    )))]);
    let (sender, receiver) = sync_channel(1);
    let (eligibility, _) = chrome_eligibility_channel(FilterConfig::default());

    let outcome = poll_once(
        &mut api,
        &mut tracker,
        &chrome_app(),
        &sender,
        &ChromeMetrics::default(),
        &eligibility,
    );
    let event = receiver.try_recv().expect("raw event");

    assert!(matches!(outcome, PollOutcome::Continue));
    assert_eq!(event.source, EVENT_SOURCE);
    assert_eq!(event.event_type, EVENT_TYPE);
    assert_eq!(event.app.pid, Some(42));
    assert_eq!(event.window.expect("window context").id, None);
    assert!(event.element.is_none());
    let EventData::BrowserNavigate(data) = event.data else {
        panic!("expected browser navigation data");
    };
    assert_eq!(data.mode, BrowserMode::Normal);
    assert_eq!(data.transition, None);
    assert_eq!(data.url, "chrome://settings/privacy");
}

#[test]
fn polling_moves_text_eligibility_between_private_excluded_and_normal() {
    let config = FilterConfig {
        exclude_websites: vec!["private.example".to_owned()],
        ..FilterConfig::default()
    };
    let (eligibility, tracker) = chrome_eligibility_channel(config);
    let mut navigation = NavigationTracker::default();
    let mut api = FakeApi::new([
        Ok(ChromeObservation::Incognito { window_id: Some(7) }),
        Ok(ChromeObservation::Snapshot(snapshot(
            "window-1",
            "tab-1",
            "https://private.example",
            "Excluded",
        ))),
        Ok(ChromeObservation::Snapshot(snapshot(
            "window-1",
            "tab-1",
            "https://example.com",
            "Normal",
        ))),
    ]);
    let (sender, _) = sync_channel(3);

    assert!(!tracker.allows_text(42, Some(7)));
    let _ = poll_once(
        &mut api,
        &mut navigation,
        &chrome_app(),
        &sender,
        &ChromeMetrics::default(),
        &eligibility,
    );
    assert!(!tracker.allows_text(42, Some(7)));
    let _ = poll_once(
        &mut api,
        &mut navigation,
        &chrome_app(),
        &sender,
        &ChromeMetrics::default(),
        &eligibility,
    );
    assert!(!tracker.allows_text(42, Some(7)));
    let _ = poll_once(
        &mut api,
        &mut navigation,
        &chrome_app(),
        &sender,
        &ChromeMetrics::default(),
        &eligibility,
    );
    assert!(tracker.allows_text(42, Some(7)));
}

#[test]
fn full_output_queue_is_counted_as_a_dropped_event() {
    let mut tracker = NavigationTracker::default();
    let mut api = FakeApi::new([Ok(ChromeObservation::Snapshot(snapshot(
        "window-1",
        "tab-1",
        "https://example.com",
        "Example",
    )))]);
    let (sender, _receiver) = sync_channel(0);
    let metrics = ChromeMetrics::default();
    let (eligibility, _) = chrome_eligibility_channel(FilterConfig::default());

    let outcome = poll_once(
        &mut api,
        &mut tracker,
        &chrome_app(),
        &sender,
        &metrics,
        &eligibility,
    );

    assert!(matches!(outcome, PollOutcome::Continue));
    assert_eq!(metrics.dropped.load(Ordering::Relaxed), 1);
}

fn tracker_with_initial_snapshot() -> NavigationTracker {
    let mut tracker = NavigationTracker::default();
    tracker
        .observe(snapshot(
            "window-1",
            "tab-1",
            "https://example.com",
            "First",
        ))
        .expect("valid snapshot")
        .expect("initial snapshot");
    tracker
}

fn snapshot(window: &str, tab: &str, url: &str, title: &str) -> ChromeSnapshot {
    ChromeSnapshot {
        window_id: Some(7),
        window_key: window.to_owned(),
        window_title: Some(title.to_owned()),
        tab_key: tab.to_owned(),
        url: url.to_owned(),
        tab_title: Some(title.to_owned()),
    }
}

fn chrome_app() -> ApplicationInfo {
    ApplicationInfo {
        name: "Google Chrome".to_owned(),
        bundle_id: Some(CHROME_BUNDLE_ID.to_owned()),
        pid: 42,
        activation_policy: crate::workspace::ApplicationActivationPolicy::Regular,
    }
}

struct FakeApi {
    observations: VecDeque<Result<ChromeObservation, &'static str>>,
    query_count: usize,
}

impl FakeApi {
    fn new(
        observations: impl IntoIterator<Item = Result<ChromeObservation, &'static str>>,
    ) -> Self {
        Self {
            observations: observations.into_iter().collect(),
            query_count: 0,
        }
    }
}

impl ChromeApi for FakeApi {
    type Error = &'static str;

    fn query(&mut self, _pid: i64) -> Result<ChromeObservation, Self::Error> {
        self.query_count += 1;
        self.observations.pop_front().expect("fake observation")
    }
}
