use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{channel, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

use zanei_core::{config::FilterConfig, privacy::PrivacyScope};

use crate::{
    CapturePolicy,
    chrome::{ChromeObserver, chrome_eligibility_channel},
    content_snapshot::{
        Control, SharedHealth, SnapshotAxError, SnapshotTriggerKind, SnapshotWalkOutput,
        scheduler::{SETTLE_QUIET_INTERVAL, SnapshotScheduler},
        snapshot_trigger_channel,
        state::SnapshotState,
        tests::walker::FakeNode,
        worker::{
            SnapshotApplication, run_worker_with_scanner, scan_application, service_controls,
        },
    },
    focus_context::FocusContext,
    secure_input::secure_input_test_channel,
    workspace::notification_channel,
};

use super::support::trigger;

#[derive(Clone)]
struct FakeApplication {
    focused: Option<FakeNode>,
    windows: Vec<FakeNode>,
    windows_reads: Arc<AtomicUsize>,
}

impl SnapshotApplication for FakeApplication {
    type Window = FakeNode;

    fn pid(&self) -> i32 {
        7
    }

    fn focused_window(&self) -> Result<Option<Self::Window>, SnapshotAxError> {
        Ok(self.focused.clone())
    }

    fn windows(&self) -> Result<Vec<Self::Window>, SnapshotAxError> {
        self.windows_reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.windows.clone())
    }
}

fn application(focused_window_id: i64) -> FakeApplication {
    let focused = FakeNode::numbered_window(focused_window_id, "Focused");
    let other = FakeNode::numbered_window(22, "Previous");
    FakeApplication {
        focused: Some(focused.clone()),
        windows: vec![focused, other],
        windows_reads: Arc::new(AtomicUsize::new(0)),
    }
}

#[test]
fn non_focused_candidate_window_is_walked_by_window_id() {
    let app = application(11);
    let reads = Arc::clone(&app.windows_reads);
    let stop = AtomicBool::new(false);
    let output = scan_application(app, 22, &stop, |_, _| None)
        .expect("scan non-focused window")
        .expect("candidate window");

    assert_eq!(output.text, "Previous");
    assert_eq!(reads.load(Ordering::Relaxed), 1);

    let focused_output = scan_application(application(22), 22, &stop, |_, _| None)
        .expect("scan focused window")
        .expect("focused candidate");
    assert_eq!(
        output.ax_calls,
        focused_output.ax_calls + 3,
        "AXWindows and enumerated-window reads must be counted"
    );
}

#[test]
fn unknown_candidate_window_id_fails_closed() {
    let app = application(11);
    let reads = Arc::clone(&app.windows_reads);

    assert!(
        scan_application(app, 33, &AtomicBool::new(false), |_, _| None)
            .expect("scan missing window")
            .is_none()
    );
    assert_eq!(reads.load(Ordering::Relaxed), 1);
}

#[test]
fn filter_reload_rearms_focused_target_and_clears_pid_backoff() {
    let observed_at = Instant::now();
    let reload_at = observed_at + Duration::from_secs(12);
    let target = trigger(7, 11, SnapshotTriggerKind::Focus, observed_at);
    let excluded_filter = FilterConfig {
        exclude_apps: vec!["dev.example.App".to_owned()],
        ..FilterConfig::default()
    };
    let (_chrome_publisher, chrome) = chrome_eligibility_channel(excluded_filter.clone());
    let policy = CapturePolicy::new(chrome, excluded_filter, None);
    assert!(
        !policy
            .decision(
                PrivacyScope::ContentSnapshot,
                &target.app.raw_app(),
                target.window.id,
            )
            .is_allowed()
    );

    let mut scheduler = SnapshotScheduler::default();
    scheduler.observe(target.clone());
    let mut state = SnapshotState::new(observed_at);
    state.record_failure(7, observed_at, true);
    assert!(!state.backoff_allows(7, observed_at));

    policy.replace_filter(FilterConfig::default());
    assert!(
        policy
            .decision(
                PrivacyScope::ContentSnapshot,
                &target.app.raw_app(),
                target.window.id,
            )
            .is_allowed()
    );

    let (control, controls) = channel();
    let (acknowledge, acknowledged) = sync_channel(1);
    control
        .send(Control::ReplaceFilter { acknowledge })
        .expect("send filter replacement");

    assert!(!service_controls(
        &controls,
        &mut scheduler,
        &mut state,
        reload_at,
    ));
    acknowledged
        .recv()
        .expect("filter replacement acknowledged");
    assert_eq!(
        scheduler.next_deadline(),
        Some(reload_at + SETTLE_QUIET_INTERVAL)
    );
    assert!(state.backoff_allows(7, reload_at));
}

#[test]
fn secure_input_enabled_after_walk_discards_the_snapshot() {
    let (trigger_publisher, trigger_receiver) = snapshot_trigger_channel();
    let (_lifecycle_publisher, lifecycle_receiver) = notification_channel();
    let (_control, controls) = channel();
    let (sender, events) = sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let focus_context = FocusContext::new();
    let target = trigger(
        7,
        11,
        SnapshotTriggerKind::Focus,
        Instant::now() - Duration::from_secs(3),
    );
    focus_context.activate(target.app.clone(), Some(target.window.clone()));
    let filter = FilterConfig::default();
    let (_chrome_publisher, chrome) = chrome_eligibility_channel(filter.clone());
    let (secure_input, secure_responder) = secure_input_test_channel();
    let policy = CapturePolicy::new(chrome, filter, Some(secure_input));
    let scan_calls = Arc::new(AtomicUsize::new(0));
    let observed_scan_calls = Arc::clone(&scan_calls);
    let worker_stop = Arc::clone(&stop);
    let worker = thread::Builder::new()
        .name("zanei-content".to_owned())
        .spawn(move || {
            let mut state = SnapshotState::new(Instant::now());
            run_worker_with_scanner(
                &trigger_receiver,
                &lifecycle_receiver,
                controls,
                worker_stop,
                sender,
                policy,
                ChromeObserver::new(),
                SharedHealth::default(),
                &mut state,
                focus_context,
                move |_pid, _window_id, _stop| {
                    observed_scan_calls.fetch_add(1, Ordering::Relaxed);
                    Ok(Some(SnapshotWalkOutput {
                        text: "private".to_owned(),
                        nodes: 1,
                        ax_calls: 1,
                        elapsed: Duration::from_millis(1),
                        complete: true,
                        cutoff: None,
                        degraded_nodes: 0,
                        frameless_nodes: 0,
                    }))
                },
            );
        })
        .expect("spawn content worker");
    let secure_worker = thread::spawn(move || {
        secure_responder.respond_next(false);
        secure_responder.respond_next(true);
    });

    assert!(trigger_publisher.publish(target));
    secure_worker.join().expect("Secure Input responder");
    stop.store(true, Ordering::Release);
    worker.join().expect("content worker");

    assert_eq!(scan_calls.load(Ordering::Relaxed), 1);
    assert!(events.try_recv().is_err());
}
