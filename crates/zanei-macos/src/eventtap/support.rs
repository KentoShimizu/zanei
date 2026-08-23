//! Small runtime conversions and counters for EventTap processing.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use zanei_core::schema::ClickButton;

use super::state::{DisableReason, Driver, EventTapApi, MonotonicTime};
use crate::ffi::eventtap::NativeDisableReason;

pub(super) fn refresh_secure_input<A: EventTapApi>(
    driver: &mut Driver<A>,
    published: &std::sync::atomic::AtomicBool,
) -> bool {
    let enabled = driver.api_mut().secure_input_enabled();
    published.store(enabled, Ordering::Relaxed);
    enabled
}

pub(super) fn record_degraded_entries<A: EventTapApi>(
    driver: &Driver<A>,
    observed: &mut u64,
    degraded_operations: &AtomicU64,
) {
    let current = driver.degraded_entries();
    if current > *observed {
        degraded_operations.fetch_add(current - *observed, Ordering::Relaxed);
        *observed = current;
    }
}

pub(super) const fn disable_reason(value: NativeDisableReason) -> DisableReason {
    match value {
        NativeDisableReason::Timeout => DisableReason::Timeout,
        NativeDisableReason::UserInput => DisableReason::UserInput,
    }
}

pub(super) const fn click_button(value: u32) -> ClickButton {
    match value {
        0 => ClickButton::Left,
        1 => ClickButton::Right,
        _ => ClickButton::Other,
    }
}

pub(super) fn elapsed(started_at: Instant) -> MonotonicTime {
    MonotonicTime::from_duration(started_at.elapsed())
}
