use super::*;
use zanei_collector::Collector;

#[test]
fn no_observation_happens_without_a_trigger_for_five_simulated_seconds() {
    let started_at = Instant::now();
    let mut api = FakeApi::new([]);
    let (sender, _) = sync_channel(1);
    let (eligibility, _) = chrome_eligibility_channel(FilterConfig::default());
    let mut state = worker_state(7);

    assert!(service_on_demand(
        started_at + Duration::from_secs(5),
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));
    assert_eq!(api.query_count, 0);
}

#[test]
fn s21_front_window_result_is_unavailable_when_focus_generation_changes() {
    let focus_context = FocusContext::new();
    focus_context.activate(chrome_app(), Some(chrome_focus(7).window.expect("window")));
    let initial = Instant::now();
    let observed_at = initial + Duration::from_millis(1);
    let (eligibility, tracker) = chrome_eligibility_channel(FilterConfig::default());
    eligibility.observe_at(
        42,
        ChromeEligibilityObservation::Normal {
            window_id: Some(7),
            url: "https://allowed.example/before".to_owned(),
        },
        initial,
    );
    let mut api = FocusChangingApi {
        focus_context: focus_context.clone(),
    };
    let (sender, _) = sync_channel(1);
    let stop = AtomicBool::new(false);
    let metrics = ChromeMetrics::default();
    let context = ObservationContext {
        sender: &sender,
        stop: &stop,
        focus_context: &focus_context,
        metrics: &metrics,
        eligibility: &eligibility,
    };

    let outcome = observe_query_once(
        &mut api,
        &mut NavigationTracker::default(),
        Some(&chrome_app()),
        ChromeQuery::FrontWindow {
            pid: 42,
            window_id: Some(7),
        },
        false,
        observed_at,
        &context,
    );

    assert!(matches!(outcome, ObservationOutcome::Continue));
    assert_eq!(tracker.state_version(42, 7), None);
    assert_eq!(tracker.observed_at(42, 7), Some(observed_at));
    assert!(!tracker.allows_text(42, Some(7)));
    assert_eq!(tracker.state_version(42, 8), None);
}

#[test]
fn ownership_stop_discards_a_blocking_query_result() {
    let stop = AtomicBool::new(false);
    let focus_context = FocusContext::new();
    let initial = Instant::now();
    let observed_at = initial + Duration::from_millis(1);
    let (eligibility, tracker) = chrome_eligibility_channel(FilterConfig::default());
    eligibility.observe_at(
        42,
        ChromeEligibilityObservation::Normal {
            window_id: Some(7),
            url: "https://allowed.example/before".to_owned(),
        },
        initial,
    );
    let (sender, _) = sync_channel(1);
    let metrics = ChromeMetrics::default();
    let context = ObservationContext {
        sender: &sender,
        stop: &stop,
        focus_context: &focus_context,
        metrics: &metrics,
        eligibility: &eligibility,
    };
    let mut api = StopDuringQuery { stop: &stop };

    let outcome = observe_query_once(
        &mut api,
        &mut NavigationTracker::default(),
        None,
        ChromeQuery::Window {
            pid: 42,
            window_id: 7,
            applescript_window_id: "window-101".to_owned(),
        },
        false,
        observed_at,
        &context,
    );

    assert!(matches!(outcome, ObservationOutcome::Stop));
    assert_eq!(tracker.observed_at(42, 7), Some(initial));
    assert_eq!(eligibility.applescript_window_id(42, 7), None);
}

#[test]
fn ownership_as_id_survives_worker_state_restart() {
    let now = Instant::now();
    let (eligibility, _) = chrome_eligibility_channel(FilterConfig::default());
    let snapshot = ChromeSnapshot::from_native(
        crate::ffi::applescript::Snapshot {
            window_key: "window-alpha-001".to_owned(),
            window_title: None,
            tab_key: "tab-alpha-001".to_owned(),
            url: "https://allowed.example".to_owned(),
            tab_title: None,
        },
        Some(7),
    );
    assert_eq!(snapshot.applescript_window_id, "window-alpha-001");
    assert_eq!(snapshot.tab_key, "tab-alpha-001");
    let mut initial_api = FakeApi::new([Ok(ChromeObservation::Snapshot(snapshot))]);
    let (initial_sender, _initial_events) = sync_channel(1);
    assert!(matches!(
        observe_once(
            &mut initial_api,
            &mut NavigationTracker::default(),
            &chrome_app(),
            &initial_sender,
            &ChromeMetrics::default(),
            &eligibility,
        ),
        ObservationOutcome::Continue
    ));
    assert_eq!(
        eligibility.applescript_window_id(42, 7).as_deref(),
        Some("window-alpha-001")
    );

    eligibility.clear_all();
    let mut state = ChromeWorkerState::default();
    let mut api = FakeApi::new([Ok(ChromeObservation::NoWindow)]);
    let (sender, _) = sync_channel(1);

    assert!(handle_observation_trigger(
        ObservationTrigger::OnDemand {
            pid: 42,
            window_id: 7,
        },
        now,
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));
    assert!(service_on_demand(
        now + Duration::from_millis(200),
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));

    assert_eq!(
        api.queries,
        [ChromeQuery::Window {
            pid: 42,
            window_id: 7,
            applescript_window_id: "window-alpha-001".to_owned(),
        }]
    );
}

#[test]
fn v2_5_worker_panic_clears_state_and_preserves_receivers_for_restart() {
    let now = Instant::now();
    let focus_context = FocusContext::new();
    let observer = ChromeObserver::new();
    let (eligibility, tracker) = chrome_eligibility_channel(FilterConfig::default());
    eligibility.observe_at(
        42,
        ChromeEligibilityObservation::Normal {
            window_id: Some(7),
            url: "https://allowed.example".to_owned(),
        },
        now,
    );
    let mut collector = ChromeCollector::new(eligibility, focus_context, observer);
    let failure = ChromeFailure::Query(ChromeQueryFailure::AppleEvent(-1712));
    collector.metrics.failure.observe_failure(failure);
    let (output, _events) = sync_channel(1);
    collector.panic_next_worker_for_test();

    collector
        .start(output.clone())
        .expect("start injected panic worker");
    collector.stop();

    assert_eq!(tracker.state_version(42, 7), None);
    assert_eq!(
        collector.failure_state(),
        ChromeFailureState::Unavailable(failure)
    );
    collector.panic_next_worker_for_test();
    collector
        .start(output)
        .expect("restart with recovered receivers");
    collector.stop();
}

#[test]
fn s23_startup_generation_is_observed_once() {
    let transition = FocusTransition {
        previous: None,
        current: Some(chrome_focus(7)),
        resynced: false,
    };
    let mut api = FakeApi::new([
        Ok(ChromeObservation::NoWindow),
        Ok(ChromeObservation::NoWindow),
    ]);
    let (sender, _) = sync_channel(1);
    let (eligibility, _) = chrome_eligibility_channel(FilterConfig::default());
    let mut state = ChromeWorkerState::default();

    assert!(handle_focus_transition(
        transition.clone(),
        Instant::now(),
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));
    assert!(handle_focus_transition(
        transition,
        Instant::now(),
        &mut api,
        &sender,
        &mut state,
        &ChromeMetrics::default(),
        &eligibility,
    ));

    assert_eq!(api.query_count, 1);
}

#[test]
fn parse_and_validation_failures_recover_only_after_a_valid_snapshot() {
    let (eligibility, capture) = chrome_eligibility_channel(FilterConfig::default());
    eligibility.observe(
        42,
        ChromeEligibilityObservation::Normal {
            window_id: Some(7),
            url: "https://allowed.example/before".to_owned(),
        },
    );
    let parse_failure =
        ChromeFailure::from(crate::ffi::applescript::AppleScriptError::InvalidResponse(
            crate::ffi::applescript::AppleScriptResponseError::UnknownStatus,
        ));
    let mut api = FakeApi::new([
        Err(parse_failure),
        Ok(ChromeObservation::Snapshot(snapshot(
            "",
            "tab-invalid",
            "https://allowed.example/invalid",
            "Invalid",
        ))),
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "opaque-window-id",
            "opaque-tab-id",
            "https://allowed.example/recovered",
            "Recovered",
        ))),
    ]);
    let mut navigation = tracker_with_initial_snapshot();
    let (sender, events) = sync_channel(3);
    let metrics = ChromeMetrics::default();

    assert!(matches!(
        observe_once(
            &mut api,
            &mut navigation,
            &chrome_app(),
            &sender,
            &metrics,
            &eligibility,
        ),
        ObservationOutcome::Continue
    ));
    assert_eq!(
        metrics.failure.state(),
        ChromeFailureState::Unavailable(parse_failure)
    );
    assert!(!capture.allows_text(42, Some(7)));

    assert!(matches!(
        observe_once(
            &mut api,
            &mut navigation,
            &chrome_app(),
            &sender,
            &metrics,
            &eligibility,
        ),
        ObservationOutcome::Continue
    ));
    assert_eq!(
        metrics.failure.state(),
        ChromeFailureState::Unavailable(ChromeFailure::Validation(
            ChromeValidationFailure::EmptyWindowIdentity
        ))
    );
    assert!(!capture.allows_text(42, Some(7)));

    assert!(matches!(
        observe_once(
            &mut api,
            &mut navigation,
            &chrome_app(),
            &sender,
            &metrics,
            &eligibility,
        ),
        ObservationOutcome::Continue
    ));
    assert_eq!(metrics.failure.state(), ChromeFailureState::Available);
    assert!(capture.allows_text(42, Some(7)));
    assert_eq!(metrics.degraded.load(Ordering::Relaxed), 2);
    let event = events.try_recv().expect("recovery navigation");
    let EventData::BrowserNavigate(data) = event.data else {
        panic!("browser navigation");
    };
    assert_eq!(data.transition, None);
}

#[test]
fn output_disconnect_remains_a_structural_worker_stop() {
    let failure = ChromeFailure::from(crate::ffi::applescript::AppleScriptError::Execute {
        code: Some(-1712),
    });
    assert_eq!(
        failure,
        ChromeFailure::Query(ChromeQueryFailure::AppleEvent(-1712))
    );
    let mut api = FakeApi::new([
        Err(failure),
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-7",
            "tab-7",
            "https://allowed.example",
            "Allowed",
        ))),
    ]);
    let (sender, receiver) = sync_channel(1);
    drop(receiver);
    let metrics = ChromeMetrics::default();
    let (eligibility, _) = chrome_eligibility_channel(FilterConfig::default());
    let focus_context = FocusContext::new();
    let focus = focus_context.subscribe();
    focus_context.activate(chrome_app(), chrome_focus(7).window);
    let observer = ChromeObserver::new();
    let observations = observer.subscribe();
    observer.page_loaded(42);

    run_worker(
        &mut api,
        &ChromeWorkerReceivers {
            focus: &focus,
            observations: &observations,
            focus_context: &focus_context,
        },
        &sender,
        &AtomicBool::new(false),
        &metrics,
        &eligibility,
        None,
    );

    assert_eq!(api.query_count, 2);
    assert_eq!(metrics.failure.state(), ChromeFailureState::Available);
    assert_eq!(metrics.degraded.load(Ordering::Relaxed), 2);
    assert_eq!(metrics.dropped.load(Ordering::Relaxed), 1);
}

struct FocusChangingApi {
    focus_context: FocusContext,
}

impl ChromeApi for FocusChangingApi {
    fn query(&mut self, _query: &ChromeQuery) -> Result<ChromeObservation, ChromeFailure> {
        self.focus_context
            .activate(chrome_app(), Some(chrome_focus(8).window.expect("window")));
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-202",
            "tab-2",
            "https://other.example",
            "Other",
        )))
    }
}

struct StopDuringQuery<'a> {
    stop: &'a AtomicBool,
}

impl ChromeApi for StopDuringQuery<'_> {
    fn query(&mut self, _query: &ChromeQuery) -> Result<ChromeObservation, ChromeFailure> {
        self.stop.store(true, Ordering::Release);
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-101",
            "tab-1",
            "https://allowed.example/after",
            "After",
        )))
    }
}
