//! Content worker loop, ordered policy gates, traversal, and delivery commit.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

use zanei_collector::RawEvent;
use zanei_core::schema::CaptureContext;
use zanei_core::schema::{ContentSnapshotData, ContentSnapshotTrigger, EventData, Window};

use crate::{
    content_snapshot::{
        SnapshotAxApplication, SnapshotAxError, SnapshotCutoff, SnapshotTriggerReceiver,
        SnapshotWalkOutput,
        budget::WalkBudget,
        policy::SnapshotPolicy,
        scheduler::{ScheduledSnapshot, SnapshotScheduler},
        state::{SaveBlock, SnapshotState, SnapshotWindowKey},
        walker::{InstantWalkClock, SnapshotWalkError, WalkClock, walk_snapshot},
    },
    ffi::window_list::window_id_for_frame,
    workspace::WorkspaceEvent,
};

use super::{Control, SharedHealth, WORKER_POLL_INTERVAL, WorkerChannels};

#[allow(clippy::too_many_arguments)]
pub(super) fn run_worker(
    trigger: SnapshotTriggerReceiver,
    lifecycle: Receiver<WorkspaceEvent>,
    controls: Receiver<Control>,
    stop: Arc<AtomicBool>,
    sender: SyncSender<RawEvent>,
    mut policy: SnapshotPolicy,
    health: SharedHealth,
    mut state: SnapshotState,
) -> WorkerChannels {
    debug_assert_eq!(std::thread::current().name(), Some("zanei-content"));
    let mut scheduler = SnapshotScheduler::default();
    while !stop.load(Ordering::Acquire) {
        if service_controls(&controls, &mut policy, &mut scheduler) {
            break;
        }
        service_lifecycle(&lifecycle, &mut scheduler, &mut state);
        let wait = scheduler
            .next_deadline()
            .map_or(WORKER_POLL_INTERVAL, |deadline| {
                deadline
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO)
                    .min(WORKER_POLL_INTERVAL)
            });
        match trigger.recv_timeout(wait) {
            Ok(observation) => scheduler.observe(observation),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Some(candidate) = scheduler.take_due(Instant::now()) {
            process_candidate(candidate, &policy, &mut state, &sender, &stop, &health);
            if stop.load(Ordering::Acquire) {
                break;
            }
        }
        update_degraded(&health, &mut state, Instant::now());
    }
    scheduler.stop();
    WorkerChannels {
        trigger,
        lifecycle,
        state,
    }
}

fn service_controls(
    controls: &Receiver<Control>,
    policy: &mut SnapshotPolicy,
    scheduler: &mut SnapshotScheduler,
) -> bool {
    for control in controls.try_iter() {
        match control {
            Control::ReplaceFilter {
                filter,
                acknowledge,
            } => {
                policy.replace_filter(*filter);
                scheduler.replace_filter();
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

fn process_candidate(
    candidate: ScheduledSnapshot,
    policy: &SnapshotPolicy,
    state: &mut SnapshotState,
    sender: &SyncSender<RawEvent>,
    stop: &AtomicBool,
    health: &SharedHealth,
) {
    let now = Instant::now();
    let Some(key) = candidate.key() else {
        trace_candidate(&candidate, "window_id", 0, Duration::ZERO, 0, false, None);
        return;
    };
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
    if !policy.app_allows(&candidate.target.app) {
        trace_candidate(&candidate, "app_scope", 0, Duration::ZERO, 0, false, None);
        return;
    }
    if !policy.chrome_allows(&candidate.target.app, key.window_id) {
        trace_candidate(&candidate, "chrome", 0, Duration::ZERO, 0, false, None);
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
    let output = match scan(pid, key.window_id, stop) {
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
    if stop.load(Ordering::Acquire) {
        trace_output(&candidate, "stopped", &output);
        return;
    }
    if output.cutoff == Some(SnapshotCutoff::Stopped) {
        trace_output(&candidate, "stopped", &output);
        return;
    }
    state.record_scan_success(key.pid);
    if output.text.is_empty() {
        trace_output(&candidate, "empty", &output);
        return;
    }
    let hash = SnapshotState::text_hash(&output.text);
    if let Err(block) = state.evaluate_save(key, hash, output.text.len(), Instant::now()) {
        let gate = match block {
            SaveBlock::Duplicate => "duplicate",
            SaveBlock::GlobalInterval => "global_interval",
            SaveBlock::DailyBudget => "daily_budget",
        };
        trace_output(&candidate, gate, &output);
        return;
    }
    emit(candidate, output, key, hash, policy, state, sender, health);
}

fn scan(
    pid: i32,
    expected_window_id: i64,
    stop: &AtomicBool,
) -> Result<Option<SnapshotWalkOutput>, ScanError> {
    let clock = InstantWalkClock::start();
    let application = SnapshotAxApplication::new(pid)?;
    debug_assert_eq!(application.pid(), pid);
    if let Some(output) = initial_time_cutoff(&clock, 1) {
        return Ok(Some(output));
    }
    let Some(window) = application.focused_window()? else {
        return Ok(None);
    };
    if let Some(output) = initial_time_cutoff(&clock, 2) {
        return Ok(Some(output));
    }
    let Some(frame) = window.frame()? else {
        return Ok(None);
    };
    if let Some(output) = initial_time_cutoff(&clock, 3) {
        return Ok(Some(output));
    }
    let window_id = window
        .window_number()?
        .or_else(|| window_id_for_frame(i64::from(pid), frame));
    if window_id != Some(expected_window_id) {
        return Ok(None);
    }
    if let Some(output) = initial_time_cutoff(&clock, 4) {
        return Ok(Some(output));
    }
    let mut output = walk_snapshot(window, frame, WalkBudget::DESIGN, &clock, || {
        stop.load(Ordering::Acquire)
    })
    .map_err(ScanError::Walk)?;
    output.ax_calls = output.ax_calls.saturating_add(4);
    Ok(Some(output))
}

fn initial_time_cutoff(clock: &impl WalkClock, ax_calls: usize) -> Option<SnapshotWalkOutput> {
    let elapsed = clock.elapsed();
    (elapsed >= WalkBudget::DESIGN.wall_time).then(|| SnapshotWalkOutput {
        text: String::new(),
        nodes: 0,
        ax_calls,
        elapsed,
        complete: false,
        cutoff: Some(SnapshotCutoff::Time),
    })
}

#[cfg(test)]
pub(super) fn test_live_scan(
    pid: i32,
    window_id: i64,
) -> Result<Option<SnapshotWalkOutput>, String> {
    scan(pid, window_id, &AtomicBool::new(false)).map_err(|error| error.to_string())
}

#[allow(clippy::too_many_arguments)]
fn emit(
    candidate: ScheduledSnapshot,
    output: SnapshotWalkOutput,
    key: SnapshotWindowKey,
    hash: u64,
    policy: &SnapshotPolicy,
    state: &mut SnapshotState,
    sender: &SyncSender<RawEvent>,
    health: &SharedHealth,
) {
    let bytes = output.text.len();
    let capture_context = policy.capture_context(&candidate.target.app, key.window_id);
    let event = build_raw_event(
        &candidate,
        key,
        output.text,
        output.complete,
        capture_context,
    );
    let gate = match sender.try_send(event) {
        Ok(()) => {
            state.commit_save(key, hash, bytes, Instant::now());
            "emit"
        }
        Err(TrySendError::Full(_)) => {
            health.dropped.fetch_add(1, Ordering::Relaxed);
            "output_full"
        }
        Err(TrySendError::Disconnected(_)) => {
            health.dropped.fetch_add(1, Ordering::Relaxed);
            "output_disconnected"
        }
    };
    trace_candidate(
        &candidate,
        gate,
        output.nodes,
        output.elapsed,
        bytes,
        output.complete,
        output.cutoff,
    );
}

pub(super) fn build_raw_event(
    candidate: &ScheduledSnapshot,
    key: SnapshotWindowKey,
    text: String,
    complete: bool,
    capture_context: CaptureContext,
) -> RawEvent {
    let chars =
        u64::try_from(text.chars().count()).expect("the 32 KiB design limit always fits in u64");
    RawEvent {
        source: "macos.ax".to_owned(),
        event_type: "content.snapshot".to_owned(),
        app: candidate.target.app.raw_app(),
        window: Some(Window {
            title: candidate.target.window.title.clone(),
            id: Some(key.window_id),
        }),
        element: None,
        data: EventData::ContentSnapshot(ContentSnapshotData {
            text: Some(text),
            chars,
            complete,
            trigger: candidate.trigger,
        }),
        capture_context,
    }
}

#[derive(Debug)]
enum ScanError {
    Ax(SnapshotAxError),
    Walk(SnapshotWalkError),
}

impl From<SnapshotAxError> for ScanError {
    fn from(error: SnapshotAxError) -> Self {
        Self::Ax(error)
    }
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

fn scan_timed_out(error: &ScanError) -> bool {
    match error {
        ScanError::Ax(error) => error.is_timeout(),
        ScanError::Walk(error) => match &error.source {
            crate::content_snapshot::walker::SnapshotReadError::Ax(error) => error.is_timeout(),
            crate::content_snapshot::walker::SnapshotReadError::Contract(_) => false,
        },
    }
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

fn trace_output(candidate: &ScheduledSnapshot, gate: &str, output: &SnapshotWalkOutput) {
    trace_candidate(
        candidate,
        gate,
        output.nodes,
        output.elapsed,
        output.text.len(),
        output.complete,
        output.cutoff,
    );
}

fn trace_candidate(
    candidate: &ScheduledSnapshot,
    gate: &str,
    nodes: usize,
    elapsed: Duration,
    bytes: usize,
    complete: bool,
    cutoff: Option<SnapshotCutoff>,
) {
    crate::trace::trace!(
        "{}",
        trace_summary(candidate, gate, nodes, elapsed, bytes, complete, cutoff)
    );
}

fn trace_summary(
    candidate: &ScheduledSnapshot,
    gate: &str,
    nodes: usize,
    elapsed: Duration,
    bytes: usize,
    complete: bool,
    cutoff: Option<SnapshotCutoff>,
) -> String {
    let trigger = match candidate.trigger {
        ContentSnapshotTrigger::Settle => "settle",
        ContentSnapshotTrigger::Refresh => "refresh",
        ContentSnapshotTrigger::FocusOut => "focus_out",
    };
    let window_id = candidate
        .target
        .window
        .id
        .map_or_else(|| "none".to_owned(), |window_id| window_id.to_string());
    format!(
        "component=content_snapshot trigger={} gate={} nodes={} elapsed_ms={} bytes={} complete={} cutoff={} pid={} window_id={}",
        trigger,
        gate,
        nodes,
        elapsed.as_millis(),
        bytes,
        complete,
        cutoff.map_or("none", SnapshotCutoff::trace_name),
        candidate.target.app.pid,
        window_id
    )
}

#[cfg(test)]
pub(super) fn test_trace_summary(candidate: &ScheduledSnapshot) -> String {
    trace_summary(
        candidate,
        "emit",
        42,
        Duration::from_millis(17),
        512,
        true,
        None,
    )
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ax(error) => error.fmt(formatter),
            Self::Walk(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ax(error) => Some(error),
            Self::Walk(error) => Some(error),
        }
    }
}
