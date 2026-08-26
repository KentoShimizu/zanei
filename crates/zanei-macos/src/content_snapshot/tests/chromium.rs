use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{channel, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

use zanei_core::{
    config::FilterConfig,
    privacy::CHROME_BUNDLE_ID,
    schema::{ContentSnapshotTrigger, EventData},
};

use crate::{
    CapturePolicy,
    ax::NativeWindow,
    chrome::{ChromeEligibilityObservation, ChromeObserver, chrome_eligibility_channel},
    content_snapshot::{
        SharedHealth, SnapshotTrigger, SnapshotTriggerKind, snapshot_trigger_channel,
        state::SnapshotState,
        worker::{SnapshotApplication, run_worker_with_scanner, scan_application},
    },
    focus_context::FocusContext,
    secure_input::secure_input_test_channel,
    workspace::{ApplicationActivationPolicy, ApplicationInfo, notification_channel},
};

use super::walker::FakeNode;

struct ChromiumApplication;

impl SnapshotApplication for ChromiumApplication {
    type Window = FakeNode;

    fn pid(&self) -> i32 {
        7
    }

    fn focused_window(
        &self,
    ) -> Result<Option<Self::Window>, crate::content_snapshot::SnapshotAxError> {
        // Chromium activation has no AXFocusedWindow notification; the current
        // window comes from the activation-time focused-window read.
        Ok(Some(FakeNode::chromium_window()))
    }

    fn windows(&self) -> Result<Vec<Self::Window>, crate::content_snapshot::SnapshotAxError> {
        Ok(vec![FakeNode::chromium_window()])
    }
}

fn chrome_app() -> ApplicationInfo {
    ApplicationInfo {
        name: "Google Chrome".to_owned(),
        bundle_id: Some(CHROME_BUNDLE_ID.to_owned()),
        pid: 7,
        activation_policy: ApplicationActivationPolicy::Regular,
    }
}

#[test]
fn chromium_profile_produces_snapshot_through_trigger_scheduler_and_worker() {
    let (trigger_publisher, trigger_receiver) = snapshot_trigger_channel();
    let (_lifecycle_publisher, lifecycle_receiver) = notification_channel();
    let (_control, controls) = channel();
    let (events, output) = sync_channel(4);
    let stop = Arc::new(AtomicBool::new(false));
    let focus_context = FocusContext::new();
    let window = NativeWindow {
        title: Some("Chromium".to_owned()),
        id: Some(11),
    };
    focus_context.activate(chrome_app(), Some(window.clone()));

    let filter = FilterConfig::default();
    let (eligibility, tracker) = chrome_eligibility_channel(filter.clone());
    let initial_observation = Instant::now() - Duration::from_secs(3);
    eligibility.observe_at(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(11),
            url: "https://allowed.example/start".to_owned(),
        },
        initial_observation,
    );
    let (secure_input, secure_responder) = secure_input_test_channel();
    let policy = CapturePolicy::new(tracker, filter, Some(secure_input));
    let health = SharedHealth::default();
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
                events,
                policy,
                ChromeObserver::new(),
                health,
                &mut state,
                focus_context,
                move |_pid, expected_window_id, stop| {
                    observed_scan_calls.fetch_add(1, Ordering::Release);
                    scan_application(
                        ChromiumApplication,
                        expected_window_id,
                        stop,
                        |pid, frame| {
                            assert_eq!(pid, 7);
                            assert_eq!(frame.origin.x, 0.0);
                            Some(11)
                        },
                    )
                },
            );
        })
        .expect("spawn content worker");
    let secure_worker = thread::spawn(move || {
        secure_responder.respond_next(false);
        secure_responder.respond_next(false);
    });

    assert!(trigger_publisher.publish(SnapshotTrigger {
        app: chrome_app(),
        window,
        kind: SnapshotTriggerKind::Focus,
        observed_at: initial_observation,
    }));

    let confirmation = eligibility.clone();
    let confirmer = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(1);
        while scan_calls.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        thread::sleep(Duration::from_millis(50));
        confirmation.observe(
            7,
            ChromeEligibilityObservation::Normal {
                window_id: Some(11),
                url: "https://allowed.example/confirmed".to_owned(),
            },
        );
    });

    let event = output
        .recv_timeout(Duration::from_secs(2))
        .expect("confirmed content.snapshot");
    stop.store(true, Ordering::Release);
    worker.join().expect("content worker");
    secure_worker.join().expect("secure responder");
    confirmer.join().expect("Chrome confirmer");

    let EventData::ContentSnapshot(data) = event.data else {
        panic!("content.snapshot");
    };
    // This single profile covers the C3 bounds fallback, C4 focus-at-trigger
    // binding, C5 post-scan confirmation, count-first leaf traversal, and
    // numeric Chromium AXValue handling. Reverting any gate drops or nulls it.
    assert_eq!(data.trigger, ContentSnapshotTrigger::Settle);
    assert_eq!(data.text.as_deref(), Some("Checked option\nHeading"));
    assert_eq!(data.cutoff(), Some(None));
    assert_eq!(
        event.capture_context.website_host.as_deref(),
        Some("allowed.example")
    );
}
