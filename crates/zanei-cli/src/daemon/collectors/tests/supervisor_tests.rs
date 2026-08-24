use super::*;
use crate::daemon::supervisor::chrome_failure_reason;
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
        assert!(errors["fake"].contains("terminated unexpectedly"));

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
        assert!(errors.contains_key("fake"));
    }

    state.finish();
    wait_for_relay(&managed);
}

#[test]
fn unexpected_exit_clears_after_sixty_seconds_of_stable_runtime() {
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
    .expect("observe failed collector");

    clock.advance(Duration::from_secs(5));
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        clock.now,
    )
    .expect("restart collector");
    assert!(errors.contains_key("fake"));

    clock.advance(Duration::from_secs(59));
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        clock.now,
    )
    .expect("observe nearly stable collector");
    assert!(errors.contains_key("fake"));

    clock.advance(Duration::from_secs(1));
    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        clock.now,
    )
    .expect("observe stable collector");
    assert!(!errors.contains_key("fake"));

    state.finish();
    wait_for_relay(&managed);
}

#[test]
fn chrome_current_failure_clears_only_after_observed_recovery() {
    let failure = ChromeFailure::Query(ChromeQueryFailure::AppleEvent(-1712));

    assert_eq!(
        chrome_failure_reason(ChromeFailureState::Unavailable(failure)).as_deref(),
        Some("state=unavailable phase=query kind=apple_event code=-1712")
    );
    assert_eq!(chrome_failure_reason(ChromeFailureState::Available), None);
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
        BTreeSet::from([Permission::Accessibility]),
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
    assert!(errors["fake"].contains("terminated unexpectedly"));

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
        BTreeSet::from([Permission::Accessibility]),
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
    assert!(errors.contains_key("fake"));

    supervise_collector(
        &mut managed,
        &pipeline,
        Some(&granted_permissions()),
        &mut errors,
        started + Duration::from_secs(121),
    )
    .expect("stable permission recovery");
    assert!(!errors.contains_key("fake"));

    state.finish();
    wait_for_relay(&managed);
}
