//! Accessibility window selection and snapshot traversal.

use std::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};

use zanei_core::schema::ContentSnapshotCutoff;

use crate::{
    content_snapshot::{
        SnapshotAttribute, SnapshotAttributeValue, SnapshotAxApplication, SnapshotAxElement,
        SnapshotAxError, SnapshotWalkOutput,
        budget::WalkBudget,
        coverage::{MaskGap, WalkAnalyzer},
        walker::{InstantWalkClock, SnapshotNode, WalkClock, walk_snapshot_analyzed},
    },
    ffi::{
        ax::AxFrame,
        window_list::{OnScreenWindow, on_screen_windows, window_id_for_frame_in_windows},
    },
};

use super::{ScanError, ScannedWindow};

pub(super) fn scan(
    pid: i32,
    expected_window_id: i64,
    stop: &AtomicBool,
    read_allowed: &mut dyn FnMut(Option<String>) -> bool,
) -> Result<ScanOutcome, ScanError> {
    let application = SnapshotAxApplication::new(pid)?;
    scan_application(
        application,
        expected_window_id,
        stop,
        on_screen_windows,
        read_allowed,
    )
}

/// What one scan found for the expected window.
pub(in crate::content_snapshot) enum ScanOutcome {
    Scanned(ScannedWindow),
    /// The capture policy denied reading the resolved window.
    Denied,
    /// No Accessibility window of the process resolved to the expected window id.
    Unresolved(WindowMisses),
}

/// Why one Accessibility window did not resolve to the expected window id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::content_snapshot) enum WindowMiss {
    /// `AXWindowNumber` names another window.
    NumberOther,
    /// The window exposes no position and size.
    FrameMissing,
    /// The frame matched another on-screen window of the process.
    BoundsOther,
    /// No on-screen window of the process matched the frame.
    BoundsNone,
}

impl WindowMiss {
    const fn trace_name(self) -> &'static str {
        match self {
            Self::NumberOther => "number_other",
            Self::FrameMissing => "frame_missing",
            Self::BoundsOther => "bounds_other",
            Self::BoundsNone => "bounds_none",
        }
    }
}

/// Metadata-only account of an unresolved window, for the candidate trace.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::content_snapshot) struct WindowMisses {
    /// The focused window's miss, or `None` when the process reports no focused window.
    pub(in crate::content_snapshot) focused: Option<WindowMiss>,
    /// `AXWindows` entries per miss.
    pub(in crate::content_snapshot) number_other: usize,
    pub(in crate::content_snapshot) frame_missing: usize,
    pub(in crate::content_snapshot) bounds_other: usize,
    pub(in crate::content_snapshot) bounds_none: usize,
    /// Whether the expected id is in the on-screen window list of the process.
    pub(in crate::content_snapshot) expected_on_screen: bool,
}

impl WindowMisses {
    fn record(&mut self, miss: WindowMiss) {
        let count = match miss {
            WindowMiss::NumberOther => &mut self.number_other,
            WindowMiss::FrameMissing => &mut self.frame_missing,
            WindowMiss::BoundsOther => &mut self.bounds_other,
            WindowMiss::BoundsNone => &mut self.bounds_none,
        };
        *count += 1;
    }
}

impl fmt::Display for WindowMisses {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "focused={} number_other={} frame_missing={} bounds_other={} bounds_none={} expected_on_screen={}",
            self.focused.map_or("absent", WindowMiss::trace_name),
            self.number_other,
            self.frame_missing,
            self.bounds_other,
            self.bounds_none,
            self.expected_on_screen
        )
    }
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
    fn frame(&self) -> Result<Option<AxFrame>, SnapshotAxError>;
    fn window_number(&self) -> Result<Option<i64>, SnapshotAxError>;
    fn title(&self) -> Result<Option<String>, SnapshotAxError>;
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
    fn frame(&self) -> Result<Option<AxFrame>, SnapshotAxError> {
        SnapshotAxElement::frame(self)
    }

    fn window_number(&self) -> Result<Option<i64>, SnapshotAxError> {
        SnapshotAxElement::window_number(self)
    }

    fn title(&self) -> Result<Option<String>, SnapshotAxError> {
        let value = self
            .copy_multiple(&[SnapshotAttribute::Title])?
            .into_iter()
            .next()
            .expect("AX title result count")?;
        match value {
            None => Ok(None),
            Some(SnapshotAttributeValue::Text(value)) => Ok(Some(value)),
            Some(_) => unreachable!("AX title result type"),
        }
    }
}

pub(in crate::content_snapshot) fn scan_application<A>(
    application: A,
    expected_window_id: i64,
    stop: &AtomicBool,
    list_on_screen: impl Fn(i64) -> Vec<OnScreenWindow>,
    read_allowed: &mut dyn FnMut(Option<String>) -> bool,
) -> Result<ScanOutcome, ScanError>
where
    A: SnapshotApplication,
{
    let clock = InstantWalkClock::start();
    let mut resolver = Resolver {
        expected_window_id,
        pid: i64::from(application.pid()),
        list_on_screen,
        on_screen: None,
    };
    let mut ax_calls = 1;
    let mut misses = WindowMisses::default();
    if let Some(output) = initial_time_cutoff(&clock, ax_calls) {
        return Ok(ScanOutcome::Scanned(output));
    }
    let focused_window = application.focused_window()?;
    ax_calls = ax_calls.saturating_add(1);
    if let Some(output) = initial_time_cutoff(&clock, ax_calls) {
        return Ok(ScanOutcome::Scanned(output));
    }
    if let Some(window) = focused_window {
        match resolver.resolve(window, &clock, &mut ax_calls)? {
            WindowResolution::Match(window, frame) => {
                return scan_window(window, frame, stop, &clock, &mut ax_calls, read_allowed);
            }
            WindowResolution::Cutoff(output) => return Ok(ScanOutcome::Scanned(output)),
            WindowResolution::Miss(miss) => misses.focused = Some(miss),
        }
    }

    let windows = application.windows()?;
    ax_calls = ax_calls.saturating_add(1);
    if let Some(output) = initial_time_cutoff(&clock, ax_calls) {
        return Ok(ScanOutcome::Scanned(output));
    }
    for window in windows {
        match resolver.resolve(window, &clock, &mut ax_calls)? {
            WindowResolution::Match(window, frame) => {
                return scan_window(window, frame, stop, &clock, &mut ax_calls, read_allowed);
            }
            WindowResolution::Cutoff(output) => return Ok(ScanOutcome::Scanned(output)),
            WindowResolution::Miss(miss) => misses.record(miss),
        }
    }
    misses.expected_on_screen = resolver
        .on_screen()
        .iter()
        .any(|window| window.id == expected_window_id);
    Ok(ScanOutcome::Unresolved(misses))
}

fn scan_window<W>(
    window: W,
    frame: AxFrame,
    stop: &AtomicBool,
    clock: &impl WalkClock,
    ax_calls: &mut usize,
    read_allowed: &mut dyn FnMut(Option<String>) -> bool,
) -> Result<ScanOutcome, ScanError>
where
    W: SnapshotWindow,
{
    let title = window.title()?;
    *ax_calls = ax_calls.saturating_add(1);
    if let Some(output) = initial_time_cutoff(clock, *ax_calls) {
        return Ok(ScanOutcome::Scanned(output));
    }
    if !read_allowed(title) {
        return Ok(ScanOutcome::Denied);
    }
    walk_window(window, frame, stop, clock, *ax_calls).map(ScanOutcome::Scanned)
}

enum WindowResolution<W> {
    Match(W, AxFrame),
    Miss(WindowMiss),
    Cutoff(ScannedWindow),
}

struct Resolver<L> {
    expected_window_id: i64,
    pid: i64,
    list_on_screen: L,
    /// Read at most once per scan, and only when a window has no `AXWindowNumber` or
    /// nothing resolves.
    on_screen: Option<Vec<OnScreenWindow>>,
}

impl<L> Resolver<L>
where
    L: Fn(i64) -> Vec<OnScreenWindow>,
{
    fn on_screen(&mut self) -> &[OnScreenWindow] {
        let Self {
            pid,
            list_on_screen,
            on_screen,
            ..
        } = self;
        on_screen.get_or_insert_with(|| list_on_screen(*pid))
    }

    fn resolve<W>(
        &mut self,
        window: W,
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
        if window_number.is_some_and(|window_id| window_id != self.expected_window_id) {
            return Ok(WindowResolution::Miss(WindowMiss::NumberOther));
        }

        let frame = window.frame()?;
        *ax_calls = ax_calls.saturating_add(1);
        if let Some(output) = initial_time_cutoff(clock, *ax_calls) {
            return Ok(WindowResolution::Cutoff(output));
        }
        let Some(frame) = frame else {
            return Ok(WindowResolution::Miss(WindowMiss::FrameMissing));
        };
        if window_number.is_some() {
            return Ok(WindowResolution::Match(window, frame));
        }
        match window_id_for_frame_in_windows(self.on_screen(), frame) {
            Some(window_id) if window_id == self.expected_window_id => {
                Ok(WindowResolution::Match(window, frame))
            }
            Some(_) => Ok(WindowResolution::Miss(WindowMiss::BoundsOther)),
            None => Ok(WindowResolution::Miss(WindowMiss::BoundsNone)),
        }
    }
}

fn walk_window<W>(
    window: W,
    frame: AxFrame,
    stop: &AtomicBool,
    clock: &impl WalkClock,
    ax_calls: usize,
) -> Result<ScannedWindow, ScanError>
where
    W: SnapshotWindow,
{
    let (mut output, analysis) =
        walk_snapshot_analyzed(window, frame, WalkBudget::DESIGN, clock, || {
            stop.load(Ordering::Acquire)
        })
        .map_err(ScanError::Walk)?;
    output.ax_calls = output.ax_calls.saturating_add(ax_calls);
    Ok((output, analysis))
}

fn initial_time_cutoff(clock: &impl WalkClock, ax_calls: usize) -> Option<ScannedWindow> {
    let elapsed = clock.elapsed();
    (elapsed >= WalkBudget::DESIGN.wall_time).then(|| {
        let output = SnapshotWalkOutput {
            text: String::new(),
            nodes: 0,
            ax_calls,
            elapsed,
            cutoff: Some(ContentSnapshotCutoff::Time),
            degraded_nodes: 0,
            frameless_nodes: 0,
        };
        let mut analyzer = WalkAnalyzer::new();
        analyzer.mask_gap(MaskGap::Cutoff(ContentSnapshotCutoff::Time));
        (output, analyzer.finish())
    })
}

#[cfg(test)]
pub(in crate::content_snapshot) fn test_live_scan(
    pid: i32,
    window_id: i64,
) -> Result<Option<SnapshotWalkOutput>, String> {
    match scan(pid, window_id, &AtomicBool::new(false), &mut |_| true) {
        Ok(ScanOutcome::Scanned((output, _))) => Ok(Some(output)),
        Ok(ScanOutcome::Denied | ScanOutcome::Unresolved(_)) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}
