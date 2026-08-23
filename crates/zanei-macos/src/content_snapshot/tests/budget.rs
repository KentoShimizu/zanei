use std::time::Duration;

use crate::content_snapshot::budget::*;

#[test]
fn design_limits_have_explicit_units_and_values() {
    assert_eq!(WALK_WALL_TIME_LIMIT, Duration::from_millis(200));
    assert_eq!(WALK_NODE_LIMIT, 2_000);
    assert_eq!(SNAPSHOT_TEXT_LIMIT_BYTES, 32 * 1_024);
    assert_eq!(AX_CALL_TIMEOUT, Duration::from_millis(100));
    assert_eq!(DAILY_TEXT_BUDGET_BYTES, 128 * 1_024 * 1_024);
    assert_eq!(DAILY_BUDGET_WINDOW, Duration::from_secs(24 * 60 * 60));
    assert_eq!(PID_BACKOFF_INITIAL, Duration::from_secs(30));
    assert_eq!(PID_BACKOFF_MAX, Duration::from_secs(10 * 60));
}
