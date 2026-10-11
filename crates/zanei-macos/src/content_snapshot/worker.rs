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
        SnapshotTriggerReceiver, SnapshotWalkOutput,
        coverage::WalkAnalysis,
        ocr_engine::OcrEngine,
        output::{emit_released, trace_candidate, trace_output},
        scheduler::{ScheduledSnapshot, SnapshotScheduler},
        state::SnapshotState,
    },
    focus_context::FocusContext,
    text_capture::TextQuarantine,
    workspace::WorkspaceEvent,
};

use super::{Control, SharedHealth, WORKER_POLL_INTERVAL};
use crate::{CapturePolicy, chrome::ChromeObserver};

mod recognition;
pub(super) mod scan;
mod scheduling;

use recognition::Delivery;
use recognition::{WalkedCandidate, finish_recognition, start_recognition};
use scheduling::CandidateTime;
pub(super) use scheduling::seed_scheduler_from_focus;

/// One walked window: its text and the analysis of the same traversal.
pub(super) type ScannedWindow = (SnapshotWalkOutput, WalkAnalysis);

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
    ocr: OcrEngine,
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
        ocr,
        scan::scan,
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
    ocr: OcrEngine,
    mut scan_window: F,
) where
    F: FnMut(
        i32,
        i64,
        &AtomicBool,
        &mut dyn FnMut(Option<String>) -> bool,
    ) -> Result<Option<ScannedWindow>, ScanError>,
{
    debug_assert_eq!(std::thread::current().name(), Some("zanei-content"));
    let mut scheduler = SnapshotScheduler::default();
    seed_scheduler_from_focus(&mut scheduler, &focus_context, Instant::now());
    let mut quarantine = TextQuarantine::new(chrome_observer);
    // While recognition runs on its own thread, the loop keeps servicing controls and
    // triggers but takes no new candidate; stopping discards the unread result.
    let mut recognizing: Option<WalkedCandidate> = None;
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
        let wait = loop_wait(
            recognizing.is_some(),
            scheduler.next_deadline(),
            Instant::now(),
        );
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
        let mut delivery = Delivery {
            policy: &capture_policy,
            sender: &sender,
            health: &health,
            quarantine: &mut quarantine,
        };
        if let Some(walked) = recognizing.take() {
            match ocr.poll() {
                // A result that arrives once stop is requested is discarded.
                Some(_) if stop.load(Ordering::Acquire) => {}
                Some(outcome) => finish_recognition(walked, outcome, state, &mut delivery),
                None => recognizing = Some(walked),
            }
        }
        while recognizing.is_none()
            && let Some(candidate) = scheduler.take_due(Instant::now())
        {
            let taken_at = CandidateTime::now();
            recognizing = process_candidate(
                candidate,
                taken_at,
                state,
                CandidateContext {
                    delivery: &mut delivery,
                    stop: &stop,
                    focus_context: &focus_context,
                    ocr: &ocr,
                    scan_window: &mut scan_window,
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

/// A due deadline must not zero the wait while recognizing, when no candidate can be taken.
pub(super) fn loop_wait(
    recognizing: bool,
    next_deadline: Option<Instant>,
    now: Instant,
) -> Duration {
    match next_deadline {
        Some(deadline) if !recognizing => deadline
            .checked_duration_since(now)
            .unwrap_or(Duration::ZERO)
            .min(WORKER_POLL_INTERVAL),
        _ => WORKER_POLL_INTERVAL,
    }
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

struct CandidateContext<'a, 'b, F> {
    delivery: &'a mut Delivery<'b>,
    stop: &'a AtomicBool,
    focus_context: &'a FocusContext,
    ocr: &'a OcrEngine,
    scan_window: &'a mut F,
}

/// Runs the gates and the walk. Returns the candidate when its text is being recognized.
fn process_candidate<F>(
    mut candidate: ScheduledSnapshot,
    taken_at: CandidateTime,
    state: &mut SnapshotState,
    context: CandidateContext<'_, '_, F>,
) -> Option<WalkedCandidate>
where
    F: FnMut(
        i32,
        i64,
        &AtomicBool,
        &mut dyn FnMut(Option<String>) -> bool,
    ) -> Result<Option<ScannedWindow>, ScanError>,
{
    let CandidateContext {
        delivery,
        stop,
        focus_context,
        ocr,
        scan_window,
    } = context;
    let policy = delivery.policy;
    let health = delivery.health;
    let now = taken_at.monotonic;
    let Some(key) = candidate.key() else {
        trace_candidate(&candidate, "window_id", 0, Duration::ZERO, 0, false, None);
        return None;
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
        return None;
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
        return None;
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
        return None;
    }
    if !policy.refresh_activity_allows(candidate.activity_window) {
        trace_candidate(&candidate, "activity", 0, Duration::ZERO, 0, false, None);
        return None;
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
        return None;
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
        return None;
    }
    if !state.backoff_allows(key.pid, now) {
        trace_candidate(&candidate, "pid_backoff", 0, Duration::ZERO, 0, false, None);
        return None;
    }
    let Ok(pid) = i32::try_from(key.pid) else {
        trace_candidate(&candidate, "pid", 0, Duration::ZERO, 0, false, None);
        return None;
    };
    let (scan_result, read_decision) = {
        let mut read_decision = None;
        let mut read_allowed = |title: Option<String>| {
            candidate.target.window.title = title.clone();
            let decision = policy.decision(
                PrivacyScope::ContentSnapshot,
                &candidate.target.app.raw_app(),
                Some(key.window_id),
                title.as_deref(),
            );
            let allowed = decision.is_allowed();
            read_decision = Some(decision);
            allowed
        };
        (
            scan_window(pid, key.window_id, stop, &mut read_allowed),
            read_decision,
        )
    };
    let (output, analysis) = match scan_result {
        Ok(Some(scanned)) => scanned,
        Ok(None) => {
            trace_candidate(&candidate, "stale", 0, Duration::ZERO, 0, false, None);
            return None;
        }
        Err(error) => {
            record_scan_failure(state, health, key.pid, now, &candidate, &error);
            return None;
        }
    };
    let Some(read_decision) = read_decision else {
        trace_candidate(
            &candidate,
            "window_policy_unavailable",
            0,
            Duration::ZERO,
            0,
            false,
            None,
        );
        return None;
    };
    health.failures.fetch_add(
        u64::try_from(output.degraded_nodes).expect("degraded node count must fit u64"),
        Ordering::Relaxed,
    );
    if stop.load(Ordering::Acquire) || output.cutoff == Some(ContentSnapshotCutoff::Stopped) {
        trace_output(&candidate, "stopped", &output);
        return None;
    }
    state.record_scan_success(key.pid);
    let walked = WalkedCandidate {
        candidate,
        key,
        decision: read_decision,
        observed_at: taken_at.wall,
        output,
        blank_coverage: analysis.blank_coverage,
    };
    start_recognition(walked, analysis, ocr, state, delivery)
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
