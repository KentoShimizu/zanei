use std::time::{Duration, Instant};

use super::{PULL_MAX_HOLD, PULL_QUIET, PullScheduler, PullTrigger, SUBMIT_SETTLE};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

// Bug: every key-down reads the AX tree instead of one read per typing burst.
#[test]
fn a_typing_burst_is_read_once_after_the_quiet_period() {
    let start = Instant::now();
    let mut scheduler = PullScheduler::default();
    for offset in [0, 50, 100] {
        scheduler.arm(7, PullTrigger::Edit, start + ms(offset));
    }

    assert!(
        scheduler
            .take_due(start + ms(100) + PULL_QUIET - ms(1))
            .is_empty()
    );
    assert_eq!(
        scheduler.take_due(start + ms(100) + PULL_QUIET),
        vec![(7, "edit")]
    );
    assert!(scheduler.take_due(start + ms(1_000)).is_empty());
}

// Bug: the quiet-period debounce starves reads while the user types continuously.
#[test]
fn continuous_typing_is_still_read_at_the_maximum_hold() {
    let start = Instant::now();
    let mut scheduler = PullScheduler::default();
    let mut due = vec![Duration::ZERO];
    for elapsed in (0..=40).map(|step| ms(step * 50)) {
        if !scheduler.take_due(start + elapsed).is_empty() {
            due.push(elapsed);
        }
        scheduler.arm(7, PullTrigger::Edit, start + elapsed);
    }

    assert!(due.len() >= 4, "continuous typing must be sampled: {due:?}");
    assert!(
        due.windows(2)
            .all(|pair| pair[1] - pair[0] <= PULL_MAX_HOLD)
    );
}

// Bug: a pull re-arms itself and turns into periodic polling while the user is idle.
#[test]
fn nothing_is_read_again_without_new_input() {
    let start = Instant::now();
    let mut scheduler = PullScheduler::default();
    scheduler.arm(7, PullTrigger::Click, start);
    scheduler.arm(8, PullTrigger::Window, start);

    let due = (0..1_000)
        .map(|step| ms(step * 10))
        .filter(|elapsed| !scheduler.take_due(start + *elapsed).is_empty())
        .collect::<Vec<_>>();

    assert_eq!(due, vec![PULL_QUIET]);
}

// Bug: Return/Enter is debounced like an ordinary key, or the post-send read is missing.
#[test]
fn submit_is_read_now_and_once_more_after_settling() {
    let start = Instant::now();
    let mut scheduler = PullScheduler::default();
    scheduler.arm(7, PullTrigger::Edit, start);
    scheduler.arm(7, PullTrigger::Submit, start + ms(20));

    assert_eq!(scheduler.take_due(start + ms(20)), vec![(7, "submit")]);
    assert!(
        scheduler
            .take_due(start + ms(20) + SUBMIT_SETTLE - ms(1))
            .is_empty()
    );
    assert_eq!(
        scheduler.take_due(start + ms(20) + SUBMIT_SETTLE),
        vec![(7, "settle")]
    );
    assert!(
        scheduler
            .take_due(start + Duration::from_secs(10))
            .is_empty()
    );
}

// Bug: reads continue for an application after it quit.
#[test]
fn removing_a_pid_cancels_its_armed_reads() {
    let start = Instant::now();
    let mut scheduler = PullScheduler::default();
    scheduler.arm(7, PullTrigger::Submit, start);
    scheduler.arm(8, PullTrigger::Edit, start);

    scheduler.remove(7);

    assert_eq!(
        scheduler.take_due(start + Duration::from_secs(1)),
        vec![(8, "edit")]
    );
    assert!(
        scheduler
            .take_due(start + Duration::from_secs(2))
            .is_empty()
    );
}
