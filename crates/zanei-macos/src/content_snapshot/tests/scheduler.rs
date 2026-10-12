use std::time::{Duration, Instant};

use zanei_core::schema::ContentSnapshotTrigger;

use crate::content_snapshot::{
    SnapshotTriggerKind, SnapshotTriggerMessage, SnapshotWindowKey,
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
        7,
        11,
        SnapshotTriggerKind::FocusOut,
        base + Duration::from_secs(5),
    ));
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

#[test]
fn input_rearms_the_current_window_without_resetting_the_refresh_backoff() {
    // Catches content changed by clicking, scrolling, or typing inside one window never
    // producing another snapshot, input for another window arming one, and input that
    // resets the backoff so refreshes outnumber the settles input triggers itself.
    let base = Instant::now();
    let input = |window_id, observed_at| SnapshotTriggerMessage::Input {
        window: SnapshotWindowKey { pid: 7, window_id },
        observed_at,
    };
    let mut scheduler = SnapshotScheduler::default();
    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::Focus, base));
    scheduler
        .take_due(base + SETTLE_QUIET_INTERVAL)
        .expect("initial settle");
    scheduler
        .take_due(base + REFRESH_INTERVALS[0])
        .expect("first refresh");
    let next_refresh = base + REFRESH_INTERVALS[0] + REFRESH_INTERVALS[1];

    let typed = base + Duration::from_secs(40);
    scheduler.observe_message(input(12, typed));
    assert_eq!(scheduler.next_deadline(), Some(next_refresh));

    scheduler.observe_message(input(11, typed));
    assert_eq!(
        scheduler.next_deadline(),
        Some(typed + SETTLE_QUIET_INTERVAL)
    );
    assert!(
        scheduler
            .take_due(typed + SETTLE_QUIET_INTERVAL - Duration::from_millis(1))
            .is_none()
    );
    assert_eq!(
        scheduler
            .take_due(typed + SETTLE_QUIET_INTERVAL)
            .expect("input settle")
            .trigger,
        ContentSnapshotTrigger::Settle
    );
    assert_eq!(
        scheduler.next_deadline(),
        Some(typed + REFRESH_INTERVALS[1])
    );

    let burst = typed + Duration::from_secs(10);
    for second in 0..=12 {
        scheduler.observe_message(input(11, burst + Duration::from_secs(second)));
    }
    assert_eq!(scheduler.next_deadline(), Some(burst + SETTLE_MAX_INTERVAL));
}

#[test]
fn input_delivered_late_never_pulls_the_settle_before_the_last_change() {
    // Catches a held-back input, published with its own earlier time after a title change
    // was processed, moving the settle earlier than two quiet seconds after that change.
    let base = Instant::now();
    let input = |observed_at| SnapshotTriggerMessage::Input {
        window: SnapshotWindowKey {
            pid: 7,
            window_id: 11,
        },
        observed_at,
    };
    let mut scheduler = SnapshotScheduler::default();
    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::Focus, base));
    scheduler
        .take_due(base + SETTLE_QUIET_INTERVAL)
        .expect("initial settle");
    let typed = base + Duration::from_secs(5);
    scheduler.observe_message(input(typed));
    let retitled = typed + Duration::from_millis(400);
    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::Title, retitled));
    scheduler.observe_message(input(typed + Duration::from_millis(100)));

    assert_eq!(
        scheduler.next_deadline(),
        Some(retitled + SETTLE_QUIET_INTERVAL)
    );
    assert!(
        scheduler
            .take_due(retitled + SETTLE_QUIET_INTERVAL - Duration::from_millis(1))
            .is_none()
    );
}

#[test]
fn a_candidate_held_back_by_the_global_interval_is_retried_once_when_it_ends() {
    // Catches a settle or refresh blocked by another window's recent save being discarded,
    // so the window is recorded only after new input; a retry that changes its trigger;
    // and a retry that is retried again, which would poll every five seconds.
    let base = Instant::now();
    let ends = base + Duration::from_secs(4);
    let mut scheduler = SnapshotScheduler::default();
    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::Focus, base));
    let settle = scheduler
        .take_due(base + SETTLE_QUIET_INTERVAL)
        .expect("initial settle");
    assert!(scheduler.defer(&settle, ends));
    assert_eq!(scheduler.next_deadline(), Some(ends));
    assert!(
        scheduler
            .take_due(ends - Duration::from_millis(1))
            .is_none()
    );
    let retry = scheduler.take_due(ends).expect("deferred settle");
    assert_eq!(retry.trigger, ContentSnapshotTrigger::Settle);
    assert!(retry.deferred);
    assert!(!scheduler.defer(&retry, ends + Duration::from_secs(5)));
    assert_eq!(scheduler.next_deadline(), Some(base + REFRESH_INTERVALS[0]));

    let refresh = scheduler
        .take_due(base + REFRESH_INTERVALS[0])
        .expect("first refresh");
    let refresh_ends = base + REFRESH_INTERVALS[0] + Duration::from_secs(1);
    assert!(scheduler.defer(&refresh, refresh_ends));
    let retry = scheduler.take_due(refresh_ends).expect("deferred refresh");
    assert_eq!(retry.trigger, ContentSnapshotTrigger::Refresh);
    assert_eq!(retry.activity_window, Some(REFRESH_INTERVALS[0]));
}

#[test]
fn a_deferred_settle_is_replaced_by_newer_input_and_dropped_by_window_change_or_pause() {
    // Catches a stale retry outliving the change that superseded it: it would add a
    // snapshot after newer input, or snapshot a window the user already left.
    let base = Instant::now();
    let input = SnapshotTriggerMessage::Input {
        window: SnapshotWindowKey {
            pid: 7,
            window_id: 11,
        },
        observed_at: base + Duration::from_secs(3),
    };
    let ends = base + Duration::from_secs(4);
    let mut scheduler = SnapshotScheduler::default();
    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::Focus, base));
    let settle = scheduler
        .take_due(base + SETTLE_QUIET_INTERVAL)
        .expect("initial settle");
    assert!(scheduler.defer(&settle, ends));
    scheduler.observe_message(input);
    assert_eq!(
        scheduler.next_deadline(),
        Some(base + Duration::from_secs(3) + SETTLE_QUIET_INTERVAL)
    );
    let settle = scheduler
        .take_due(base + Duration::from_secs(3) + SETTLE_QUIET_INTERVAL)
        .expect("input settle");
    assert!(!settle.deferred);

    assert!(scheduler.defer(&settle, ends));
    scheduler.observe(trigger(8, 12, SnapshotTriggerKind::Focus, base));
    let next = scheduler.take_due(ends).expect("new window settle");
    assert_eq!((next.target.window.id, next.deferred), (Some(12), false));
    assert!(
        scheduler.take_due(ends).is_none(),
        "the left window's retry"
    );
    assert!(
        !scheduler.defer(&settle, ends),
        "only the current window defers"
    );

    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::Focus, base));
    let settle = scheduler
        .take_due(base + SETTLE_QUIET_INTERVAL)
        .expect("returned settle");
    assert!(scheduler.defer(&settle, ends));
    scheduler.pause();
    assert!(scheduler.take_due(ends).is_none());
    assert!(!scheduler.defer(&settle, ends));
}

#[test]
fn a_deferred_focus_out_waits_its_turn_and_yields_to_a_newer_trigger_for_its_window() {
    // Catches a deferred focus-out at the queue front holding back later immediate
    // focus-outs, and a retry that still snapshots a window the user returned to.
    let base = Instant::now();
    let ends = base + Duration::from_secs(4);
    let mut scheduler = SnapshotScheduler::default();
    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::FocusOut, base));
    let left = scheduler.take_due(base).expect("first focus-out");
    assert!(scheduler.defer(&left, ends));
    let next_left = base + Duration::from_secs(1);
    scheduler.observe(trigger(8, 12, SnapshotTriggerKind::FocusOut, next_left));
    assert_eq!(
        scheduler
            .take_due(next_left)
            .expect("immediate focus-out")
            .target
            .window
            .id,
        Some(12)
    );
    let retry = scheduler.take_due(ends).expect("deferred focus-out");
    assert_eq!(retry.trigger, ContentSnapshotTrigger::FocusOut);
    assert_eq!(retry.target.window.id, Some(11));
    assert!(!scheduler.defer(&retry, ends + Duration::from_secs(5)));

    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::FocusOut, base));
    let left = scheduler.take_due(base).expect("second focus-out");
    assert!(scheduler.defer(&left, ends));
    scheduler.observe(trigger(7, 11, SnapshotTriggerKind::Focus, next_left));
    assert_eq!(
        scheduler.next_deadline(),
        Some(next_left + SETTLE_QUIET_INTERVAL)
    );
}
