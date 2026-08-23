use std::{sync::mpsc::sync_channel, time::Instant};

use zanei_core::{
    config::FilterConfig,
    privacy::{CHROME_BUNDLE_ID, PrivacyScope},
    schema::ContentSnapshotTrigger,
};

use crate::{
    CapturePolicy,
    chrome::{ChromeEligibilityObservation, ChromeObserver, chrome_eligibility_channel},
    content_snapshot::{
        SharedHealth, SnapshotTriggerKind, SnapshotWalkOutput,
        output::{emit, emit_released},
        scheduler::ScheduledSnapshot,
        state::{SaveBlock, SnapshotState, SnapshotWindowKey},
    },
    text_capture::TextQuarantine,
};

use super::support::trigger;

#[test]
fn quarantined_snapshot_reserves_interval_and_budget_without_double_commit() {
    let now = Instant::now();
    let mut target = trigger(7, 11, SnapshotTriggerKind::Focus, now);
    target.app.name = "Google Chrome".to_owned();
    target.app.bundle_id = Some(CHROME_BUNDLE_ID.to_owned());
    let candidate = ScheduledSnapshot {
        target,
        trigger: ContentSnapshotTrigger::Settle,
        activity_window: None,
    };
    let key = SnapshotWindowKey {
        pid: 7,
        window_id: 11,
    };
    let text = "held snapshot".to_owned();
    let bytes = text.len();
    let hash = SnapshotState::text_hash(&text);
    let output = SnapshotWalkOutput {
        text,
        nodes: 1,
        ax_calls: 1,
        elapsed: std::time::Duration::ZERO,
        complete: true,
        cutoff: None,
        degraded_nodes: 0,
    };
    let filter = FilterConfig::default();
    let (eligibility, tracker) = chrome_eligibility_channel(filter.clone());
    eligibility.observe(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(11),
            url: "https://allowed.example/initial".to_owned(),
        },
    );
    let policy = CapturePolicy::new(tracker, filter, None);
    let decision = policy.decision(
        PrivacyScope::ContentSnapshot,
        &candidate.target.app.raw_app(),
        Some(11),
    );
    let observer = ChromeObserver::new();
    let mut quarantine = TextQuarantine::new(observer);
    let mut state = SnapshotState::new(now);
    let health = SharedHealth::default();
    let (sender, events) = sync_channel(2);

    emit(
        candidate,
        output,
        key,
        hash,
        decision.capture_context(),
        decision.chrome_version(),
        &mut state,
        &sender,
        &health,
        &mut quarantine,
    );

    assert_eq!(
        state.evaluate_save(
            SnapshotWindowKey {
                pid: 8,
                window_id: 12,
            },
            SnapshotState::text_hash("next"),
            4,
            Instant::now(),
        ),
        Err(SaveBlock::GlobalInterval)
    );
    let reserved_bytes = u64::try_from(bytes).expect("snapshot size fits u64");
    assert_eq!(state.daily_bytes(Instant::now()), reserved_bytes);
    assert!(events.try_recv().is_err(), "snapshot remains quarantined");

    eligibility.observe(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(11),
            url: "https://allowed.example/confirmed".to_owned(),
        },
    );
    let released = quarantine.release(Instant::now(), &policy);
    emit_released(released, &sender, &health);

    assert!(events.try_recv().is_ok(), "confirmed snapshot is delivered");
    assert_eq!(
        state.daily_bytes(Instant::now()),
        reserved_bytes,
        "release must not commit the reservation twice"
    );
}
