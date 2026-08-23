use super::*;

#[test]
fn focus_out_confirmation_targets_background_window_and_releases_body() {
    let held_at = Instant::now() - Duration::from_secs(1);
    let mut api = FakeApi::new([
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            101,
            "tab-1",
            "https://first.example",
            "First",
        ))),
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            101,
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
            applescript_window_id: 101,
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
fn targeted_confirmation_drops_snapshot_when_window_closed() {
    let held_at = Instant::now() - Duration::from_secs(1);
    let mut api = FakeApi::new([
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            101,
            "tab-1",
            "https://first.example",
            "First",
        ))),
        Ok(ChromeObservation::NoWindow),
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
    assert_eq!(tracker.state_version(42, 7), None);
    assert!(quarantine.release(Instant::now(), &policy).is_empty());
}

#[test]
fn intra_app_focus_out_confirmation_targets_previous_window() {
    let held_at = Instant::now() - Duration::from_secs(1);
    let mut api = FakeApi::new([
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            101,
            "tab-1",
            "https://first.example",
            "First",
        ))),
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            8,
            202,
            "tab-2",
            "https://second.example",
            "Second",
        ))),
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            101,
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
            applescript_window_id: 101,
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

fn assert_snapshot_body(event: &zanei_collector::RawEvent, expected: &str) {
    let EventData::ContentSnapshot(data) = &event.data else {
        panic!("content.snapshot");
    };
    assert_eq!(data.text.as_deref(), Some(expected));
}
