//! Snapshot event construction, quarantine delivery, and bounded trace summaries.

use std::{
    sync::{
        atomic::Ordering,
        mpsc::{SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

use time::OffsetDateTime;
use zanei_collector::RawEvent;
use zanei_core::privacy::PrivacyScope;
use zanei_core::schema::{
    CaptureContext, ContentSnapshotCutoff, ContentSnapshotData, ContentSnapshotTrigger, EventData,
    RECOGNIZED_TEXT_SOURCE, Window,
};

use super::{
    SharedHealth, SnapshotWalkOutput,
    scheduler::ScheduledSnapshot,
    state::{SaveBlock, SnapshotOrigin, SnapshotState, SnapshotWindowKey},
};
use crate::{
    browser_context::BrowserTarget,
    capture_policy::{CaptureDecision, CapturePolicy},
    text_capture::{ChromeWindowKey, ReleasedEvent, TextQuarantine},
};

const AX_SOURCE: &str = "macos.ax";

/// A snapshot body for the send-time gates, from either origin.
pub(super) struct SnapshotBody {
    pub(super) origin: SnapshotOrigin,
    pub(super) data: ContentSnapshotData,
    hash: u64,
    /// Daily-budget size: the text, and for recognized text the whole payload with its lines.
    bytes: usize,
    /// Fingerprint of the window image recognized text came from.
    image: Option<u64>,
}

impl SnapshotBody {
    pub(super) fn ax(
        text: String,
        cutoff: Option<ContentSnapshotCutoff>,
        trigger: ContentSnapshotTrigger,
    ) -> Self {
        let chars = u64::try_from(text.chars().count())
            .expect("the 32 KiB design limit always fits in u64");
        Self {
            origin: SnapshotOrigin::Ax,
            hash: SnapshotState::text_hash(&text),
            bytes: text.len(),
            data: ContentSnapshotData::new(Some(text), chars, cutoff, trigger),
            image: None,
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used once the worker recognizes text")
    )]
    pub(super) fn recognized(data: ContentSnapshotData, image: u64) -> Self {
        Self {
            origin: SnapshotOrigin::Ocr,
            hash: SnapshotState::text_hash(
                data.text
                    .as_deref()
                    .expect("recognized bodies always have text"),
            ),
            bytes: serde_json::to_vec(&data)
                .expect("content snapshot payloads serialize to JSON")
                .len(),
            data,
            image: Some(image),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit(
    candidate: ScheduledSnapshot,
    output: &SnapshotWalkOutput,
    body: SnapshotBody,
    key: SnapshotWindowKey,
    capture_policy: &CapturePolicy,
    earlier_decision: &CaptureDecision,
    observed_at: OffsetDateTime,
    reserved_at: Instant,
    state: &mut SnapshotState,
    sender: &SyncSender<RawEvent>,
    health: &SharedHealth,
    quarantine: &mut TextQuarantine,
) {
    let SnapshotBody {
        origin,
        data,
        hash,
        bytes,
        image,
    } = body;
    let metrics = TraceMetrics {
        bytes,
        source: origin_trace_name(origin),
        ..TraceMetrics::from(output)
    };
    let decision = capture_policy.decision_at_send(
        PrivacyScope::ContentSnapshot,
        &candidate.target.app.raw_app(),
        Some(key.window_id),
        candidate.target.window.title.as_deref(),
        Some(earlier_decision),
    );
    if !decision.is_allowed() {
        trace_metrics(&candidate, "app_scope", metrics);
        return;
    }
    let chrome_version = decision.chrome_version();
    if BrowserTarget::from_bundle_id(candidate.target.app.bundle_id.as_deref()).is_some()
        && chrome_version.is_none()
    {
        trace_metrics(&candidate, "chrome_version", metrics);
        return;
    }
    if let Err(block) = state.evaluate_save(key, origin, hash, bytes, reserved_at) {
        let gate = match block {
            SaveBlock::Duplicate => "duplicate",
            SaveBlock::GlobalInterval => "global_interval",
            SaveBlock::DailyBudget => "daily_budget",
        };
        trace_metrics(&candidate, gate, metrics);
        return;
    }
    let event = build_raw_event(
        &candidate,
        key,
        origin,
        data,
        decision.capture_context(),
        observed_at,
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
            state.record_hash(key, origin, hash);
            if let Some(image) = image {
                state.record_recognized_image(key, image);
            }
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
        let origin = if event.source == RECOGNIZED_TEXT_SOURCE {
            SnapshotOrigin::Ocr
        } else {
            SnapshotOrigin::Ax
        };
        match sender.try_send(event) {
            Ok(()) => {
                if let Some((key, hash)) = snapshot_hash {
                    state.record_hash(
                        SnapshotWindowKey {
                            pid: key.pid,
                            window_id: key.window_id,
                        },
                        origin,
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
    origin: SnapshotOrigin,
    data: ContentSnapshotData,
    capture_context: CaptureContext,
    observed_at: OffsetDateTime,
) -> RawEvent {
    RawEvent {
        observed_at: Some(observed_at),
        source: match origin {
            SnapshotOrigin::Ax => AX_SOURCE,
            SnapshotOrigin::Ocr => RECOGNIZED_TEXT_SOURCE,
        }
        .to_owned(),
        event_type: "content.snapshot".to_owned(),
        app: candidate.target.app.raw_app(),
        window: Some(Window {
            title: candidate.target.window.title.clone(),
            id: Some(key.window_id),
        }),
        element: None,
        data: EventData::ContentSnapshot(data),
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
    cutoff: Option<ContentSnapshotCutoff>,
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
                source: "none",
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
    cutoff: Option<ContentSnapshotCutoff>,
    source: &'static str,
}

impl From<&SnapshotWalkOutput> for TraceMetrics {
    fn from(output: &SnapshotWalkOutput) -> Self {
        Self {
            nodes: output.nodes,
            degraded_nodes: output.degraded_nodes,
            frameless_nodes: output.frameless_nodes,
            elapsed: output.elapsed,
            bytes: output.text.len(),
            complete: output.cutoff.is_none(),
            cutoff: output.cutoff,
            source: "ax",
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
        "component=content_snapshot trigger={} gate={} source={} nodes={} degraded_nodes={} frameless_nodes={} elapsed_ms={} bytes={} complete={} cutoff={} pid={} window_id={}",
        trigger,
        gate,
        metrics.source,
        metrics.nodes,
        metrics.degraded_nodes,
        metrics.frameless_nodes,
        metrics.elapsed.as_millis(),
        metrics.bytes,
        metrics.complete,
        metrics.cutoff.map_or("none", cutoff_trace_name),
        candidate.target.app.pid,
        window_id
    )
}

const fn origin_trace_name(origin: SnapshotOrigin) -> &'static str {
    match origin {
        SnapshotOrigin::Ax => "ax",
        SnapshotOrigin::Ocr => "ocr",
    }
}

pub(super) const fn cutoff_trace_name(cutoff: ContentSnapshotCutoff) -> &'static str {
    match cutoff {
        ContentSnapshotCutoff::Time => "time",
        ContentSnapshotCutoff::Nodes => "nodes",
        ContentSnapshotCutoff::Bytes => "bytes",
        ContentSnapshotCutoff::Stopped => "stopped",
    }
}

fn trace_metrics(candidate: &ScheduledSnapshot, gate: &str, metrics: TraceMetrics) {
    crate::trace::trace!("{}", trace_summary(candidate, gate, metrics));
}

#[cfg(test)]
pub(super) fn test_trace_summary(
    candidate: &ScheduledSnapshot,
    cutoff: Option<ContentSnapshotCutoff>,
) -> String {
    let output = SnapshotWalkOutput {
        text: "x".repeat(512),
        nodes: 42,
        ax_calls: 0,
        elapsed: Duration::from_millis(17),
        cutoff,
        degraded_nodes: 0,
        frameless_nodes: 0,
    };
    trace_summary(candidate, "emit", TraceMetrics::from(&output))
}
