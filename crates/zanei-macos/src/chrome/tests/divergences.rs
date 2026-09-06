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
            target: BrowserTarget::Chrome,
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
            target: BrowserTarget::Chrome,
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
    assert_eq!(snapshot.page.tab_key(), Some("tab-alpha-001"));
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
    state.apps.insert(42, chrome_app());
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
            target: BrowserTarget::Chrome,
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
            target: BrowserTarget::Chrome,
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
        metrics.failure.state(&eligibility.query_targets()),
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
    collector
        .metrics
        .failure
        .observe_failure(BrowserTarget::Chrome, failure);
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
        metrics.failure.state(&eligibility.query_targets()),
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
        metrics.failure.state(&eligibility.query_targets()),
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
    assert_eq!(
        metrics.failure.state(&eligibility.query_targets()),
        ChromeFailureState::Available
    );
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
    assert_eq!(
        metrics.failure.state(&eligibility.query_targets()),
        ChromeFailureState::Available
    );
    assert_eq!(metrics.degraded.load(Ordering::Relaxed), 2);
    assert_eq!(metrics.dropped.load(Ordering::Relaxed), 1);
}

#[test]
fn restart_seeds_current_safari_focus_after_target_set_change() {
    let focus_context = FocusContext::new();
    let mut safari_focus = chrome_focus(7);
    safari_focus.app = browser_app(BrowserTarget::Safari);
    focus_context.activate(safari_focus.app.clone(), safari_focus.window.clone());

    let (eligibility, _) = chrome_eligibility_channel(both_browser_filter());
    assert_eq!(
        eligibility.query_targets(),
        BTreeSet::from([BrowserTarget::Chrome])
    );
    let (sender, events) = sync_channel(1);
    let metrics = ChromeMetrics::default();

    let mut old_api = FakeApi::new([]);
    let mut old_state = ChromeWorkerState::default();
    assert!(handle_focus_transition(
        FocusTransition {
            previous: None,
            current: focus_context.current(),
            resynced: false,
        },
        Instant::now(),
        &mut old_api,
        &sender,
        &mut old_state,
        &metrics,
        &eligibility,
    ));
    assert_eq!(old_api.query_count, 0);
    assert_eq!(old_state.frontmost, None);

    eligibility.set_query_targets(BTreeSet::from([BrowserTarget::Safari]));
    let new_focus = focus_context.subscribe();
    let (_new_observation_sender, new_observations) = sync_channel(1);
    let new_focus_context = focus_context.clone();
    let new_metrics = metrics.clone();
    let new_eligibility = eligibility.clone();
    let new_sender = sender.clone();
    let new_stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&new_stop);
    let initial_focus = focus_context.current().map(|focus| FocusTransition {
        previous: None,
        current: Some(focus),
        resynced: false,
    });
    let worker = std::thread::spawn(move || {
        let mut new_api = FakeApi::new([Ok(ChromeObservation::Snapshot(browser_snapshot(
            BrowserTarget::Safari,
            Some("https://allowed.example/reloaded"),
        )))]);
        let receivers = ChromeWorkerReceivers {
            focus: &new_focus,
            observations: &new_observations,
            focus_context: &new_focus_context,
        };
        run_worker(
            &mut new_api,
            &receivers,
            &new_sender,
            &worker_stop,
            &new_metrics,
            &new_eligibility,
            initial_focus,
        );
        new_api
    });

    let event = events.recv_timeout(Duration::from_secs(1));
    new_stop.store(true, Ordering::Release);
    let new_api = worker.join().expect("worker stopped");
    let event = event.expect("seeded Safari navigation");

    assert_eq!(
        new_api.queries,
        [ChromeQuery::FrontWindow {
            target: BrowserTarget::Safari,
            pid: 43,
            window_id: Some(7),
        }]
    );
    assert_eq!(
        event.app.bundle_id.as_deref(),
        Some(BrowserTarget::Safari.bundle_id())
    );
    let EventData::BrowserNavigate(data) = event.data else {
        panic!("Safari navigation");
    };
    assert_eq!(data.url, "https://allowed.example/reloaded");
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
            url: snapshot.page.url().unwrap().to_owned(),
        },
        Some(snapshot.applescript_window_id.clone()),
        snapshot.page.tab_key(),
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

fn both_browser_filter() -> FilterConfig {
    let mut filter = publication_filter();
    let policy = filter.capture_policy.as_mut().unwrap();
    policy.allowed_apps.push("Safari".to_owned());
    policy.browser.on_url_unavailable = zanei_core::config::capture_policy::PolicyAction::Allow;
    filter
}

fn browser_app(target: BrowserTarget) -> ApplicationInfo {
    ApplicationInfo {
        name: target.display_name().to_owned(),
        bundle_id: Some(target.bundle_id().to_owned()),
        pid: if target == BrowserTarget::Chrome {
            42
        } else {
            43
        },
        ..chrome_app()
    }
}

fn browser_snapshot(target: BrowserTarget, url: Option<&str>) -> ChromeSnapshot {
    let mut value = snapshot_for_window(
        7,
        "101",
        "tab-1",
        url.unwrap_or("https://allowed.example"),
        "Page",
    );
    if target == BrowserTarget::Safari {
        value.page = BrowserPage::Safari {
            url: url.map(str::to_owned),
        };
    }
    value
}

#[test]
fn denied_browser_does_not_stop_or_get_hidden_by_other_browser_success() {
    let denied = ChromeFailure::from(AppleScriptError::Execute { code: Some(-1743) });
    assert_eq!(
        denied,
        ChromeFailure::Query(ChromeQueryFailure::AppleEvent(-1743))
    );
    for (blocked, allowed) in [
        (BrowserTarget::Chrome, BrowserTarget::Safari),
        (BrowserTarget::Safari, BrowserTarget::Chrome),
    ] {
        let filter = both_browser_filter();
        let (publisher, capture) = chrome_eligibility_channel(filter.clone());
        publisher.set_query_targets(BTreeSet::from([blocked, allowed]));
        let metrics = ChromeMetrics::default();
        let (sender, events) = sync_channel(8);
        let mut navigation = NavigationTracker::default();
        for target in [blocked, allowed] {
            let mut api = FakeApi::new([Ok(ChromeObservation::Snapshot(browser_snapshot(
                target,
                Some("https://allowed.example"),
            )))]);
            observe_once(
                &mut api,
                &mut navigation,
                &browser_app(target),
                &sender,
                &metrics,
                &publisher,
            );
            assert_eq!(
                events.try_recv().unwrap().app.bundle_id.as_deref(),
                Some(target.bundle_id())
            );
        }
        let mut api = FakeApi::new([Err(denied)]);
        assert!(matches!(
            observe_once(
                &mut api,
                &mut navigation,
                &browser_app(blocked),
                &sender,
                &metrics,
                &publisher
            ),
            ObservationOutcome::Continue
        ));
        assert!(events.try_recv().is_err());
        assert_eq!(capture.state_version(browser_app(blocked).pid, 7), None);
        assert!(capture.state_version(browser_app(allowed).pid, 7).is_some());
        let mut api = FakeApi::new([Ok(ChromeObservation::Snapshot(browser_snapshot(
            allowed,
            Some("https://allowed.example/next"),
        )))]);
        observe_once(
            &mut api,
            &mut navigation,
            &browser_app(allowed),
            &sender,
            &metrics,
            &publisher,
        );
        assert!(events.try_recv().is_ok());
        assert_eq!(
            metrics.failure.state(&publisher.query_targets()),
            ChromeFailureState::Unavailable(denied)
        );
        publisher.set_query_targets(BTreeSet::from([allowed]));
        assert_eq!(
            metrics.failure.state(&publisher.query_targets()),
            ChromeFailureState::Available
        );
        publisher.set_query_targets(BTreeSet::from([blocked, allowed]));
        let mut changed = filter;
        changed
            .capture_policy
            .as_mut()
            .unwrap()
            .allowed_apps
            .retain(|name| name != blocked.display_name());
        capture.replace_filter(changed);
        assert_eq!(
            metrics.failure.state(&publisher.query_targets()),
            ChromeFailureState::Available
        );
    }
}

#[test]
fn safari_unknown_url_preserves_surface_and_unknown_identity_without_url_event() {
    let (publisher, capture) = chrome_eligibility_channel(both_browser_filter());
    publisher.set_query_targets(BTreeSet::from([BrowserTarget::Safari]));
    let metrics = ChromeMetrics::default();
    let (sender, events) = sync_channel(4);
    let mut navigation = NavigationTracker::default();
    for url in [
        Some("https://allowed.example"),
        None,
        Some("https://allowed.example"),
    ] {
        let mut api = FakeApi::new([Ok(ChromeObservation::Snapshot(browser_snapshot(
            BrowserTarget::Safari,
            url,
        )))]);
        observe_once(
            &mut api,
            &mut navigation,
            &browser_app(BrowserTarget::Safari),
            &sender,
            &metrics,
            &publisher,
        );
        let decision = capture.decision(PrivacyScope::TextContent, 43, Some(7));
        assert!(decision.is_allowed());
        let context = decision.capture_context();
        assert_eq!(context.url.as_deref(), url);
        let surface = context.surface.unwrap();
        assert_eq!(surface.cg_window_id, Some(7));
        assert_eq!(surface.applescript_window_id.as_deref(), Some("101"));
        assert_eq!(surface.tab_id, None);
        if let Some(url) = url {
            let event = events.try_recv().unwrap();
            let EventData::BrowserNavigate(data) = event.data else {
                panic!("navigation");
            };
            assert_eq!(data.mode, BrowserMode::Unknown);
            assert_eq!(data.url, url);
            assert_eq!(data.transition, None);
            assert_eq!(event.capture_context.surface.unwrap().tab_id, None);
        } else {
            assert!(events.try_recv().is_err());
        }
    }
}

#[test]
fn browser_queries_require_both_configuration_and_current_policy() {
    let mut off = both_browser_filter();
    off.capture_policy.as_mut().unwrap().browser.mode =
        zanei_core::config::capture_policy::BrowserMode::Off;
    for (filter, targets, target) in [
        (
            both_browser_filter(),
            BTreeSet::from([BrowserTarget::Chrome]),
            BrowserTarget::Safari,
        ),
        (
            FilterConfig::default(),
            BTreeSet::from([BrowserTarget::Safari]),
            BrowserTarget::Safari,
        ),
        (
            off,
            BTreeSet::from([BrowserTarget::Chrome, BrowserTarget::Safari]),
            BrowserTarget::Chrome,
        ),
    ] {
        let (publisher, _) = chrome_eligibility_channel(filter);
        publisher.set_query_targets(targets);
        let mut api = FakeApi::new([]);
        let (sender, events) = sync_channel(1);
        observe_once(
            &mut api,
            &mut NavigationTracker::default(),
            &browser_app(target),
            &sender,
            &ChromeMetrics::default(),
            &publisher,
        );
        assert_eq!(api.query_count, 0);
        assert!(events.try_recv().is_err());
    }
}

fn background_confirmation_preserves_foreground(
    background: BrowserTarget,
    result: Result<ChromeObservation, ChromeFailure>,
) {
    let foreground = if background == BrowserTarget::Chrome {
        BrowserTarget::Safari
    } else {
        BrowserTarget::Chrome
    };
    let pid = browser_app(background).pid;
    let (publisher, capture) = chrome_eligibility_channel(both_browser_filter());
    publisher.set_query_targets(BTreeSet::from([
        BrowserTarget::Chrome,
        BrowserTarget::Safari,
    ]));
    let metrics = ChromeMetrics::default();
    let (sender, events) = sync_channel(4);
    let mut state = ChromeWorkerState::default();
    let mut api = FakeApi::new(
        [background, foreground]
            .map(|target| {
                Ok(ChromeObservation::Snapshot(browser_snapshot(
                    target,
                    Some("https://allowed.example"),
                )))
            })
            .into_iter()
            .chain([result]),
    );
    let now = Instant::now();
    for (index, target) in [background, foreground].into_iter().enumerate() {
        let mut focus = chrome_focus(7);
        focus.app = browser_app(target);
        focus.generation = index as u64 + 1;
        assert!(handle_focus_transition(
            FocusTransition {
                previous: state.frontmost.clone(),
                current: Some(focus),
                resynced: false
            },
            now,
            &mut api,
            &sender,
            &mut state,
            &metrics,
            &publisher
        ));
        let event = events.try_recv().unwrap();
        assert_eq!(event.app.bundle_id.as_deref(), Some(target.bundle_id()));
        let EventData::BrowserNavigate(data) = event.data else {
            panic!("navigation");
        };
        assert_eq!(data.transition, None);
    }
    handle_observation_trigger(
        ObservationTrigger::OnDemand { pid, window_id: 7 },
        now,
        &mut api,
        &sender,
        &mut state,
        &metrics,
        &publisher,
    );
    service_on_demand(
        now + Duration::from_millis(200),
        &mut api,
        &sender,
        &mut state,
        &metrics,
        &publisher,
    );
    assert_eq!(api.queries[0].target(), background);
    assert_eq!(api.queries[1].target(), foreground);
    assert_eq!(
        api.queries[2],
        ChromeQuery::Window {
            target: background,
            pid,
            window_id: 7,
            applescript_window_id: AppleScriptWindowId::for_test("101")
        }
    );
    assert!(events.try_recv().is_err());
    for (url, emits, allowed) in [
        ("https://allowed.example", false, true),
        ("https://allowed.example/private", false, false),
        ("https://allowed.example/next", true, true),
    ] {
        let mut api = FakeApi::new([Ok(ChromeObservation::Snapshot(browser_snapshot(
            foreground,
            Some(url),
        )))]);
        handle_observation_trigger(
            ObservationTrigger::PageLoaded {
                pid: browser_app(foreground).pid,
            },
            now,
            &mut api,
            &sender,
            &mut state,
            &metrics,
            &publisher,
        );
        assert_eq!(
            api.query_count, 1,
            "background result must preserve foreground observation"
        );
        assert_eq!(events.try_recv().is_ok(), emits);
        let decision = capture.decision(
            PrivacyScope::TextContent,
            browser_app(foreground).pid,
            Some(7),
        );
        assert_eq!(decision.is_allowed(), allowed);
        assert_eq!(decision.capture_context().url.as_deref(), Some(url));
    }
}

#[test]
fn background_browser_results_preserve_foreground_with_identical_window_ids() {
    for background in [BrowserTarget::Chrome, BrowserTarget::Safari] {
        let snapshot = browser_snapshot(background, Some("https://allowed.example"));
        let mut invalid = snapshot.clone();
        invalid.applescript_window_id = AppleScriptWindowId::for_test("mismatched");
        for result in [
            Ok(ChromeObservation::Snapshot(snapshot)),
            Ok(ChromeObservation::NotRunning),
            Err(ChromeFailure::Query(ChromeQueryFailure::AppleEvent(-1743))),
            Ok(ChromeObservation::Snapshot(invalid)),
        ] {
            background_confirmation_preserves_foreground(background, result);
        }
    }
}

struct ReconfigureBrowserDuringQuery<'a> {
    publisher: &'a ChromeEligibilityPublisher,
    focus: &'a FocusContext,
    change_focus: bool,
}

impl ChromeApi for ReconfigureBrowserDuringQuery<'_> {
    fn query(&mut self, query: &ChromeQuery) -> Result<ChromeObservation, ChromeFailure> {
        assert_eq!(query.target(), BrowserTarget::Safari);
        if self.change_focus {
            self.focus
                .activate(browser_app(BrowserTarget::Chrome), None);
        } else {
            self.publisher
                .set_query_targets(BTreeSet::from([BrowserTarget::Chrome]));
        }
        Ok(ChromeObservation::Snapshot(browser_snapshot(
            BrowserTarget::Safari,
            Some("https://allowed.example/stale"),
        )))
    }
}

#[test]
fn safari_delayed_result_is_discarded_when_target_configuration_or_focus_changes() {
    for change_focus in [false, true] {
        let (publisher, capture) = chrome_eligibility_channel(both_browser_filter());
        publisher.set_query_targets(BTreeSet::from([
            BrowserTarget::Chrome,
            BrowserTarget::Safari,
        ]));
        let focus = FocusContext::new();
        let app = browser_app(BrowserTarget::Safari);
        focus.activate(app.clone(), None);
        let stop = AtomicBool::new(false);
        let metrics = ChromeMetrics::default();
        let (sender, events) = sync_channel(1);
        let context = ObservationContext {
            sender: &sender,
            stop: &stop,
            focus_context: &focus,
            metrics: &metrics,
            eligibility: &publisher,
        };
        let mut api = ReconfigureBrowserDuringQuery {
            publisher: &publisher,
            focus: &focus,
            change_focus,
        };
        observe_query_once(
            &mut api,
            &mut NavigationTracker::default(),
            Some(&app),
            ChromeQuery::FrontWindow {
                target: BrowserTarget::Safari,
                pid: 43,
                window_id: None,
            },
            true,
            Instant::now(),
            &context,
        );
        assert!(events.try_recv().is_err());
        assert_eq!(capture.state_version(43, 7), None);
        assert_eq!(metrics.degraded.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn client_switch_failure_never_reuses_the_other_browser_client() {
    let mut api = SystemChromeApi::<BrowserTarget> { client: None };
    assert_eq!(
        *api.get_or_initialize_client(BrowserTarget::Chrome, || Ok(BrowserTarget::Chrome))
            .unwrap(),
        BrowserTarget::Chrome
    );
    assert!(
        api.get_or_initialize_client(BrowserTarget::Safari, || Err(
            AppleScriptError::SafariUnavailable
        ))
        .is_err()
    );
    assert_eq!(
        *api.get_or_initialize_client(BrowserTarget::Safari, || Ok(BrowserTarget::Safari))
            .unwrap(),
        BrowserTarget::Safari
    );
    assert_eq!(
        *api.get_or_initialize_client(BrowserTarget::Chrome, || Ok(BrowserTarget::Chrome))
            .unwrap(),
        BrowserTarget::Chrome
    );
}

#[test]
fn safari_window_and_url_changes_never_assert_known_tab_transitions() {
    let mut tracker = NavigationTracker::default();
    let first = browser_snapshot(BrowserTarget::Safari, Some("https://allowed.example"));
    assert!(tracker.observe(first.clone()).unwrap().is_some());
    assert!(tracker.observe(first.clone()).unwrap().is_none());
    let mut another_window = first;
    another_window.applescript_window_id = AppleScriptWindowId::for_test("102");
    let change = tracker.observe(another_window.clone()).unwrap().unwrap();
    assert_eq!(change.transition, None);
    assert_eq!(change.snapshot.page.tab_key(), None);
    another_window.page = BrowserPage::Safari {
        url: Some("https://allowed.example/other".to_owned()),
    };
    assert_eq!(
        tracker.observe(another_window).unwrap().unwrap().transition,
        None
    );
}

#[test]
fn standalone_safari_focus_leaves_chrome_and_return_emits_again() {
    let (publisher, _) = chrome_eligibility_channel(FilterConfig::default());
    let (sender, events) = sync_channel(4);
    let metrics = ChromeMetrics::default();
    let mut state = ChromeWorkerState::default();
    let snapshot = browser_snapshot(BrowserTarget::Chrome, Some("https://allowed.example"));
    let mut api = FakeApi::new([
        Ok(ChromeObservation::Snapshot(snapshot.clone())),
        Ok(ChromeObservation::Snapshot(snapshot)),
    ]);
    for (index, target) in [
        BrowserTarget::Chrome,
        BrowserTarget::Safari,
        BrowserTarget::Chrome,
    ]
    .into_iter()
    .enumerate()
    {
        let mut focus = chrome_focus(7);
        focus.app = browser_app(target);
        focus.generation = index as u64 + 1;
        handle_focus_transition(
            FocusTransition {
                previous: state.frontmost.clone(),
                current: Some(focus),
                resynced: false,
            },
            Instant::now(),
            &mut api,
            &sender,
            &mut state,
            &metrics,
            &publisher,
        );
        if target == BrowserTarget::Chrome {
            assert!(events.try_recv().is_ok());
        } else {
            assert!(events.try_recv().is_err());
        }
    }
    assert_eq!(
        api.queries
            .iter()
            .map(ChromeQuery::target)
            .collect::<Vec<_>>(),
        [BrowserTarget::Chrome, BrowserTarget::Chrome]
    );
}
