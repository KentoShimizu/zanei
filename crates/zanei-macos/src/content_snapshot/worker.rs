//! Content worker loop, ordered policy gates, traversal, and delivery commit.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

use zanei_collector::RawEvent;
use zanei_core::{
    privacy::PrivacyScope,
    schema::{ContentSnapshotCutoff, ContentSnapshotTrigger},
};

use crate::{
    content_snapshot::{
        SnapshotAxApplication, SnapshotAxError, SnapshotTriggerReceiver, SnapshotWalkOutput,
        budget::WalkBudget,
        output::{emit, emit_released, trace_candidate, trace_output},
        scheduler::{ScheduledSnapshot, SnapshotScheduler},
        state::SnapshotState,
        walker::{InstantWalkClock, WalkClock, walk_snapshot},
    },
    ffi::window_list::window_id_for_frame,
    focus_context::FocusContext,
    text_capture::TextQuarantine,
    workspace::WorkspaceEvent,
};

use super::{Control, SharedHealth, WORKER_POLL_INTERVAL};
use crate::{CapturePolicy, chrome::ChromeObserver};

mod scheduling;

use scheduling::CandidateTime;
pub(super) use scheduling::seed_scheduler_from_focus;

#[allow(clippy::too_many_arguments)]
pub(super) fn run_worker(
    trigger: &SnapshotTriggerReceiver,
    lifecycle: &Receiver<WorkspaceEvent>,
    controls: Receiver<Control>,
    stop: Arc<AtomicBool>,
    sender: SyncSender<RawEvent>,
    capture_policy: CapturePolicy,
    chrome_observer: ChromeObserver,
    health: SharedHealth,
    state: &mut SnapshotState,
    focus_context: FocusContext,
) {
    run_worker_with_scanner(
        trigger,
        lifecycle,
        controls,
        stop,
        sender,
        capture_policy,
        chrome_observer,
        health,
        state,
        focus_context,
        scan,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_worker_with_scanner<F>(
    trigger: &SnapshotTriggerReceiver,
    lifecycle: &Receiver<WorkspaceEvent>,
    controls: Receiver<Control>,
    stop: Arc<AtomicBool>,
    sender: SyncSender<RawEvent>,
    capture_policy: CapturePolicy,
    chrome_observer: ChromeObserver,
    health: SharedHealth,
    state: &mut SnapshotState,
    focus_context: FocusContext,
    scan_window: F,
) where
    F: Fn(i32, i64, &AtomicBool) -> Result<Option<SnapshotWalkOutput>, ScanError>,
{
    debug_assert_eq!(std::thread::current().name(), Some("zanei-content"));
    let mut scheduler = SnapshotScheduler::default();
    seed_scheduler_from_focus(&mut scheduler, &focus_context, Instant::now());
    let mut quarantine = TextQuarantine::new(chrome_observer);
    while !stop.load(Ordering::Acquire) {
        if service_controls(&controls, &mut scheduler, state, Instant::now()) {
            break;
        }
        emit_released(
            quarantine.release(Instant::now(), &capture_policy),
            &sender,
            &health,
            state,
        );
        let wait = scheduler
            .next_deadline()
            .map_or(WORKER_POLL_INTERVAL, |deadline| {
                deadline
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO)
                    .min(WORKER_POLL_INTERVAL)
            });
        let observation = match trigger.recv_timeout(wait) {
            Ok(observation) => Some(observation),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // Content subscribes before AX, so a wake reset is queued before the
        // focus resync trigger derived from it. Drain lifecycle before re-seeding.
        service_lifecycle(lifecycle, &mut scheduler, state);
        if let Some(observation) = observation {
            health.processed_triggers.fetch_add(1, Ordering::Relaxed);
            scheduler.observe_message(observation);
        }
        while let Some(candidate) = scheduler.take_due(Instant::now()) {
            let taken_at = CandidateTime::now();
            process_candidate(
                candidate,
                taken_at,
                state,
                CandidateContext {
                    policy: &capture_policy,
                    sender: &sender,
                    stop: &stop,
                    health: &health,
                    focus_context: &focus_context,
                    quarantine: &mut quarantine,
                    scan_window: &scan_window,
                },
            );
            if stop.load(Ordering::Acquire) {
                break;
            }
        }
        update_degraded(&health, state, Instant::now());
    }
    emit_released(quarantine.flush(), &sender, &health, state);
    scheduler.stop();
}

pub(super) fn service_controls(
    controls: &Receiver<Control>,
    scheduler: &mut SnapshotScheduler,
    state: &mut SnapshotState,
    now: Instant,
) -> bool {
    for control in controls.try_iter() {
        match control {
            Control::ReplaceFilter { acknowledge } => {
                if let Some(pid) = scheduler.replace_filter(now) {
                    state.clear_backoff(pid);
                }
                let _ = acknowledge.send(());
            }
            Control::Stop => {
                scheduler.stop();
                return true;
            }
        }
    }
    false
}

fn service_lifecycle(
    lifecycle: &Receiver<WorkspaceEvent>,
    scheduler: &mut SnapshotScheduler,
    state: &mut SnapshotState,
) {
    for event in lifecycle.try_iter() {
        match event {
            WorkspaceEvent::Terminated(app) => {
                scheduler.terminate_pid(app.pid);
                state.terminate_pid(app.pid);
            }
            WorkspaceEvent::DidWake => scheduler.did_wake(),
            WorkspaceEvent::Activated(_) | WorkspaceEvent::Launched(_) => {}
        }
    }
}

struct CandidateContext<'a, F> {
    policy: &'a CapturePolicy,
    sender: &'a SyncSender<RawEvent>,
    stop: &'a AtomicBool,
    health: &'a SharedHealth,
    focus_context: &'a FocusContext,
    quarantine: &'a mut TextQuarantine,
    scan_window: &'a F,
}

fn process_candidate<F>(
    candidate: ScheduledSnapshot,
    taken_at: CandidateTime,
    state: &mut SnapshotState,
    context: CandidateContext<'_, F>,
) where
    F: Fn(i32, i64, &AtomicBool) -> Result<Option<SnapshotWalkOutput>, ScanError>,
{
    let CandidateContext {
        policy,
        sender,
        stop,
        health,
        focus_context,
        quarantine,
        scan_window,
    } = context;
    let now = taken_at.monotonic;
    let Some(key) = candidate.key() else {
        trace_candidate(&candidate, "window_id", 0, Duration::ZERO, 0, false, None);
        return;
    };
    if candidate.trigger != ContentSnapshotTrigger::FocusOut
        && !focus_context.current().is_some_and(|focus| {
            focus.app.pid == key.pid
                && focus.window.as_ref().and_then(|window| window.id) == Some(key.window_id)
        })
    {
        trace_candidate(
            &candidate,
            "focus_context_stale",
            0,
            Duration::ZERO,
            0,
            false,
            None,
        );
        return;
    }
    if candidate.trigger == ContentSnapshotTrigger::FocusOut
        && !SnapshotScheduler::focus_out_allows(state.last_saved_at(key), now)
    {
        trace_candidate(
            &candidate,
            "focus_out_interval",
            0,
            Duration::ZERO,
            0,
            false,
            None,
        );
        return;
    }
    let initial_decision = policy.decision(
        PrivacyScope::ContentSnapshot,
        &candidate.target.app.raw_app(),
        Some(key.window_id),
    );
    if !initial_decision.is_allowed() {
        trace_candidate(&candidate, "app_scope", 0, Duration::ZERO, 0, false, None);
        return;
    }
    if !policy.secure_input_allows() {
        trace_candidate(
            &candidate,
            "secure_input",
            0,
            Duration::ZERO,
            0,
            false,
            None,
        );
        return;
    }
    if !policy.refresh_activity_allows(candidate.activity_window) {
        trace_candidate(&candidate, "activity", 0, Duration::ZERO, 0, false, None);
        return;
    }
    if !state.global_interval_allows(now) {
        trace_candidate(
            &candidate,
            "global_interval",
            0,
            Duration::ZERO,
            0,
            false,
            None,
        );
        return;
    }
    if !state.daily_budget_allows(now) {
        trace_candidate(
            &candidate,
            "daily_budget",
            0,
            Duration::ZERO,
            0,
            false,
            None,
        );
        return;
    }
    if !state.backoff_allows(key.pid, now) {
        trace_candidate(&candidate, "pid_backoff", 0, Duration::ZERO, 0, false, None);
        return;
    }
    let Ok(pid) = i32::try_from(key.pid) else {
        trace_candidate(&candidate, "pid", 0, Duration::ZERO, 0, false, None);
        return;
    };
    let output = match scan_window(pid, key.window_id, stop) {
        Ok(Some(output)) => output,
        Ok(None) => {
            trace_candidate(&candidate, "stale", 0, Duration::ZERO, 0, false, None);
            return;
        }
        Err(error) => {
            record_scan_failure(state, health, key.pid, now, &candidate, &error);
            return;
        }
    };
    health.failures.fetch_add(
        u64::try_from(output.degraded_nodes).expect("degraded node count must fit u64"),
        Ordering::Relaxed,
    );
    if stop.load(Ordering::Acquire) || output.cutoff == Some(ContentSnapshotCutoff::Stopped) {
        trace_output(&candidate, "stopped", &output);
        return;
    }
    state.record_scan_success(key.pid);
    if output.text.is_empty() {
        trace_output(&candidate, "empty", &output);
        return;
    }
    if !policy.secure_input_allows() {
        trace_output(&candidate, "secure_input", &output);
        return;
    }
    let hash = SnapshotState::text_hash(&output.text);
    let save_at = Instant::now();
    emit(
        candidate,
        output,
        key,
        hash,
        policy,
        &initial_decision,
        taken_at.wall,
        save_at,
        state,
        sender,
        health,
        quarantine,
    );
}

fn scan(
    pid: i32,
    expected_window_id: i64,
    stop: &AtomicBool,
) -> Result<Option<SnapshotWalkOutput>, ScanError> {
    let application = SnapshotAxApplication::new(pid)?;
    scan_application(application, expected_window_id, stop, window_id_for_frame)
}

pub(super) trait SnapshotApplication {
    type Window: SnapshotWindow;
    fn pid(&self) -> i32;
    fn focused_window(&self) -> Result<Option<Self::Window>, SnapshotAxError>;
    fn windows(&self) -> Result<Vec<Self::Window>, SnapshotAxError>;
}

pub(super) trait SnapshotWindow: crate::content_snapshot::walker::SnapshotNode {
    fn frame(&self) -> Result<Option<crate::ffi::ax::AxFrame>, SnapshotAxError>;
    fn window_number(&self) -> Result<Option<i64>, SnapshotAxError>;
}

impl SnapshotApplication for SnapshotAxApplication {
    type Window = crate::content_snapshot::SnapshotAxElement;
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

impl SnapshotWindow for crate::content_snapshot::SnapshotAxElement {
    fn frame(&self) -> Result<Option<crate::ffi::ax::AxFrame>, SnapshotAxError> {
        crate::content_snapshot::SnapshotAxElement::frame(self)
    }

    fn window_number(&self) -> Result<Option<i64>, SnapshotAxError> {
        crate::content_snapshot::SnapshotAxElement::window_number(self)
    }
}

pub(super) fn scan_application<A>(
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
pub(super) fn test_live_scan(
    pid: i32,
    window_id: i64,
) -> Result<Option<SnapshotWalkOutput>, String> {
    scan(pid, window_id, &AtomicBool::new(false)).map_err(|error| error.to_string())
}

fn record_scan_failure(
    state: &mut SnapshotState,
    health: &SharedHealth,
    pid: i64,
    now: Instant,
    candidate: &ScheduledSnapshot,
    error: &ScanError,
) {
    health.failures.fetch_add(1, Ordering::Relaxed);
    state.record_failure(pid, now, scan_timed_out(error));
    let (nodes, elapsed) = match error {
        ScanError::Walk(error) => (error.nodes, error.elapsed),
        ScanError::Ax(_) => (0, Duration::ZERO),
    };
    trace_candidate(candidate, "ax_failure", nodes, elapsed, 0, false, None);
}

fn update_degraded(health: &SharedHealth, state: &mut SnapshotState, now: Instant) {
    let reason = if !state.daily_budget_allows(now) {
        Some("daily budget exhausted".to_owned())
    } else {
        state.backoff_remaining(now).map(|remaining| {
            format!(
                "Accessibility traversal is backing off for {} seconds",
                remaining.as_secs()
            )
        })
    };
    match health.degraded.write() {
        Ok(mut current) => *current = reason,
        Err(_) => crate::trace::trace!(
            "component=content_snapshot phase=health action=update result=poisoned"
        ),
    }
}

mod error;

pub(super) use error::ScanError;
use error::scan_timed_out;
