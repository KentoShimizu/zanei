use std::{
    sync::mpsc::sync_channel,
    time::{Duration, Instant},
};

use zanei_core::{
    config::{
        CapturePolicyConfig, FilterConfig,
        capture_policy::{BrowserMode, BrowserPolicy, IdePolicy, PolicyAction},
    },
    privacy::{CHROME_BUNDLE_ID, PrivacyScope},
    schema::{ContentSnapshotTrigger, EventData},
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
    permission::SAFARI_BUNDLE_ID,
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
        cutoff: None,
        degraded_nodes: 0,
        frameless_nodes: 0,
    };
    let mut state = SnapshotState::new(now);
    let health = SharedHealth::default();
    let (sender, events) = sync_channel(1);
    let mut quarantine = TextQuarantine::new(ChromeObserver::new());
    let filter = FilterConfig::default();
    let (_, tracker) = chrome_eligibility_channel(filter.clone());
    let policy = CapturePolicy::new(tracker, filter, None);
    let decision = policy.decision(
        PrivacyScope::ContentSnapshot,
        &candidate.target.app.raw_app(),
        Some(11),
        candidate.target.window.title.as_deref(),
    );

    emit(
        candidate,
        output,
        key,
        hash,
        &policy,
        &decision,
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
        candidate.target.window.title.as_deref(),
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
        &policy,
        &decision,
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
            url: "https://allowed.example/initial".to_owned(),
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
        candidate.target.window.title.as_deref(),
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
        &policy,
        &decision,
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

#[test]
fn safari_snapshot_delivery_obeys_confirmation_mode() {
    let key = SnapshotWindowKey {
        pid: 7,
        window_id: 11,
    };

    let now = Instant::now();
    let standalone = standalone_safari_filter();
    let (_, tracker) = chrome_eligibility_channel(standalone.clone());
    let policy = CapturePolicy::new(tracker, standalone, None);
    let candidate = safari_candidate(now);
    let decision = policy.decision(
        PrivacyScope::ContentSnapshot,
        &candidate.target.app.raw_app(),
        Some(11),
        candidate.target.window.title.as_deref(),
    );
    let mut state = SnapshotState::new(now);
    let health = SharedHealth::default();
    let (sender, events) = sync_channel(1);
    let mut quarantine = TextQuarantine::new(ChromeObserver::new());
    emit(
        candidate,
        snapshot_output("standalone"),
        key,
        SnapshotState::text_hash("standalone"),
        &policy,
        &decision,
        time::OffsetDateTime::UNIX_EPOCH,
        now,
        &mut state,
        &sender,
        &health,
        &mut quarantine,
    );
    let event = events.try_recv().expect("standalone Safari is immediate");
    let EventData::ContentSnapshot(data) = event.data else {
        panic!("content.snapshot")
    };
    assert_eq!(data.text.as_deref(), Some("standalone"));

    let now = Instant::now();
    let filter = app_owned_safari_filter();
    let (publisher, tracker) = chrome_eligibility_channel(filter.clone());
    publisher.observe_at(
        7,
        safari_observation("https://same.example/path"),
        now - Duration::from_millis(1),
    );
    let policy = CapturePolicy::new(tracker, filter, None);
    let candidate = safari_candidate(now);
    let decision = policy.decision(
        PrivacyScope::ContentSnapshot,
        &candidate.target.app.raw_app(),
        Some(11),
        candidate.target.window.title.as_deref(),
    );
    let text = "app-owned";
    let mut state = SnapshotState::new(now);
    let health = SharedHealth::default();
    let (sender, events) = sync_channel(1);
    let mut quarantine = TextQuarantine::new(ChromeObserver::new());
    emit(
        candidate,
        snapshot_output(text),
        key,
        SnapshotState::text_hash(text),
        &policy,
        &decision,
        time::OffsetDateTime::UNIX_EPOCH,
        now,
        &mut state,
        &sender,
        &health,
        &mut quarantine,
    );
    assert!(events.try_recv().is_err(), "app-owned Safari is held");
    let reserved = state.daily_bytes(now);
    publisher.observe_at(
        7,
        safari_observation("https://same.example/path"),
        now + Duration::from_millis(1),
    );
    emit_released(
        quarantine.release(now + Duration::from_millis(2), &policy),
        &sender,
        &health,
        &mut state,
    );
    let event = events.try_recv().expect("same Safari URL is released");
    let EventData::ContentSnapshot(data) = &event.data else {
        panic!("content.snapshot")
    };
    assert_eq!(data.text.as_deref(), Some(text));
    assert_eq!(
        event.capture_context.url.as_deref(),
        Some("https://same.example/path")
    );
    assert_eq!(state.daily_bytes(now), reserved, "release reserves once");

    let now = Instant::now();
    let standalone = standalone_safari_filter();
    let (publisher, tracker) = chrome_eligibility_channel(standalone.clone());
    let policy = CapturePolicy::new(tracker, standalone, None);
    let candidate = safari_candidate(now);
    let generic = policy.decision(
        PrivacyScope::ContentSnapshot,
        &candidate.target.app.raw_app(),
        Some(11),
        candidate.target.window.title.as_deref(),
    );
    policy.replace_filter(app_owned_safari_filter());
    publisher.observe(7, safari_observation("https://fresh.example/path"));
    let current = policy.decision(
        PrivacyScope::ContentSnapshot,
        &candidate.target.app.raw_app(),
        Some(11),
        candidate.target.window.title.as_deref(),
    );
    assert!(current.is_allowed());
    assert!(current.chrome_version().is_some());
    let mut state = SnapshotState::new(now);
    let health = SharedHealth::default();
    let (sender, events) = sync_channel(1);
    let mut quarantine = TextQuarantine::new(ChromeObserver::new());
    emit(
        candidate,
        snapshot_output("stale generic"),
        key,
        SnapshotState::text_hash("stale generic"),
        &policy,
        &generic,
        time::OffsetDateTime::UNIX_EPOCH,
        now,
        &mut state,
        &sender,
        &health,
        &mut quarantine,
    );
    assert!(events.try_recv().is_err());
    assert_eq!(state.daily_bytes(now), 0);

    let now = Instant::now();
    let filter = app_owned_safari_filter();
    let (publisher, tracker) = chrome_eligibility_channel(filter.clone());
    publisher.observe_at(
        7,
        safari_observation("https://old.example/path"),
        now - Duration::from_millis(1),
    );
    let policy = CapturePolicy::new(tracker, filter, None);
    let candidate = safari_candidate(now);
    let app_owned = policy.decision(
        PrivacyScope::ContentSnapshot,
        &candidate.target.app.raw_app(),
        Some(11),
        candidate.target.window.title.as_deref(),
    );
    policy.replace_filter(standalone_safari_filter());
    let mut state = SnapshotState::new(now);
    let health = SharedHealth::default();
    let (sender, events) = sync_channel(1);
    let mut quarantine = TextQuarantine::new(ChromeObserver::new());
    emit(
        candidate,
        snapshot_output("stale app-owned"),
        key,
        SnapshotState::text_hash("stale app-owned"),
        &policy,
        &app_owned,
        time::OffsetDateTime::UNIX_EPOCH,
        now,
        &mut state,
        &sender,
        &health,
        &mut quarantine,
    );
    assert!(
        quarantine.release(Instant::now(), &policy).is_empty(),
        "app-owned body is discarded after standalone reload"
    );
    assert!(events.try_recv().is_err());
}

fn safari_candidate(now: Instant) -> ScheduledSnapshot {
    let mut target = trigger(7, 11, SnapshotTriggerKind::Focus, now);
    target.app.name = "Safari".to_owned();
    target.app.bundle_id = Some(SAFARI_BUNDLE_ID.to_owned());
    ScheduledSnapshot {
        target,
        trigger: ContentSnapshotTrigger::Settle,
        activity_window: None,
    }
}

fn snapshot_output(text: &str) -> SnapshotWalkOutput {
    SnapshotWalkOutput {
        text: text.to_owned(),
        nodes: 1,
        ax_calls: 1,
        elapsed: Duration::ZERO,
        cutoff: None,
        degraded_nodes: 0,
        frameless_nodes: 0,
    }
}

fn standalone_safari_filter() -> FilterConfig {
    let mut filter = FilterConfig::default();
    filter.content_snapshot.exclude_apps.clear();
    filter
}

fn app_owned_safari_filter() -> FilterConfig {
    let mut filter = FilterConfig {
        capture_policy: Some(CapturePolicyConfig {
            allowed_apps: vec!["Safari".to_owned()],
            browser: BrowserPolicy {
                mode: BrowserMode::AllSites,
                default_policy: PolicyAction::Allow,
                on_url_unavailable: PolicyAction::Block,
                block_auth: false,
                block_payments: false,
                allow_list: Vec::new(),
                block_list: Vec::new(),
            },
            ide: IdePolicy {
                block_env_files: false,
                on_file_name_unavailable: PolicyAction::Allow,
            },
        }),
        ..FilterConfig::default()
    };
    filter.content_snapshot.exclude_apps.clear();
    filter
}

fn safari_observation(url: &str) -> ChromeEligibilityObservation {
    ChromeEligibilityObservation::Safari {
        window_id: Some(11),
        url: Some(url.to_owned()),
    }
}
