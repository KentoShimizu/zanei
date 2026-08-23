//! Snapshot event construction, quarantine delivery, and bounded trace summaries.

use std::{
    sync::{
        atomic::Ordering,
        mpsc::{SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

use zanei_collector::RawEvent;
use zanei_core::schema::{
    CaptureContext, ContentSnapshotData, ContentSnapshotTrigger, EventData, Window,
};

use super::{
    SharedHealth, SnapshotCutoff, SnapshotWalkOutput,
    scheduler::ScheduledSnapshot,
    state::{SnapshotState, SnapshotWindowKey},
};
use crate::text_capture::{ChromeWindowKey, ReleasedEvent, TextQuarantine};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit(
    candidate: ScheduledSnapshot,
    output: SnapshotWalkOutput,
    key: SnapshotWindowKey,
    hash: u64,
    capture_context: CaptureContext,
    chrome_version: Option<u64>,
    reserved_at: Instant,
    state: &mut SnapshotState,
    sender: &SyncSender<RawEvent>,
    health: &SharedHealth,
    quarantine: &mut TextQuarantine,
) {
    let bytes = output.text.len();
    let metrics = TraceMetrics::from(&output);
    let event = build_raw_event(
        &candidate,
        key,
        output.text,
        output.complete,
        capture_context,
    );
    if let Some(version) = chrome_version {
        quarantine.hold_snapshot(
            event,
            ChromeWindowKey {
                pid: key.pid,
                window_id: key.window_id,
            },
            version,
            hash,
            reserved_at,
        );
        // Conservatively reserve limits now: even a later quarantine drop consumes
        // the global interval and daily budget, preventing unbounded held snapshots.
        state.reserve(key, bytes, reserved_at);
        trace_metrics(&candidate, "quarantine_reserved", metrics);
        return;
    }
    let gate = match sender.try_send(event) {
        Ok(()) => {
            state.reserve(key, bytes, reserved_at);
            state.record_hash(key, hash);
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
    trace_metrics(&candidate, gate, metrics);
}

pub(super) fn emit_released(
    events: Vec<ReleasedEvent>,
    sender: &SyncSender<RawEvent>,
    health: &SharedHealth,
    state: &mut SnapshotState,
) {
    for released in events {
        let (event, snapshot_hash) = released.into_parts();
        match sender.try_send(event) {
            Ok(()) => {
                if let Some((key, hash)) = snapshot_hash {
                    state.record_hash(
                        SnapshotWindowKey {
                            pid: key.pid,
                            window_id: key.window_id,
                        },
                        hash,
                    );
                }
            }
            Err(_) => {
                health.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
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
        observed_at: None,
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

pub(super) fn trace_output(candidate: &ScheduledSnapshot, gate: &str, output: &SnapshotWalkOutput) {
    crate::trace::trace!(
        "{}",
        trace_summary(candidate, gate, TraceMetrics::from(output))
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn trace_candidate(
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
        trace_summary(
            candidate,
            gate,
            TraceMetrics {
                nodes,
                degraded_nodes: 0,
                frameless_nodes: 0,
                elapsed,
                bytes,
                complete,
                cutoff,
            },
        )
    );
}

#[derive(Clone, Copy)]
struct TraceMetrics {
    nodes: usize,
    degraded_nodes: usize,
    frameless_nodes: usize,
    elapsed: Duration,
    bytes: usize,
    complete: bool,
    cutoff: Option<SnapshotCutoff>,
}

impl From<&SnapshotWalkOutput> for TraceMetrics {
    fn from(output: &SnapshotWalkOutput) -> Self {
        Self {
            nodes: output.nodes,
            degraded_nodes: output.degraded_nodes,
            frameless_nodes: output.frameless_nodes,
            elapsed: output.elapsed,
            bytes: output.text.len(),
            complete: output.complete,
            cutoff: output.cutoff,
        }
    }
}

fn trace_summary(candidate: &ScheduledSnapshot, gate: &str, metrics: TraceMetrics) -> String {
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
        "component=content_snapshot trigger={} gate={} nodes={} degraded_nodes={} frameless_nodes={} elapsed_ms={} bytes={} complete={} cutoff={} pid={} window_id={}",
        trigger,
        gate,
        metrics.nodes,
        metrics.degraded_nodes,
        metrics.frameless_nodes,
        metrics.elapsed.as_millis(),
        metrics.bytes,
        metrics.complete,
        metrics.cutoff.map_or("none", SnapshotCutoff::trace_name),
        candidate.target.app.pid,
        window_id
    )
}

fn trace_metrics(candidate: &ScheduledSnapshot, gate: &str, metrics: TraceMetrics) {
    crate::trace::trace!("{}", trace_summary(candidate, gate, metrics));
}

#[cfg(test)]
pub(super) fn test_trace_summary(candidate: &ScheduledSnapshot) -> String {
    trace_summary(
        candidate,
        "emit",
        TraceMetrics {
            nodes: 42,
            degraded_nodes: 0,
            frameless_nodes: 0,
            elapsed: Duration::from_millis(17),
            bytes: 512,
            complete: true,
            cutoff: None,
        },
    )
}
