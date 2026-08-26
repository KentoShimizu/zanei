use super::*;
use crate::daemon::collectors::{ProducerFailureOrigin, ProducerFailures};
use crate::daemon::supervisor::add_restart_degradation;
use zanei_core::privacy::CHROME_BUNDLE_ID;
use zanei_macos::chrome::{ChromeFailure, ChromeFailureState, ChromeQueryFailure};

struct FakeClock {
    now: Instant,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            now: Instant::now(),
        }
    }

    fn advance(&mut self, duration: Duration) {
        self.now += duration;
    }
}

#[test]
fn eventtap_gate_does_not_block_other_collectors() {
    let eventtap_state = Arc::new(FakeState::default());
    let deferred_eventtap_state = Arc::new(FakeState::default());
    let other_state = Arc::new(FakeState::default());
    let mut eventtap = Some(Managed::new(FakeCollector::new(
        Arc::clone(&eventtap_state),
        BTreeSet::new(),
    )));
    let mut deferred_eventtap = Some(Managed::new(FakeCollector::new(
        Arc::clone(&deferred_eventtap_state),
        BTreeSet::new(),
    )));
    let mut other = Some(Managed::new(FakeCollector::new(
        Arc::clone(&other_state),
        BTreeSet::new(),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();
    let mut degraded = BTreeMap::new();
    let mut gate = EventTapStartGate::open();
    let mut deferred_gate = EventTapStartGate::open();
    let now = Instant::now();

    configure_eventtap_start_gate(
        Some(Ok(PermissionStatus::Granted)),
        &mut gate,
        &mut degraded,
    );
    configure_eventtap_start_gate(
        Some(Ok(PermissionStatus::Denied)),
        &mut deferred_gate,
        &mut degraded,
    );
    start_collector_if_allowed(&mut eventtap, &pipeline, &mut errors, now, gate);
    start_collector_if_allowed(
        &mut deferred_eventtap,
        &pipeline,
        &mut errors,
        now,
        deferred_gate,
    );
    start_collector(&mut other, &pipeline, &mut errors, now);
    assert_eq!(eventtap_state.starts.load(Ordering::Relaxed), 1);
    assert_eq!(deferred_eventtap_state.starts.load(Ordering::Relaxed), 0);
    assert_eq!(other_state.starts.load(Ordering::Relaxed), 1);

    eventtap_state.finish();
    other_state.finish();
    wait_for_relay(&eventtap);
    wait_for_relay(&other);
}

#[test]
fn eventtap_waits_for_the_typed_permission_completion_channel() {
    let state = Arc::new(FakeState::default());
    let mut eventtap = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::new(),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let (release, release_rx) = mpsc::sync_channel(1);
    let mut worker = Some(
        PermissionRequestWorker::start_with(move || {
            release_rx.recv().expect("release permission worker");
            Ok(PermissionRequestOutcome::Completed)
        })
        .expect("permission worker"),
    );
    let mut errors = BTreeMap::new();
    let mut degraded = BTreeMap::new();
    let mut gate = EventTapStartGate::open();
    configure_eventtap_start_gate(
        Some(Ok(PermissionStatus::NotDetermined)),
        &mut gate,
        &mut degraded,
    );

    start_collector_if_allowed(&mut eventtap, &pipeline, &mut errors, Instant::now(), gate);
    service_permission_request_worker(&mut worker, &mut degraded, true, |_| {
        panic!("pending worker must not release EventTap")
    });
    assert_eq!(state.starts.load(Ordering::Relaxed), 0);
    assert!(!gate.allows_start());

    release.send(()).expect("complete permission worker");
    complete_permission_worker(
        &mut worker,
        &mut gate,
        &mut eventtap,
        &pipeline,
        &mut errors,
        &mut degraded,
        true,
    );

    assert!(gate.allows_start());
    assert_eq!(state.starts.load(Ordering::Relaxed), 1);
    state.finish();
    wait_for_relay(&eventtap);
}

#[test]
fn permission_timeout_attempts_eventtap_start() {
    let state = Arc::new(FakeState::default());
    let mut eventtap = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::new(),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut worker = Some(
        PermissionRequestWorker::start_with(|| Ok(PermissionRequestOutcome::TimedOut))
            .expect("permission worker"),
    );
    let mut errors = BTreeMap::new();
    let mut degraded = BTreeMap::new();
    let mut gate = EventTapStartGate::open();
    gate.defer();

    complete_permission_worker(
        &mut worker,
        &mut gate,
        &mut eventtap,
        &pipeline,
        &mut errors,
        &mut degraded,
        true,
    );

    assert_eq!(state.starts.load(Ordering::Relaxed), 1);
    assert!(degraded["permission_request"].contains("timed out"));
    state.finish();
    wait_for_relay(&eventtap);
}

#[test]
fn inactive_daemon_opens_gate_without_starting_eventtap() {
    let state = Arc::new(FakeState::default());
    let mut eventtap = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::new(),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut worker = Some(
        PermissionRequestWorker::start_with(|| Ok(PermissionRequestOutcome::Completed))
            .expect("permission worker"),
    );
    let mut errors = BTreeMap::new();
    let mut degraded = BTreeMap::new();
    let mut gate = EventTapStartGate::open();
    gate.defer();

    complete_permission_worker(
        &mut worker,
        &mut gate,
        &mut eventtap,
        &pipeline,
        &mut errors,
        &mut degraded,
        false,
    );
    assert!(gate.allows_start());
    assert_eq!(state.starts.load(Ordering::Relaxed), 0);

    start_collector_if_allowed(&mut eventtap, &pipeline, &mut errors, Instant::now(), gate);
    assert_eq!(state.starts.load(Ordering::Relaxed), 1);
    state.finish();
    wait_for_relay(&eventtap);
}

#[test]
fn unexpected_exit_uses_capped_backoff_and_stays_degraded_after_restart() {
    let state = Arc::new(FakeState::default());
    let mut managed = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::new(),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();
    let mut clock = FakeClock::new();
    start_collector(&mut managed, &pipeline, &mut errors, clock.now);

    for (failure_index, delay_seconds) in [5, 10, 20, 40, 60, 60].into_iter().enumerate() {
        state.finish();
        wait_for_relay(&managed);
        supervise_collector(
            &mut managed,
            &pipeline,
            Some(&granted_permissions()),
            &mut errors,
            clock.now,
        )
        .expect("observe failed collector");
        assert_eq!(state.starts.load(Ordering::Relaxed), failure_index + 1);
        assert_eq!(
            managed.as_ref().and_then(Managed::restart_degraded_reason),
            Some("collector worker terminated unexpectedly")
        );

        clock.advance(Duration::from_secs(delay_seconds - 1));
        supervise_collector(
            &mut managed,
            &pipeline,
            Some(&granted_permissions()),
            &mut errors,
            clock.now,
        )
        .expect("hold collector before restart deadline");
        assert_eq!(state.starts.load(Ordering::Relaxed), failure_index + 1);

        clock.advance(Duration::from_secs(1));
        supervise_collector(
            &mut managed,
            &pipeline,
            Some(&granted_permissions()),
            &mut errors,
            clock.now,
        )
        .expect("restart collector at deadline");
        assert_eq!(state.starts.load(Ordering::Relaxed), failure_index + 2);
        assert!(errors.is_empty());
        assert!(
            managed
                .as_ref()
                .and_then(Managed::restart_degraded_reason)
                .is_some()
        );
    }

    state.finish();
    wait_for_relay(&managed);
}

#[test]
fn restart_start_failure_overrides_retained_exit_until_recovery_is_stable() {
    let state = Arc::new(FakeState::default());
    let mut managed = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::new(),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();
    let mut clock = FakeClock::new();
    start_collector(&mut managed, &pipeline, &mut errors, clock.now);

    state.finish();
    wait_for_relay(&managed);
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        clock.now,
    )
    .expect("observe unexpected exit");
    assert_eq!(
        projected_degradation(&managed, &errors).as_deref(),
        Some("collector worker terminated unexpectedly")
    );

    managed
        .as_mut()
        .expect("fake collector")
        .collector
        .fail_start = true;
    clock.advance(Duration::from_secs(5));
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        clock.now,
    )
    .expect("record restart failure");
    assert_eq!(state.starts.load(Ordering::Relaxed), 2);
    assert_eq!(
        projected_degradation(&managed, &errors).as_deref(),
        Some("missing permission")
    );

    managed
        .as_mut()
        .expect("fake collector")
        .collector
        .fail_start = false;
    clock.advance(Duration::from_secs(10));
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        clock.now,
    )
    .expect("restart collector");
    assert_eq!(state.starts.load(Ordering::Relaxed), 3);
    assert_eq!(
        projected_degradation(&managed, &errors).as_deref(),
        Some("collector worker terminated unexpectedly")
    );

    clock.advance(Duration::from_secs(60));
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        clock.now,
    )
    .expect("observe stable restart");
    assert_eq!(projected_degradation(&managed, &errors), None);

    state.finish();
    wait_for_relay(&managed);
}

#[test]
fn planned_stop_and_suspend_preserve_exit_until_resumed_worker_is_stable() {
    for suspend in [false, true] {
        assert_exit_survives_collector_set_resume(suspend);
    }
}

const PRODUCER_FAILURE_CASES: [(ProducerFailureOrigin, &str); 4] = [
    (ProducerFailureOrigin::SecureInputStart, "secure_input"),
    (ProducerFailureOrigin::WorkspaceMainThread, "workspace"),
    (ProducerFailureOrigin::EventTapMainThread, "eventtap"),
    (
        ProducerFailureOrigin::ContentSnapshotFilter,
        "content_snapshot",
    ),
];

#[test]
fn producer_failure_retries_and_successes_change_only_their_origin() {
    for (retried_origin, retried_component) in PRODUCER_FAILURE_CASES {
        let mut failures = ProducerFailures::default();
        for (origin, component) in PRODUCER_FAILURE_CASES {
            failures.record(origin, Err(format!("initial {component} failure")));
        }

        failures.record(
            retried_origin,
            Err(format!("retried {retried_component} failure")),
        );
        let retry_reasons: BTreeMap<_, _> = failures.reasons().collect();
        for (origin, component) in PRODUCER_FAILURE_CASES {
            let expected = if origin == retried_origin {
                format!("retried {component} failure")
            } else {
                format!("initial {component} failure")
            };
            assert_eq!(retry_reasons.get(component), Some(&expected.as_str()));
        }

        failures.record(retried_origin, Ok(()));
        let recovered_reasons: BTreeMap<_, _> = failures.reasons().collect();
        assert!(!recovered_reasons.contains_key(retried_component));
        for (origin, component) in PRODUCER_FAILURE_CASES {
            if origin != retried_origin {
                let expected = format!("initial {component} failure");
                assert_eq!(recovered_reasons.get(component), Some(&expected.as_str()));
            }
        }
    }
}

#[test]
fn main_thread_producer_failures_prevent_derived_worker_start_failures() {
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut workspace_config = collector_lifecycle_test_config();
    workspace_config.capture.sources = vec![CaptureSource::App];
    let mut workspace = CollectorSet::new(&workspace_config);
    workspace.set_producer_result_for_test(
        ProducerFailureOrigin::WorkspaceMainThread,
        Err("workspace observer creation failed"),
    );

    workspace.start(&pipeline);
    workspace
        .supervise(&pipeline, Some(&granted_permissions()), Instant::now())
        .expect("supervise workspace with a missing producer");

    assert!(!workspace.start_errors.contains_key("workspace"));
    assert_eq!(
        workspace
            .health()
            .degraded
            .get("workspace")
            .map(String::as_str),
        Some("workspace observer creation failed")
    );
    workspace.suspend();

    let mut eventtap = CollectorSet::new(&input_text_content_test_config());
    assert!(
        eventtap.chrome.is_none(),
        "input lifecycle fixture must not start the system Chrome worker"
    );
    eventtap.set_producer_result_for_test(
        ProducerFailureOrigin::EventTapMainThread,
        Err("input source observer creation failed"),
    );

    eventtap.start_eventtap(&pipeline, Instant::now());
    eventtap
        .supervise(&pipeline, Some(&granted_permissions()), Instant::now())
        .expect("supervise EventTap with a missing producer");

    assert!(!eventtap.start_errors.contains_key("eventtap"));
    assert_eq!(
        eventtap
            .health()
            .degraded
            .get("eventtap")
            .map(String::as_str),
        Some("input source observer creation failed")
    );
}

#[test]
fn producer_reason_has_priority_over_start_restart_and_runtime_degradation() {
    let mut collectors = collector_set_with_config_and_secure_input_result(
        &input_text_content_test_config(),
        Err("secure input monitor failed"),
    );
    assert!(
        collectors.eventtap.is_some(),
        "input capture keeps EventTap"
    );
    collectors.set_secure_input_runtime_for_test(true);
    collectors.start_errors.insert(
        "secure_input".to_owned(),
        "derived start failure".to_owned(),
    );

    assert_secure_input_failure(&collectors, "secure input monitor failed");

    collectors.set_producer_result_for_test(ProducerFailureOrigin::SecureInputStart, Ok(()));
    assert_eq!(
        collectors
            .health()
            .degraded
            .get("secure_input")
            .map(String::as_str),
        Some("macOS Secure Input is active; input.key delivery is suspended"),
        "runtime degradation becomes visible only after the producer recovers"
    );

    let mut restart_collision = CollectorSet::new(&input_text_content_test_config());
    restart_collision.set_producer_result_for_test(
        ProducerFailureOrigin::EventTapMainThread,
        Err("producer failure"),
    );
    restart_collision
        .start_errors
        .insert("eventtap".to_owned(), "start failure".to_owned());
    restart_collision
        .eventtap
        .as_mut()
        .expect("EventTap collector")
        .record_unexpected_exit_for_test(Instant::now(), "restart failure");
    restart_collision.set_eventtap_runtime_for_test(true);
    assert_eq!(
        restart_collision
            .health()
            .degraded
            .get("eventtap")
            .map(String::as_str),
        Some("producer failure")
    );
    restart_collision
        .set_producer_result_for_test(ProducerFailureOrigin::EventTapMainThread, Ok(()));
    assert_eq!(
        restart_collision
            .health()
            .degraded
            .get("eventtap")
            .map(String::as_str),
        Some("event capture or wake recovery is unavailable")
    );
    restart_collision.set_eventtap_runtime_for_test(false);
    assert_eq!(
        restart_collision
            .health()
            .degraded
            .get("eventtap")
            .map(String::as_str),
        Some("start failure")
    );
    restart_collision.start_errors.remove("eventtap");
    assert_eq!(
        restart_collision
            .health()
            .degraded
            .get("eventtap")
            .map(String::as_str),
        Some("restart failure")
    );
}

#[test]
fn content_filter_failure_clears_only_on_worker_ack_or_fresh_start() {
    let config = secure_input_enabled_test_config();
    let mut collectors = CollectorSet::new(&config);
    assert!(
        collectors.chrome.is_none(),
        "content lifecycle fixture must not start the system Chrome worker"
    );
    let _observers = collectors.prepare_main_thread();
    let (pipeline, _events) = mpsc::sync_channel(4);
    collectors.start(&pipeline);

    collectors.set_producer_result_for_test(
        ProducerFailureOrigin::ContentSnapshotFilter,
        Err("filter acknowledgement failed"),
    );
    collectors.replace_filter(config.filter.clone());
    assert!(
        !collectors
            .health()
            .degraded
            .contains_key("content_snapshot")
    );

    collectors.set_producer_result_for_test(
        ProducerFailureOrigin::ContentSnapshotFilter,
        Err("filter acknowledgement failed while running"),
    );
    collectors.suspend();
    collectors.replace_filter(config.filter.clone());
    assert_eq!(
        collectors
            .health()
            .degraded
            .get("content_snapshot")
            .map(String::as_str),
        Some("filter acknowledgement failed while running"),
        "worker absence is not an acknowledgement"
    );

    collectors
        .supervise(&pipeline, Some(&granted_permissions()), Instant::now())
        .expect("recreate content worker through supervision");
    assert!(
        !collectors
            .health()
            .degraded
            .contains_key("content_snapshot"),
        "a successfully recreated worker owns the current filter"
    );
    collectors.suspend();
}

#[test]
fn removing_chrome_preserves_every_other_failure_origin() {
    let mut config = secure_input_enabled_test_config();
    config.filter.content_snapshot.exclude_apps.clear();
    let mut collectors = CollectorSet::new(&config);
    assert!(collectors.chrome.is_some());
    for (origin, component) in PRODUCER_FAILURE_CASES {
        collectors.set_producer_result_for_test(origin, Err(component));
    }
    collectors
        .start_errors
        .insert("chrome".to_owned(), "Chrome start failure".to_owned());
    collectors
        .start_errors
        .insert("ax".to_owned(), "AX start failure".to_owned());

    config
        .filter
        .content_snapshot
        .exclude_apps
        .push(CHROME_BUNDLE_ID.to_owned());
    collectors.replace_filter(config.filter);

    assert!(collectors.chrome.is_none());
    assert!(!collectors.start_errors.contains_key("chrome"));
    assert_eq!(
        collectors.start_errors.get("ax").map(String::as_str),
        Some("AX start failure")
    );
    let remaining: BTreeMap<_, _> = collectors.producer_failures.reasons().collect();
    for (_, component) in PRODUCER_FAILURE_CASES {
        assert_eq!(remaining.get(component), Some(&component));
    }
}

#[test]
fn secure_input_start_failure_survives_suspend_and_resume() {
    let mut collectors = collector_set_with_secure_input_result(Err("monitor unavailable"));
    let (pipeline, _events) = mpsc::sync_channel(4);

    collectors.suspend();
    collectors.start(&pipeline);

    assert_secure_input_failure(&collectors, "monitor unavailable");
}

#[test]
fn secure_input_start_failure_survives_pause_and_unpause() {
    let mut collectors = collector_set_with_secure_input_result(Err("monitor unavailable"));
    let (pipeline, _events) = mpsc::sync_channel(4);

    collectors.stop();
    collectors.start(&pipeline);

    assert_secure_input_failure(&collectors, "monitor unavailable");
}

#[test]
fn secure_input_start_failure_survives_filter_reload() {
    let mut collectors = collector_set_with_secure_input_result(Err("monitor unavailable"));

    collectors.replace_filter(zanei_core::config::FilterConfig::default());

    assert_secure_input_failure(&collectors, "monitor unavailable");
}

#[test]
fn filter_reload_does_not_clear_managed_content_start_failure() {
    let config = secure_input_enabled_test_config();
    let mut collectors = CollectorSet::new(&config);
    collectors.start_errors.insert(
        "content_snapshot".to_owned(),
        "content worker failed to start".to_owned(),
    );

    collectors.replace_filter(config.filter);

    assert_eq!(
        collectors
            .start_errors
            .get("content_snapshot")
            .map(String::as_str),
        Some("content worker failed to start")
    );
}

#[test]
fn recreated_secure_input_owner_clears_recovered_start_failure() {
    let config = secure_input_enabled_test_config();
    let failed =
        collector_set_with_config_and_secure_input_result(&config, Err("monitor unavailable"));
    assert_secure_input_failure(&failed, "monitor unavailable");

    let recovered = collector_set_with_config_and_secure_input_result(&config, Ok(()));

    assert!(!recovered.health().degraded.contains_key("secure_input"));
}

#[test]
fn disabling_secure_input_consumers_removes_the_owned_failure() {
    let failed = collector_set_with_config_and_secure_input_result(
        &secure_input_enabled_test_config(),
        Err("monitor unavailable"),
    );
    assert_secure_input_failure(&failed, "monitor unavailable");

    let config = collector_lifecycle_test_config();
    let disabled = CollectorSet::new(&config);

    assert!(!disabled.health().degraded.contains_key("secure_input"));
}

#[test]
fn chrome_health_projects_failure_recovery_and_exit_priority() {
    let mut config = zanei_core::config::Config::default();
    config.capture.sources = vec![CaptureSource::Browser];
    let mut collectors = CollectorSet::new(&config);
    let failure = ChromeFailure::Query(ChromeQueryFailure::AppleEvent(-1712));
    let chrome = collectors.chrome.as_mut().expect("Chrome collector");

    chrome.set_health_for_test(true, ChromeFailureState::Unavailable(failure));
    assert_eq!(
        collectors
            .health()
            .degraded
            .get("chrome")
            .map(String::as_str),
        Some("state=unavailable phase=query kind=apple_event code=-1712")
    );

    collectors
        .chrome
        .as_mut()
        .expect("Chrome collector")
        .set_health_for_test(true, ChromeFailureState::Available);
    assert!(!collectors.health().degraded.contains_key("chrome"));

    let chrome = collectors.chrome.as_mut().expect("Chrome collector");
    chrome.record_unexpected_exit_for_test(
        Instant::now(),
        "collector worker terminated unexpectedly",
    );
    chrome.set_health_for_test(true, ChromeFailureState::Unavailable(failure));
    assert_eq!(
        collectors
            .health()
            .degraded
            .get("chrome")
            .map(String::as_str),
        Some("state=unavailable phase=query kind=apple_event code=-1712")
    );

    let chrome = collectors.chrome.as_mut().expect("Chrome collector");
    chrome.set_health_for_test(true, ChromeFailureState::Available);
    assert_eq!(
        collectors
            .health()
            .degraded
            .get("chrome")
            .map(String::as_str),
        Some("collector worker terminated unexpectedly")
    );
}

#[test]
fn supervisor_starts_a_collector_added_after_startup_on_its_next_tick() {
    let state = Arc::new(FakeState::default());
    let mut managed = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::new(),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();

    supervise_collector(&mut managed, &pipeline, None, &mut errors, Instant::now())
        .expect("supervise late collector");

    assert_eq!(state.starts.load(Ordering::Relaxed), 1);
    state.finish();
    wait_for_relay(&managed);
}

#[test]
fn collector_supervision_continues_while_permission_snapshot_is_pending() {
    let state = Arc::new(FakeState::default());
    let mut managed = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::new(),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();
    let started = Instant::now();
    start_collector(&mut managed, &pipeline, &mut errors, started);
    state.finish();
    wait_for_relay(&managed);

    supervise_collector(&mut managed, &pipeline, None, &mut errors, started)
        .expect("observe collector while permissions are pending");
    supervise_collector(
        &mut managed,
        &pipeline,
        None,
        &mut errors,
        started + Duration::from_secs(5),
    )
    .expect("restart collector while permissions are pending");

    assert_eq!(state.starts.load(Ordering::Relaxed), 2);
    state.finish();
    wait_for_relay(&managed);
}

#[test]
fn pending_snapshot_observes_permission_required_failure_without_restarting() {
    let state = Arc::new(FakeState::default());
    let mut managed = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::from([Capability::ReadAccessibilityTree]),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();
    let started = Instant::now();
    start_collector(&mut managed, &pipeline, &mut errors, started);
    state.finish();
    wait_for_relay(&managed);

    supervise_collector(&mut managed, &pipeline, None, &mut errors, started)
        .expect("observe collector while permissions are pending");
    supervise_collector(
        &mut managed,
        &pipeline,
        None,
        &mut errors,
        started + Duration::from_secs(60),
    )
    .expect("hold restart while permissions are pending");
    assert_eq!(state.starts.load(Ordering::Relaxed), 1);
    assert!(
        managed
            .as_ref()
            .and_then(Managed::restart_degraded_reason)
            .is_some()
    );

    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        started + Duration::from_secs(61),
    )
    .expect("restart after permission snapshot is granted");
    assert_eq!(state.starts.load(Ordering::Relaxed), 2);
    state.finish();
    wait_for_relay(&managed);
}

#[test]
fn collector_start_failure_is_recorded_without_failing_the_daemon_start() {
    let state = Arc::new(FakeState::default());
    let mut managed = Some(Managed::new(FakeCollector::failing(Arc::clone(&state))));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();

    super::start_collector(&mut managed, &pipeline, &mut errors, Instant::now());

    assert_eq!(state.starts.load(Ordering::Relaxed), 1);
    assert_eq!(errors["fake"], "missing permission");
}

#[test]
fn permission_blocked_collector_waits_for_granted_transition() {
    let state = Arc::new(FakeState::default());
    let mut managed = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::from([Capability::ReadAccessibilityTree]),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();
    let started = Instant::now();
    super::start_collector(&mut managed, &pipeline, &mut errors, started);
    state.finish();
    wait_for_relay(&managed);

    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&denied_permissions()),
        &mut errors,
        started,
    )
    .expect("record permission failure");
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&denied_permissions()),
        &mut errors,
        started + Duration::from_secs(60),
    )
    .expect("hold restart");
    assert_eq!(state.starts.load(Ordering::Relaxed), 1);

    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        started + Duration::from_secs(61),
    )
    .expect("permission recovery");
    assert_eq!(state.starts.load(Ordering::Relaxed), 2);
    assert!(
        managed
            .as_ref()
            .and_then(Managed::restart_degraded_reason)
            .is_some()
    );

    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        started + Duration::from_secs(121),
    )
    .expect("stable permission recovery");
    assert!(
        managed
            .as_ref()
            .and_then(Managed::restart_degraded_reason)
            .is_none()
    );

    state.finish();
    wait_for_relay(&managed);
}

#[test]
fn deferred_browser_automation_keeps_the_worker_stopped() {
    let state = Arc::new(FakeState::default());
    let mut managed = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::from([Capability::AutomateBrowser]),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();
    let started = Instant::now();
    start_collector(&mut managed, &pipeline, &mut errors, started);
    state.finish();
    wait_for_relay(&managed);

    let mut deferred = granted_permissions();
    deferred.permissions_ok = true;
    deferred
        .automation
        .insert(CHROME_BUNDLE_ID.to_owned(), PermissionState::NotDetermined);
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&deferred),
        &mut errors,
        started,
    )
    .expect("record browser worker exit");
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&deferred),
        &mut errors,
        started + Duration::from_secs(60),
    )
    .expect("hold browser restart while automation is deferred");
    assert_eq!(state.starts.load(Ordering::Relaxed), 1);

    deferred
        .automation
        .insert(CHROME_BUNDLE_ID.to_owned(), PermissionState::Granted);
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&deferred),
        &mut errors,
        started + Duration::from_secs(61),
    )
    .expect("restart browser worker after automation is granted");
    assert_eq!(state.starts.load(Ordering::Relaxed), 2);

    state.finish();
    wait_for_relay(&managed);
}

#[test]
fn backoff_deadline_rechecks_current_permission_before_restart() {
    let state = Arc::new(FakeState::default());
    let mut managed = Some(Managed::new(FakeCollector::new(
        Arc::clone(&state),
        BTreeSet::from([Capability::ReadAccessibilityTree]),
    )));
    let (pipeline, _events) = mpsc::sync_channel(4);
    let mut errors = BTreeMap::new();
    let mut clock = FakeClock::new();
    start_collector(&mut managed, &pipeline, &mut errors, clock.now);
    state.finish();
    wait_for_relay(&managed);

    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        clock.now,
    )
    .expect("schedule restart while granted");
    clock.advance(Duration::from_secs(5));
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&denied_permissions()),
        &mut errors,
        clock.now,
    )
    .expect("hold restart at deadline while denied");
    clock.advance(Duration::from_secs(60));
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&denied_permissions()),
        &mut errors,
        clock.now,
    )
    .expect("hold overdue restart while denied");
    assert_eq!(state.starts.load(Ordering::Relaxed), 1);

    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        clock.now,
    )
    .expect("restart immediately after regrant");
    assert_eq!(state.starts.load(Ordering::Relaxed), 2);

    state.finish();
    wait_for_relay(&managed);
}

fn projected_degradation(
    managed: &Option<Managed<FakeCollector>>,
    errors: &BTreeMap<String, String>,
) -> Option<String> {
    let mut degraded = errors.clone();
    add_restart_degradation(&mut degraded, managed.as_ref());
    degraded.remove("fake")
}

fn collector_set_with_secure_input_result(result: Result<(), &str>) -> CollectorSet {
    let collectors = collector_set_with_config_and_secure_input_result(
        &secure_input_enabled_test_config(),
        result,
    );
    assert!(
        collectors.chrome.is_none(),
        "Secure Input lifecycle fixture must not start the system Chrome worker"
    );
    collectors
}

fn collector_set_with_config_and_secure_input_result(
    config: &zanei_core::config::Config,
    result: Result<(), &str>,
) -> CollectorSet {
    match result {
        Ok(()) => CollectorSet::new(config),
        Err(reason) => CollectorSet::new_with_secure_input_start(config, || Err(reason.to_owned())),
    }
}

fn collector_lifecycle_test_config() -> zanei_core::config::Config {
    let mut config = zanei_core::config::Config::default();
    config.capture.sources.clear();
    config
}

fn secure_input_enabled_test_config() -> zanei_core::config::Config {
    let mut config = collector_lifecycle_test_config();
    config.capture.content_snapshot = true;
    config
        .filter
        .content_snapshot
        .exclude_apps
        .push(CHROME_BUNDLE_ID.to_owned());
    config
}

fn input_text_content_test_config() -> zanei_core::config::Config {
    let mut config = collector_lifecycle_test_config();
    config.capture.sources = vec![CaptureSource::Input];
    config.capture.text_content = true;
    config
        .filter
        .text_content
        .exclude_apps
        .push(CHROME_BUNDLE_ID.to_owned());
    config
}

fn assert_secure_input_failure(collectors: &CollectorSet, expected: &str) {
    assert_eq!(
        collectors
            .health()
            .degraded
            .get("secure_input")
            .map(String::as_str),
        Some(expected)
    );
}

fn assert_exit_survives_collector_set_resume(suspend: bool) {
    let mut config = zanei_core::config::Config::default();
    config.capture.sources = vec![CaptureSource::Browser];
    let mut collectors = CollectorSet::new(&config);
    let (pipeline, _events) = mpsc::sync_channel(4);
    collectors
        .chrome
        .as_mut()
        .expect("Chrome collector")
        .record_unexpected_exit_for_test(
            Instant::now(),
            "collector worker terminated unexpectedly",
        );

    if suspend {
        collectors.suspend();
    } else {
        collectors.stop();
    }
    assert_eq!(
        collectors
            .health()
            .degraded
            .get("chrome")
            .map(String::as_str),
        Some("collector worker terminated unexpectedly")
    );
    let resumed_at = Instant::now();
    collectors
        .chrome
        .as_mut()
        .expect("Chrome collector")
        .record_started_for_test(resumed_at);
    let mut errors = BTreeMap::new();

    supervise_collector(
        &mut collectors.chrome,
        &pipeline,
        None,
        &mut errors,
        resumed_at + Duration::from_secs(59),
    )
    .expect("observe resumed Chrome collector before stable threshold");
    assert!(collectors.health().degraded.contains_key("chrome"));
    supervise_collector(
        &mut collectors.chrome,
        &pipeline,
        None,
        &mut errors,
        resumed_at + Duration::from_secs(60),
    )
    .expect("observe stable resumed Chrome collector");
    assert!(
        !collectors.health().degraded.contains_key("chrome"),
        "unexpected exit clears only after the resumed worker is stable"
    );

    collectors.suspend();
}
