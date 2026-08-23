use std::time::{Duration, Instant};

use crate::content_snapshot::{
    budget::{DAILY_BUDGET_WINDOW, DAILY_TEXT_BUDGET_BYTES},
    state::{SaveBlock, SnapshotState, SnapshotWindowKey},
};

fn key(pid: i64, window_id: i64) -> SnapshotWindowKey {
    SnapshotWindowKey { pid, window_id }
}

#[test]
fn hashes_and_times_commit_only_after_successful_delivery() {
    let base = Instant::now();
    let mut state = SnapshotState::new(base);
    let first = key(7, 11);
    let hash = SnapshotState::text_hash("same");

    assert_eq!(state.evaluate_save(first, hash, 4, base), Ok(()));
    assert_eq!(state.evaluate_save(first, hash, 4, base), Ok(()));
    state.commit_save(first, hash, 4, base);
    assert_eq!(
        state.evaluate_save(first, hash, 4, base + Duration::from_secs(5)),
        Err(SaveBlock::Duplicate)
    );
    assert_eq!(
        state.evaluate_save(
            key(8, 12),
            SnapshotState::text_hash("different"),
            9,
            base + Duration::from_secs(4)
        ),
        Err(SaveBlock::GlobalInterval)
    );
}

#[test]
fn daily_budget_rolls_at_the_24_hour_boundary() {
    let base = Instant::now();
    let mut state = SnapshotState::new(base);
    state.commit_save(
        key(7, 11),
        1,
        usize::try_from(DAILY_TEXT_BUDGET_BYTES).expect("design budget fits usize"),
        base,
    );
    assert_eq!(state.daily_bytes(base), DAILY_TEXT_BUDGET_BYTES);
    assert!(!state.daily_budget_allows(base));
    assert!(state.daily_budget_allows(base + DAILY_BUDGET_WINDOW));
    assert_eq!(state.daily_bytes(base + DAILY_BUDGET_WINDOW), 0);
}

#[test]
fn a_body_that_would_cross_the_daily_limit_marks_the_current_budget_degraded() {
    let base = Instant::now();
    let mut state = SnapshotState::new(base);
    let almost_full =
        usize::try_from(DAILY_TEXT_BUDGET_BYTES - 1).expect("design budget fits usize");
    state.commit_save(key(7, 11), 1, almost_full, base);
    assert_eq!(
        state.evaluate_save(key(8, 12), 2, 2, base + Duration::from_secs(5)),
        Err(SaveBlock::DailyBudget)
    );
    assert!(!state.daily_budget_allows(base + Duration::from_secs(5)));
}

#[test]
fn backoff_doubles_to_the_cap_and_termination_cleans_pid_state() {
    let base = Instant::now();
    let mut state = SnapshotState::new(base);
    state.record_failure(7, base, true);
    assert!(!state.backoff_allows(7, base + Duration::from_secs(29)));
    assert!(state.backoff_allows(7, base + Duration::from_secs(30)));

    for attempt in 1..=8 {
        state.record_failure(7, base + Duration::from_secs(attempt), true);
    }
    assert_eq!(
        state.backoff_remaining(base + Duration::from_secs(8)),
        Some(Duration::from_secs(600))
    );
    state.commit_save(key(7, 11), 1, 1, base + Duration::from_secs(700));
    state.record_failure(7, base + Duration::from_secs(701), true);
    state.terminate_pid(7);
    assert!(state.backoff_allows(7, base + Duration::from_secs(701)));
    assert_eq!(state.last_saved_at(key(7, 11)), None);
}

#[test]
fn non_timeout_failure_resets_the_timeout_streak() {
    let base = Instant::now();
    let mut state = SnapshotState::new(base);
    state.record_failure(7, base, true);
    state.record_failure(7, base + Duration::from_secs(1), true);
    state.record_failure(7, base + Duration::from_secs(2), false);

    assert!(state.backoff_allows(7, base + Duration::from_secs(32)));

    state.record_failure(7, base + Duration::from_secs(33), true);
    assert!(state.backoff_allows(7, base + Duration::from_secs(63)));
}
