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
use crate::text_capture::{ChromeWindowKey, TextQuarantine};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit(
    candidate: ScheduledSnapshot,
    output: SnapshotWalkOutput,
    key: SnapshotWindowKey,
    hash: u64,
    capture_context: CaptureContext,
    chrome_version: Option<u64>,
    state: &mut SnapshotState,
    sender: &SyncSender<RawEvent>,
    health: &SharedHealth,
    quarantine: &mut TextQuarantine,
) {
    let bytes = output.text.len();
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
        );
        trace_candidate(
            &candidate,
            "quarantine",
            output.nodes,
            output.elapsed,
            bytes,
            output.complete,
            output.cutoff,
        );
        return;
    }
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

pub(super) fn emit_released(
    events: Vec<RawEvent>,
    state: &mut SnapshotState,
    sender: &SyncSender<RawEvent>,
    health: &SharedHealth,
) {
    for event in events {
        let key = event
            .app
            .pid
            .zip(event.window.as_ref().and_then(|window| window.id));
        let (hash, bytes) = match &event.data {
            EventData::ContentSnapshot(data) => data
                .text
                .as_deref()
                .map_or((0, 0), |body| (SnapshotState::text_hash(body), body.len())),
            _ => (0, 0),
        };
        match sender.try_send(event) {
            Ok(()) => {
                if let Some((pid, window_id)) = key {
                    state.commit_save(
                        SnapshotWindowKey { pid, window_id },
                        hash,
                        bytes,
                        Instant::now(),
                    );
                }
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
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
        "component=content_snapshot trigger={} gate={} nodes={} degraded_nodes={} elapsed_ms={} bytes={} complete={} cutoff={} pid={} window_id={}",
        trigger,
        gate,
        metrics.nodes,
        metrics.degraded_nodes,
        metrics.elapsed.as_millis(),
        metrics.bytes,
        metrics.complete,
        metrics.cutoff.map_or("none", SnapshotCutoff::trace_name),
        candidate.target.app.pid,
        window_id
    )
}

#[cfg(test)]
pub(super) fn test_trace_summary(candidate: &ScheduledSnapshot) -> String {
    trace_summary(
        candidate,
        "emit",
        TraceMetrics {
            nodes: 42,
            degraded_nodes: 0,
            elapsed: Duration::from_millis(17),
            bytes: 512,
            complete: true,
            cutoff: None,
        },
    )
}
