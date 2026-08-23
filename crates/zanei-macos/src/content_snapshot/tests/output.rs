use std::{
    sync::mpsc::sync_channel,
    time::{Duration, Instant},
};

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
        budget::GLOBAL_SAVE_INTERVAL,
        output::{emit, emit_released},
        scheduler::ScheduledSnapshot,
        state::{SaveBlock, SnapshotState, SnapshotWindowKey},
    },
    text_capture::TextQuarantine,
};

use super::support::trigger;

#[test]
fn v2_1_chrome_snapshot_without_version_is_dropped() {
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
    let text = "must not bypass confirmation".to_owned();
    let hash = SnapshotState::text_hash(&text);
    let output = SnapshotWalkOutput {
        text,
        nodes: 1,
        ax_calls: 1,
        elapsed: Duration::ZERO,
        complete: true,
        cutoff: None,
        degraded_nodes: 0,
        frameless_nodes: 0,
    };
    let mut state = SnapshotState::new(now);
    let health = SharedHealth::default();
    let (sender, events) = sync_channel(1);
    let mut quarantine = TextQuarantine::new(ChromeObserver::new());

    emit(
        candidate,
        output,
        key,
        hash,
        Default::default(),
        None,
        time::OffsetDateTime::UNIX_EPOCH,
        now,
        &mut state,
        &sender,
        &health,
        &mut quarantine,
    );

    assert!(events.try_recv().is_err());
    assert_eq!(state.daily_bytes(now), 0);
}

#[test]
fn snapshot_ts_quarantine_release_preserves_candidate_time_and_reservation() {
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
        frameless_nodes: 0,
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
        time::OffsetDateTime::UNIX_EPOCH,
        now,
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
            now,
        ),
        Err(SaveBlock::GlobalInterval)
    );
    let reserved_bytes = u64::try_from(bytes).expect("snapshot size fits u64");
    assert_eq!(state.daily_bytes(now), reserved_bytes);
    assert!(events.try_recv().is_err(), "snapshot remains quarantined");

    eligibility.observe(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(11),
            url: "https://allowed.example/confirmed".to_owned(),
        },
    );
    let released = quarantine.release(now + std::time::Duration::from_millis(1), &policy);
    emit_released(released, &sender, &health, &mut state);

    let event = events.try_recv().expect("confirmed snapshot is delivered");
    assert_eq!(event.observed_at, Some(time::OffsetDateTime::UNIX_EPOCH));
    assert_eq!(
        state.daily_bytes(now),
        reserved_bytes,
        "release must not commit the reservation twice"
    );
}

#[test]
fn dropped_snapshot_does_not_deduplicate_identical_settle_on_return() {
    let now = Instant::now();
    let mut target = trigger(7, 11, SnapshotTriggerKind::Focus, now);
    target.app.name = "Google Chrome".to_owned();
    target.app.bundle_id = Some(CHROME_BUNDLE_ID.to_owned());
    let candidate = ScheduledSnapshot {
        target,
        trigger: ContentSnapshotTrigger::FocusOut,
        activity_window: None,
    };
    let key = SnapshotWindowKey {
        pid: 7,
        window_id: 11,
    };
    let text = "identical after return".to_owned();
    let bytes = text.len();
    let hash = SnapshotState::text_hash(&text);
    let output = SnapshotWalkOutput {
        text,
        nodes: 1,
        ax_calls: 1,
        elapsed: Duration::ZERO,
        complete: true,
        cutoff: None,
        degraded_nodes: 0,
        frameless_nodes: 0,
    };
    let filter = FilterConfig::default();
    let (eligibility, tracker) = chrome_eligibility_channel(filter.clone());
    eligibility.observe_at(
        7,
        ChromeEligibilityObservation::Normal {
            window_id: Some(11),
            url: "https://allowed.example/initial".to_owned(),
        },
        now - Duration::from_millis(1),
    );
    let policy = CapturePolicy::new(tracker, filter, None);
    let decision = policy.decision(
        PrivacyScope::ContentSnapshot,
        &candidate.target.app.raw_app(),
        Some(11),
    );
    let mut quarantine = TextQuarantine::new(ChromeObserver::new());
    let mut state = SnapshotState::new(now);
    let health = SharedHealth::default();
    let (sender, events) = sync_channel(1);

    emit(
        candidate,
        output,
        key,
        hash,
        decision.capture_context(),
        decision.chrome_version(),
        time::OffsetDateTime::UNIX_EPOCH,
        now,
        &mut state,
        &sender,
        &health,
        &mut quarantine,
    );
    eligibility.observe_at(
        7,
        ChromeEligibilityObservation::Unavailable {
            window_id: Some(11),
        },
        now + Duration::from_millis(1),
    );
    assert!(
        quarantine
            .release(now + Duration::from_millis(1), &policy)
            .is_empty()
    );
    assert!(events.try_recv().is_err());

    let return_at = now + GLOBAL_SAVE_INTERVAL;
    assert_eq!(state.evaluate_save(key, hash, bytes, return_at), Ok(()));
    state.reserve(key, bytes, return_at);
    state.record_hash(key, hash);
    assert_eq!(
        state.evaluate_save(key, hash, bytes, return_at + GLOBAL_SAVE_INTERVAL),
        Err(SaveBlock::Duplicate)
    );
}
