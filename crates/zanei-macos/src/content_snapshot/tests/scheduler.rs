use std::time::{Duration, Instant};

use zanei_core::schema::ContentSnapshotTrigger;

use crate::content_snapshot::{
    SnapshotTriggerKind,
    scheduler::{
        FOCUS_OUT_MIN_INTERVAL, REFRESH_INTERVALS, SETTLE_MAX_INTERVAL, SETTLE_QUIET_INTERVAL,
        SnapshotScheduler,
    },
};

use super::support::trigger;

#[test]
fn settle_waits_for_quiet_but_never_passes_the_ten_second_cap() {
    let base = Instant::now();
    let mut quiet = SnapshotScheduler::default();
    quiet.observe(trigger(7, 11, SnapshotTriggerKind::Focus, base));
    assert_eq!(quiet.next_deadline(), Some(base + SETTLE_QUIET_INTERVAL));
    assert!(
        quiet
            .take_due(base + SETTLE_QUIET_INTERVAL - Duration::from_millis(1))
            .is_none()
    );
    assert_eq!(
        quiet
            .take_due(base + SETTLE_QUIET_INTERVAL)
            .expect("quiet settle")
            .trigger,
        ContentSnapshotTrigger::Settle
    );

    let mut busy = SnapshotScheduler::default();
    busy.observe(trigger(7, 11, SnapshotTriggerKind::Focus, base));
    for seconds in [1, 3, 5, 7, 9] {
        busy.observe(trigger(
            7,
            11,
            SnapshotTriggerKind::Title,
            base + Duration::from_secs(seconds),
        ));
    }
    assert_eq!(busy.next_deadline(), Some(base + SETTLE_MAX_INTERVAL));
}

#[test]
fn refresh_uses_the_fixed_backoff_and_resets_after_a_change() {
    let base = Instant::now();
    let mut scheduler = SnapshotScheduler::default();
    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::Focus, base));
    assert_eq!(
        scheduler
            .take_due(base + SETTLE_QUIET_INTERVAL)
            .expect("initial settle")
            .trigger,
        ContentSnapshotTrigger::Settle
    );
    let first = scheduler
        .take_due(base + REFRESH_INTERVALS[0])
        .expect("first refresh");
    assert_eq!(first.trigger, ContentSnapshotTrigger::Refresh);
    assert_eq!(first.activity_window, Some(REFRESH_INTERVALS[0]));
    assert_eq!(
        scheduler.next_deadline(),
        Some(base + REFRESH_INTERVALS[0] + REFRESH_INTERVALS[1])
    );

    let changed = base + Duration::from_secs(40);
    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::Title, changed));
    assert_eq!(
        scheduler.next_deadline(),
        Some(changed + SETTLE_QUIET_INTERVAL)
    );
    scheduler.take_due(changed + SETTLE_QUIET_INTERVAL);
    assert_eq!(
        scheduler.next_deadline(),
        Some(changed + REFRESH_INTERVALS[0])
    );
}

#[test]
fn focus_change_schedules_previous_window_and_frequency_predicates_are_exact() {
    let base = Instant::now();
    let mut scheduler = SnapshotScheduler::default();
    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::Focus, base));
    scheduler.observe(trigger(
        8,
        12,
        SnapshotTriggerKind::Focus,
        base + Duration::from_secs(5),
    ));
    let focus_out = scheduler
        .take_due(base + Duration::from_secs(5))
        .expect("focus-out");
    assert_eq!(focus_out.trigger, ContentSnapshotTrigger::FocusOut);
    assert_eq!(focus_out.target.window.id, Some(11));

    assert!(!SnapshotScheduler::focus_out_allows(
        Some(base),
        base + FOCUS_OUT_MIN_INTERVAL - Duration::from_nanos(1)
    ));
    assert!(SnapshotScheduler::focus_out_allows(
        Some(base),
        base + FOCUS_OUT_MIN_INTERVAL
    ));
    assert!(!SnapshotScheduler::global_interval_allows(
        Some(base),
        base + Duration::from_secs(5) - Duration::from_nanos(1)
    ));
    assert!(SnapshotScheduler::global_interval_allows(
        Some(base),
        base + Duration::from_secs(5)
    ));
}

#[test]
fn filter_replacement_rearms_current_target_while_pause_stop_and_wake_discard_it() {
    let base = Instant::now();
    let mut scheduler = SnapshotScheduler::default();
    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::Focus, base));
    let reload = base + Duration::from_secs(9);
    assert_eq!(scheduler.replace_filter(reload), Some(7));
    assert_eq!(
        scheduler.next_deadline(),
        Some(reload + SETTLE_QUIET_INTERVAL)
    );
    assert_eq!(
        scheduler
            .take_due(reload + SETTLE_QUIET_INTERVAL)
            .expect("reload settle")
            .trigger,
        ContentSnapshotTrigger::Settle
    );
    assert_eq!(
        scheduler.next_deadline(),
        Some(reload + REFRESH_INTERVALS[0])
    );

    scheduler.did_wake();
    assert_eq!(scheduler.next_deadline(), None);
    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::Focus, base));
    scheduler.pause();
    assert_eq!(scheduler.next_deadline(), None);
    scheduler.stop();
    assert!(scheduler.take_due(base + Duration::from_secs(30)).is_none());
}
