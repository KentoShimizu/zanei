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
            applescript_window_id: AppleScriptWindowId::for_test("window-101"),
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
fn front_response_identity_is_targeted_instead_of_cg_window_number() {
    let now = Instant::now();
    let (eligibility, _) = chrome_eligibility_channel(FilterConfig::default());
    let snapshot = ChromeSnapshot::from_native(
        crate::ffi::applescript::Snapshot {
            window_id: AppleScriptWindowId::for_test("101"),
            window_title: None,
            tab_key: "tab-alpha-001".to_owned(),
            url: "https://allowed.example".to_owned(),
            tab_title: None,
        },
        Some(7),
    );
    assert_eq!(snapshot.applescript_window_id.as_str(), "101");
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
        eligibility
            .applescript_window_id(42, 7)
            .as_ref()
            .map(AppleScriptWindowId::as_str),
        Some("101")
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
            applescript_window_id: AppleScriptWindowId::for_test("101"),
        }]
    );
}

#[test]
fn cg_window_number_used_as_applescript_identity_fails_closed() {
    let (eligibility, tracker) = chrome_eligibility_channel(FilterConfig::default());
    let (sender, _) = sync_channel(1);
    let stop = AtomicBool::new(false);
    let focus_context = FocusContext::new();
    let metrics = ChromeMetrics::default();
    let context = ObservationContext {
        sender: &sender,
        stop: &stop,
        focus_context: &focus_context,
        metrics: &metrics,
        eligibility: &eligibility,
    };
    let mut api = FakeApi::new([Ok(ChromeObservation::Snapshot(snapshot_for_window(
        7,
        "101",
        "tab-1",
        "https://allowed.example",
        "Allowed",
    )))]);

    let outcome = observe_query_once(
        &mut api,
        &mut NavigationTracker::default(),
        None,
        ChromeQuery::Window {
            pid: 42,
            window_id: 7,
            applescript_window_id: AppleScriptWindowId::for_test("7"),
        },
        false,
        Instant::now(),
        &context,
    );

    assert!(matches!(outcome, ObservationOutcome::Continue));
    assert_eq!(tracker.state_version(42, 7), None);
    assert_eq!(metrics.degraded.load(Ordering::Relaxed), 1);
    assert_eq!(
        metrics.failure.state(),
        ChromeFailureState::Unavailable(ChromeFailure::Validation(
            ChromeValidationFailure::WindowIdentityMismatch
        ))
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

fn publication_filter() -> FilterConfig {
    use zanei_core::config::capture_policy::{
        BrowserMode, BrowserPolicy, BrowserUrlRule, IdePolicy, PolicyAction,
    };
    FilterConfig {
        capture_policy: Some(zanei_core::config::CapturePolicyConfig {
            allowed_apps: vec!["Google Chrome".to_owned()],
            browser: BrowserPolicy {
                mode: BrowserMode::Rules,
                default_policy: PolicyAction::Allow,
                on_url_unavailable: PolicyAction::Block,
                block_auth: false,
                block_payments: false,
                allow_list: Vec::new(),
                block_list: vec![BrowserUrlRule {
                    host: "allowed.example".to_owned(),
                    path_prefix: "/private".to_owned(),
                    match_subdomains: false,
                }],
            },
            ide: IdePolicy {
                block_env_files: false,
                on_file_name_unavailable: PolicyAction::Allow,
            },
        }),
        ..FilterConfig::default()
    }
}

struct ReloadDuringQuery<'a> {
    tracker: &'a ChromeEligibilityTracker,
    publisher: &'a ChromeEligibilityPublisher,
    filters: Vec<FilterConfig>,
    replacement_at: Option<Instant>,
    result: Option<Result<ChromeObservation, ChromeFailure>>,
}

impl ChromeApi for ReloadDuringQuery<'_> {
    fn query(&mut self, _: &ChromeQuery) -> Result<ChromeObservation, ChromeFailure> {
        for filter in self.filters.drain(..) {
            self.tracker.replace_filter(filter);
        }
        if let Some(observed_at) = self.replacement_at {
            self.publisher.observe_at(
                42,
                ChromeEligibilityObservation::Normal {
                    window_id: Some(7),
                    url: "https://allowed.example/new-generation".to_owned(),
                },
                observed_at,
            );
        }
        self.result.take().expect("one query")
    }
}

#[test]
fn policy_reload_discards_every_old_query_result_without_touching_new_generation() {
    for result in [
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-101",
            "tab-1",
            "https://allowed.example/old",
            "Old",
        ))),
        Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "",
            "tab-1",
            "https://allowed.example/old",
            "Invalid",
        ))),
        Ok(ChromeObservation::Incognito { window_id: Some(7) }),
        Ok(ChromeObservation::NoWindow),
        Ok(ChromeObservation::NotRunning),
        Err(ChromeFailure::Query(ChromeQueryFailure::AppleEvent(-1712))),
    ] {
        // A -> B -> A must still reject the query started under the first A.
        for restore_original in [false, true] {
            let filter = publication_filter();
            let (publisher, capture) = chrome_eligibility_channel(filter.clone());
            let mut changed = filter.clone();
            changed.capture_policy.as_mut().unwrap().browser.block_auth = true;
            let mut filters = vec![changed];
            if restore_original {
                filters.push(filter);
            }
            let replacement_at = Instant::now();
            let mut api = ReloadDuringQuery {
                tracker: &capture,
                publisher: &publisher,
                filters,
                replacement_at: Some(replacement_at),
                result: Some(result.clone()),
            };
            let (sender, events) = sync_channel(1);
            let metrics = ChromeMetrics::default();
            let mut navigation = tracker_with_initial_snapshot();
            let outcome = observe_once(
                &mut api,
                &mut navigation,
                &chrome_app(),
                &sender,
                &metrics,
                &publisher,
            );
            assert!(matches!(outcome, ObservationOutcome::Continue));
            assert!(events.try_recv().is_err());
            assert_eq!(capture.observed_at(42, 7), Some(replacement_at));
            assert!(capture.state_version(42, 7).is_some());
            assert_eq!(
                capture
                    .decision(PrivacyScope::AllEvents, 42, Some(7))
                    .capture_context()
                    .url
                    .as_deref(),
                Some("https://allowed.example/new-generation")
            );
            let mut repeated_page = FakeApi::new([Ok(ChromeObservation::Snapshot(snapshot(
                "window-1",
                "tab-1",
                "https://example.com",
                "First",
            )))]);
            observe_once(
                &mut repeated_page,
                &mut navigation,
                &chrome_app(),
                &sender,
                &metrics,
                &publisher,
            );
            assert!(
                events.try_recv().is_err(),
                "stale result must not reset navigation"
            );
            assert_eq!(metrics.degraded.load(Ordering::Relaxed), 0);
        }
    }
}

#[test]
fn policy_reload_does_not_reregister_an_invalidated_window() {
    let (publisher, capture) = chrome_eligibility_channel(publication_filter());
    publisher.observe(
        42,
        ChromeEligibilityObservation::Normal {
            window_id: Some(7),
            url: "https://allowed.example/before".to_owned(),
        },
    );
    let mut changed = publication_filter();
    changed.capture_policy.as_mut().unwrap().browser.block_auth = true;
    let mut api = ReloadDuringQuery {
        tracker: &capture,
        publisher: &publisher,
        filters: vec![changed],
        replacement_at: None,
        result: Some(Ok(ChromeObservation::Snapshot(snapshot_for_window(
            7,
            "window-101",
            "tab-1",
            "https://allowed.example/old",
            "Old",
        )))),
    };
    let (sender, events) = sync_channel(1);
    observe_once(
        &mut api,
        &mut NavigationTracker::default(),
        &chrome_app(),
        &sender,
        &ChromeMetrics::default(),
        &publisher,
    );
    assert_eq!(capture.state_version(42, 7), None);
    assert!(events.try_recv().is_err());
}

#[test]
fn identical_policy_reload_preserves_version_and_allows_query_publication() {
    let filter = publication_filter();
    let (publisher, capture) = chrome_eligibility_channel(filter.clone());
    let snapshot = snapshot_for_window(
        7,
        "window-101",
        "tab-1",
        "https://allowed.example/normal",
        "Normal",
    );
    let initial = Instant::now() - Duration::from_secs(1);
    publisher.observe_with_surface_at(
        42,
        ChromeEligibilityObservation::Normal {
            window_id: Some(7),
            url: snapshot.url.clone(),
        },
        Some(snapshot.applescript_window_id.clone()),
        Some(&snapshot.tab_key),
        initial,
    );
    let version = capture.state_version(42, 7);
    let mut api = ReloadDuringQuery {
        tracker: &capture,
        publisher: &publisher,
        filters: vec![filter],
        replacement_at: None,
        result: Some(Ok(ChromeObservation::Snapshot(snapshot))),
    };
    let (sender, events) = sync_channel(1);
    observe_once(
        &mut api,
        &mut NavigationTracker::default(),
        &chrome_app(),
        &sender,
        &ChromeMetrics::default(),
        &publisher,
    );
    assert_eq!(capture.state_version(42, 7), version);
    assert!(capture.observed_at(42, 7).unwrap() > initial);
    assert!(events.try_recv().is_ok());
}

#[test]
fn denied_url_is_not_published_or_used_as_the_next_navigation_origin() {
    let standalone = FilterConfig {
        exclude_websites: vec!["allowed.example".to_owned()],
        ..FilterConfig::default()
    };
    for filter in [publication_filter(), standalone] {
        let (publisher, capture) = chrome_eligibility_channel(filter);
        let (sender, events) = sync_channel(2);
        let mut api = FakeApi::new([
            Ok(ChromeObservation::Snapshot(snapshot_for_window(
                7,
                "window-101",
                "tab-1",
                "https://allowed.example/private/details",
                "Private",
            ))),
            Ok(ChromeObservation::Snapshot(snapshot_for_window(
                7,
                "window-101",
                "tab-1",
                "https://other.example/public",
                "Public",
            ))),
        ]);
        let mut navigation = tracker_with_initial_snapshot();
        let metrics = ChromeMetrics::default();
        observe_once(
            &mut api,
            &mut navigation,
            &chrome_app(),
            &sender,
            &metrics,
            &publisher,
        );
        assert!(events.try_recv().is_err());
        assert!(!capture.allows_url_events(42, Some(7)));
        observe_once(
            &mut api,
            &mut navigation,
            &chrome_app(),
            &sender,
            &metrics,
            &publisher,
        );
        let event = events.try_recv().expect("allowed navigation");
        let EventData::BrowserNavigate(data) = event.data else {
            panic!("navigation")
        };
        assert_eq!(data.url, "https://other.example/public");
        assert_eq!(data.transition, None);
    }
}
