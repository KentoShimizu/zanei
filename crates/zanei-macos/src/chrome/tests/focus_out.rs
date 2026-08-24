use super::*;

#[test]
fn focus_out_confirmation_targets_background_window_and_releases_body() {
    let held_at = Instant::now() - Duration::from_secs(1);
    let mut api = FakeApi::new([
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-101",
            "tab-1",
            "https://first.example",
            "First",
        ))),
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-101",
            "tab-1",
            "https://first.example",
            "First",
        ))),
    ]);
    let (sender, events) = sync_channel(4);
    let filter = FilterConfig::default();
    let (eligibility, tracker) = chrome_eligibility_channel(filter.clone());
    let policy = CapturePolicy::new(tracker.clone(), filter, None);
    let mut state = ChromeWorkerState::default();

    assert!(handle_focus_transition(
        focus_transition(None, Some(chrome_focus(7))),
        held_at,
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));
    let _ = events.try_recv().expect("initial navigation");
    let version = tracker.state_version(42, 7).expect("Chrome version");
    let observed_at = tracker.observed_at(42, 7).expect("initial observation");
    let mut quarantine = TextQuarantine::new(ChromeObserver::new());
    quarantine.hold_snapshot(
        snapshot_event(7, "focus-out body"),
        ChromeWindowKey {
            pid: 42,
            window_id: 7,
        },
        version,
        77,
        held_at,
    );
    assert!(handle_observation_trigger(
        ObservationTrigger::OnDemand {
            pid: 42,
            window_id: 7,
        },
        held_at,
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));

    assert!(handle_focus_transition(
        focus_transition(Some(chrome_focus(7)), Some(other_focus())),
        held_at + Duration::from_millis(100),
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));
    assert_eq!(tracker.state_version(42, 7), Some(version));
    assert_eq!(tracker.observed_at(42, 7), Some(observed_at));
    assert_eq!(state.on_demand.len(), 1);

    assert!(service_on_demand(
        held_at + Duration::from_millis(200),
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));
    assert_eq!(
        api.queries.last(),
        Some(&ChromeQuery::Window {
            pid: 42,
            window_id: 7,
            applescript_window_id: "window-101".to_owned(),
        })
    );
    assert!(
        events.try_recv().is_err(),
        "confirmation emits no navigation"
    );
    let released = quarantine.release(Instant::now(), &policy);
    assert_eq!(released.len(), 1);
    assert_snapshot_body(&released[0], "focus-out body");
}

#[test]
fn targeted_identity_mismatch_nulls_text_and_drops_snapshot() {
    let held_at = Instant::now() - Duration::from_secs(1);
    let mut api = FakeApi::new([
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-101",
            "tab-1",
            "https://first.example",
            "First",
        ))),
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-202",
            "tab-2",
            "https://first.example",
            "Reused",
        ))),
    ]);
    let (sender, events) = sync_channel(2);
    let filter = FilterConfig::default();
    let (eligibility, tracker) = chrome_eligibility_channel(filter.clone());
    let policy = CapturePolicy::new(tracker.clone(), filter, None);
    let mut state = ChromeWorkerState::default();
    assert!(handle_focus_transition(
        focus_transition(None, Some(chrome_focus(7))),
        held_at,
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));
    let _ = events.try_recv().expect("initial navigation");
    let version = tracker.state_version(42, 7).expect("Chrome version");
    let text_event = input_text_event(7, "reused text");
    let mut text_quarantine = TextQuarantine::new(ChromeObserver::new());
    text_quarantine.hold_text(
        text_event,
        ChromeWindowKey {
            pid: 42,
            window_id: 7,
        },
        version,
        time::OffsetDateTime::UNIX_EPOCH,
    );
    let mut quarantine = TextQuarantine::new(ChromeObserver::new());
    quarantine.hold_snapshot(
        snapshot_event(7, "closed body"),
        ChromeWindowKey {
            pid: 42,
            window_id: 7,
        },
        version,
        88,
        held_at,
    );
    let confirmation_at = Instant::now();
    let metrics = ChromeMetrics::default();
    assert!(handle_observation_trigger(
        ObservationTrigger::OnDemand {
            pid: 42,
            window_id: 7,
        },
        confirmation_at,
        &mut api,
        &sender,
        &mut state,
        &metrics,
        &eligibility,
    ));

    assert!(service_on_demand(
        confirmation_at + Duration::from_millis(200),
        &mut api,
        &sender,
        &mut state,
        &metrics,
        &eligibility,
    ));
    assert_eq!(tracker.state_version(42, 7), None);
    assert_eq!(
        metrics.failure.state(),
        ChromeFailureState::Unavailable(ChromeFailure::Validation(
            ChromeValidationFailure::WindowIdentityMismatch
        ))
    );
    let released = text_quarantine.release(Instant::now(), &policy);
    let EventData::InputKey(data) = &released[0].data else {
        panic!("input.key");
    };
    assert_eq!(data.text, None);
    assert!(quarantine.release(Instant::now(), &policy).is_empty());
}

#[test]
fn window_identity_change_advances_version_and_invalidates_quarantined_bodies() {
    let held_at = Instant::now() - Duration::from_secs(1);
    let mut api = FakeApi::new([
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-a",
            "tab-a",
            "https://same.example/page",
            "Window A",
        ))),
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-b",
            "tab-b",
            "https://same.example/page",
            "Window B",
        ))),
    ]);
    let (sender, events) = sync_channel(2);
    let filter = FilterConfig::default();
    let (eligibility, tracker) = chrome_eligibility_channel(filter.clone());
    let policy = CapturePolicy::new(tracker.clone(), filter, None);
    let mut state = ChromeWorkerState::default();

    assert!(handle_focus_transition(
        focus_transition(None, Some(chrome_focus(7))),
        held_at,
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));
    let _ = events.try_recv().expect("window A navigation");
    let version = tracker.state_version(42, 7).expect("window A version");
    let mut text_quarantine = TextQuarantine::new(ChromeObserver::new());
    text_quarantine.hold_text(
        input_text_event(7, "window A text"),
        ChromeWindowKey {
            pid: 42,
            window_id: 7,
        },
        version,
        time::OffsetDateTime::UNIX_EPOCH,
    );
    let mut snapshot_quarantine = TextQuarantine::new(ChromeObserver::new());
    snapshot_quarantine.hold_snapshot(
        snapshot_event(7, "window A snapshot"),
        ChromeWindowKey {
            pid: 42,
            window_id: 7,
        },
        version,
        89,
        held_at,
    );

    let replacement_observed_at = Instant::now() + Duration::from_millis(1);
    assert!(handle_observation_trigger(
        ObservationTrigger::PageLoaded { pid: 42 },
        replacement_observed_at,
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));

    assert_eq!(
        api.queries.last(),
        Some(&ChromeQuery::FrontWindow {
            pid: 42,
            window_id: Some(7),
        })
    );
    let replacement_version = tracker.state_version(42, 7).expect("window B version");
    assert!(replacement_version > version);
    assert!(tracker.allows_text(42, Some(7)));
    assert!(tracker.allows_snapshot(42, Some(7)));
    let released = text_quarantine.release(Instant::now(), &policy);
    assert_eq!(released.len(), 1);
    let EventData::InputKey(data) = &released[0].data else {
        panic!("input.key");
    };
    assert_eq!(data.text, None);
    assert!(
        snapshot_quarantine
            .release(Instant::now(), &policy)
            .is_empty()
    );
}

#[test]
fn intra_app_focus_out_confirmation_targets_previous_window() {
    let held_at = Instant::now() - Duration::from_secs(1);
    let mut api = FakeApi::new([
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-101",
            "tab-1",
            "https://first.example",
            "First",
        ))),
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            8,
            "window-202",
            "tab-2",
            "https://second.example",
            "Second",
        ))),
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-101",
            "tab-1",
            "https://first.example",
            "First",
        ))),
    ]);
    let (sender, events) = sync_channel(4);
    let filter = FilterConfig::default();
    let (eligibility, tracker) = chrome_eligibility_channel(filter.clone());
    let policy = CapturePolicy::new(tracker.clone(), filter, None);
    let mut state = ChromeWorkerState::default();
    assert!(handle_focus_transition(
        focus_transition(None, Some(chrome_focus(7))),
        held_at,
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));
    let _ = events.try_recv().expect("first navigation");
    let version = tracker.state_version(42, 7).expect("first version");
    let mut quarantine = TextQuarantine::new(ChromeObserver::new());
    quarantine.hold_snapshot(
        snapshot_event(7, "first body"),
        ChromeWindowKey {
            pid: 42,
            window_id: 7,
        },
        version,
        99,
        held_at,
    );

    assert!(handle_focus_transition(
        focus_transition(Some(chrome_focus(7)), Some(chrome_focus(8))),
        held_at + Duration::from_millis(100),
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));
    let _ = events.try_recv().expect("second navigation");
    assert_eq!(tracker.state_version(42, 7), Some(version));
    assert!(handle_observation_trigger(
        ObservationTrigger::OnDemand {
            pid: 42,
            window_id: 7,
        },
        held_at,
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));
    assert!(service_on_demand(
        held_at + Duration::from_millis(200),
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));

    assert_eq!(
        api.queries.last(),
        Some(&ChromeQuery::Window {
            pid: 42,
            window_id: 7,
            applescript_window_id: "window-101".to_owned(),
        })
    );
    assert!(
        events.try_recv().is_err(),
        "targeted read emits no navigation"
    );
    let released = quarantine.release(Instant::now(), &policy);
    assert_eq!(released.len(), 1);
    assert_snapshot_body(&released[0], "first body");
}

fn focus_transition(
    previous: Option<crate::focus_context::FocusSnapshot>,
    mut current: Option<crate::focus_context::FocusSnapshot>,
) -> FocusTransition {
    if let (Some(previous), Some(current)) = (&previous, &mut current)
        && current.generation <= previous.generation
    {
        current.generation = previous.generation + 1;
    }
    FocusTransition {
        previous,
        current,
        resynced: false,
    }
}

fn other_focus() -> crate::focus_context::FocusSnapshot {
    crate::focus_context::FocusSnapshot {
        app: ApplicationInfo {
            name: "Other".to_owned(),
            bundle_id: Some("dev.example.Other".to_owned()),
            pid: 99,
            activation_policy: crate::workspace::ApplicationActivationPolicy::Regular,
        },
        window: Some(crate::ffi::window_list::NativeWindow {
            title: Some("Other".to_owned()),
            id: Some(9),
        }),
        generation: 2,
        focused_field: None,
        field_generation: 2,
    }
}

fn snapshot_event(window_id: i64, text: &str) -> zanei_collector::RawEvent {
    zanei_collector::RawEvent {
        observed_at: None,
        source: "macos.ax".to_owned(),
        event_type: "content.snapshot".to_owned(),
        app: App {
            name: "Google Chrome".to_owned(),
            bundle_id: Some(CHROME_BUNDLE_ID.to_owned()),
            pid: Some(42),
        },
        window: Some(Window {
            title: Some("Chrome".to_owned()),
            id: Some(window_id),
        }),
        element: None,
        data: EventData::ContentSnapshot(ContentSnapshotData {
            text: Some(text.to_owned()),
            chars: u64::try_from(text.chars().count()).expect("fixture length"),
            complete: true,
            trigger: ContentSnapshotTrigger::FocusOut,
        }),
        capture_context: Default::default(),
    }
}

fn input_text_event(window_id: i64, text: &str) -> zanei_collector::RawEvent {
    let mut event = snapshot_event(window_id, text);
    event.data = EventData::InputKey(zanei_core::schema::InputKeyData {
        kind: zanei_core::schema::InputKeyKind::Text,
        modifiers: Vec::new(),
        combo: None,
        text: Some(text.to_owned()),
        field_kind: None,
        count: 1,
    });
    event
}

fn assert_snapshot_body(event: &zanei_collector::RawEvent, expected: &str) {
    let EventData::ContentSnapshot(data) = &event.data else {
        panic!("content.snapshot");
    };
    assert_eq!(data.text.as_deref(), Some(expected));
}
