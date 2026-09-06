//! Accessibility window selection and snapshot traversal.

use std::sync::atomic::{AtomicBool, Ordering};

use zanei_core::schema::ContentSnapshotCutoff;

use crate::{
    content_snapshot::{
        SnapshotAxApplication, SnapshotAxElement, SnapshotAxError, SnapshotWalkOutput,
        budget::WalkBudget,
        walker::{InstantWalkClock, SnapshotNode, WalkClock, walk_snapshot},
    },
    ffi::window_list::window_id_for_frame,
};

use super::ScanError;

pub(super) fn scan(
    pid: i32,
    expected_window_id: i64,
    stop: &AtomicBool,
) -> Result<Option<SnapshotWalkOutput>, ScanError> {
    let application = SnapshotAxApplication::new(pid)?;
    scan_application(application, expected_window_id, stop, window_id_for_frame)
}

pub(in crate::content_snapshot) trait SnapshotApplication {
    type Window: SnapshotWindow;
    fn pid(&self) -> i32;
    fn focused_window(&self) -> Result<Option<Self::Window>, SnapshotAxError>;
    fn windows(&self) -> Result<Vec<Self::Window>, SnapshotAxError>;
}

pub(in crate::content_snapshot) trait SnapshotWindow:
    SnapshotNode
{
    fn frame(&self) -> Result<Option<crate::ffi::ax::AxFrame>, SnapshotAxError>;
    fn window_number(&self) -> Result<Option<i64>, SnapshotAxError>;
}

impl SnapshotApplication for SnapshotAxApplication {
    type Window = SnapshotAxElement;

    fn pid(&self) -> i32 {
        SnapshotAxApplication::pid(self)
    }

    fn focused_window(&self) -> Result<Option<Self::Window>, SnapshotAxError> {
        SnapshotAxApplication::focused_window(self)
    }

    fn windows(&self) -> Result<Vec<Self::Window>, SnapshotAxError> {
        SnapshotAxApplication::windows(self)
    }
}

impl SnapshotWindow for SnapshotAxElement {
    fn frame(&self) -> Result<Option<crate::ffi::ax::AxFrame>, SnapshotAxError> {
        SnapshotAxElement::frame(self)
    }

    fn window_number(&self) -> Result<Option<i64>, SnapshotAxError> {
        SnapshotAxElement::window_number(self)
    }
}

pub(in crate::content_snapshot) fn scan_application<A>(
    application: A,
    expected_window_id: i64,
    stop: &AtomicBool,
    resolve_bounds: impl Fn(i64, crate::ffi::ax::AxFrame) -> Option<i64>,
) -> Result<Option<SnapshotWalkOutput>, ScanError>
where
    A: SnapshotApplication,
{
    let clock = InstantWalkClock::start();
    let pid = application.pid();
    let mut ax_calls = 1;
    if let Some(output) = initial_time_cutoff(&clock, ax_calls) {
        return Ok(Some(output));
    }
    let focused_window = application.focused_window()?;
    ax_calls = ax_calls.saturating_add(1);
    if let Some(output) = initial_time_cutoff(&clock, ax_calls) {
        return Ok(Some(output));
    }
    if let Some(window) = focused_window {
        match resolve_window(
            window,
            expected_window_id,
            i64::from(pid),
            &resolve_bounds,
            &clock,
            &mut ax_calls,
        )? {
            WindowResolution::Match(window, frame) => {
                return walk_window(window, frame, stop, &clock, ax_calls).map(Some);
            }
            WindowResolution::Cutoff(output) => return Ok(Some(output)),
            WindowResolution::Miss => {}
        }
    }

    let windows = application.windows()?;
    ax_calls = ax_calls.saturating_add(1);
    if let Some(output) = initial_time_cutoff(&clock, ax_calls) {
        return Ok(Some(output));
    }
    for window in windows {
        match resolve_window(
            window,
            expected_window_id,
            i64::from(pid),
            &resolve_bounds,
            &clock,
            &mut ax_calls,
        )? {
            WindowResolution::Match(window, frame) => {
                return walk_window(window, frame, stop, &clock, ax_calls).map(Some);
            }
            WindowResolution::Cutoff(output) => return Ok(Some(output)),
            WindowResolution::Miss => {}
        }
    }
    Ok(None)
}

enum WindowResolution<W> {
    Match(W, crate::ffi::ax::AxFrame),
    Miss,
    Cutoff(SnapshotWalkOutput),
}

fn resolve_window<W>(
    window: W,
    expected_window_id: i64,
    pid: i64,
    resolve_bounds: &impl Fn(i64, crate::ffi::ax::AxFrame) -> Option<i64>,
    clock: &impl WalkClock,
    ax_calls: &mut usize,
) -> Result<WindowResolution<W>, ScanError>
where
    W: SnapshotWindow,
{
    let window_number = window.window_number()?;
    *ax_calls = ax_calls.saturating_add(1);
    if let Some(output) = initial_time_cutoff(clock, *ax_calls) {
        return Ok(WindowResolution::Cutoff(output));
    }
    if window_number.is_some_and(|window_id| window_id != expected_window_id) {
        return Ok(WindowResolution::Miss);
    }

    let frame = window.frame()?;
    *ax_calls = ax_calls.saturating_add(1);
    if let Some(output) = initial_time_cutoff(clock, *ax_calls) {
        return Ok(WindowResolution::Cutoff(output));
    }
    let Some(frame) = frame else {
        return Ok(WindowResolution::Miss);
    };
    let window_id = window_number.or_else(|| resolve_bounds(pid, frame));
    if window_id == Some(expected_window_id) {
        Ok(WindowResolution::Match(window, frame))
    } else {
        Ok(WindowResolution::Miss)
    }
}

fn walk_window<W>(
    window: W,
    frame: crate::ffi::ax::AxFrame,
    stop: &AtomicBool,
    clock: &impl WalkClock,
    ax_calls: usize,
) -> Result<SnapshotWalkOutput, ScanError>
where
    W: SnapshotWindow,
{
    let mut output = walk_snapshot(window, frame, WalkBudget::DESIGN, clock, || {
        stop.load(Ordering::Acquire)
    })
    .map_err(ScanError::Walk)?;
    output.ax_calls = output.ax_calls.saturating_add(ax_calls);
    Ok(output)
}

fn initial_time_cutoff(clock: &impl WalkClock, ax_calls: usize) -> Option<SnapshotWalkOutput> {
    let elapsed = clock.elapsed();
    (elapsed >= WalkBudget::DESIGN.wall_time).then(|| SnapshotWalkOutput {
        text: String::new(),
        nodes: 0,
        ax_calls,
        elapsed,
        cutoff: Some(ContentSnapshotCutoff::Time),
        degraded_nodes: 0,
        frameless_nodes: 0,
    })
}

#[cfg(test)]
pub(in crate::content_snapshot) fn test_live_scan(
    pid: i32,
    window_id: i64,
) -> Result<Option<SnapshotWalkOutput>, String> {
    scan(pid, window_id, &AtomicBool::new(false)).map_err(|error| error.to_string())
}
