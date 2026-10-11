//! Choice between the Accessibility body and recognized text for one walked candidate.

use std::{
    sync::{atomic::Ordering, mpsc::SyncSender},
    time::{Duration, Instant},
};

use time::OffsetDateTime;
use zanei_collector::RawEvent;

use crate::{
    CapturePolicy,
    capture_policy::CaptureDecision,
    content_snapshot::{
        SharedHealth, SnapshotWalkOutput,
        coverage::WalkAnalysis,
        ocr_engine::{OcrEngine, OcrOutcome, OcrRequest},
        output::{SnapshotBody, emit, trace_output},
        scheduler::ScheduledSnapshot,
        state::{SnapshotState, SnapshotWindowKey},
    },
    text_capture::TextQuarantine,
};

/// The send path's shared dependencies.
pub(in crate::content_snapshot) struct Delivery<'a> {
    pub(in crate::content_snapshot) policy: &'a CapturePolicy,
    pub(in crate::content_snapshot) sender: &'a SyncSender<RawEvent>,
    pub(in crate::content_snapshot) health: &'a SharedHealth,
    pub(in crate::content_snapshot) quarantine: &'a mut TextQuarantine,
}

/// A candidate whose walk completed and whose body is not yet sent.
pub(in crate::content_snapshot) struct WalkedCandidate {
    pub(in crate::content_snapshot) candidate: ScheduledSnapshot,
    pub(in crate::content_snapshot) key: SnapshotWindowKey,
    pub(in crate::content_snapshot) decision: CaptureDecision,
    pub(in crate::content_snapshot) observed_at: OffsetDateTime,
    pub(in crate::content_snapshot) output: SnapshotWalkOutput,
    pub(in crate::content_snapshot) blank_coverage: f64,
}

/// Sends the Accessibility body, or submits the window for recognition when Accessibility
/// exposes too little of it and recognition is available. Returns the submitted candidate.
pub(super) fn start_recognition(
    walked: WalkedCandidate,
    analysis: WalkAnalysis,
    ocr: &OcrEngine,
    state: &mut SnapshotState,
    delivery: &mut Delivery<'_>,
) -> Option<WalkedCandidate> {
    if !analysis.ax_insufficient() {
        trace_recognition(&walked, "sufficient", None);
        send_ax(walked, state, delivery);
        return None;
    }
    if let Err(reason) = ocr.access() {
        trace_recognition(&walked, reason.trace_name(), None);
        send_ax(walked, state, delivery);
        return None;
    }
    if !delivery.policy.secure_input_allows() {
        trace_output(&walked.candidate, "secure_input", &walked.output);
        return None;
    }
    ocr.submit(OcrRequest {
        window_id: walked.key.window_id,
        stored_image: state.recognized_image(walked.key),
        analysis,
        trigger: walked.candidate.trigger,
    });
    Some(walked)
}

pub(super) fn finish_recognition(
    walked: WalkedCandidate,
    outcome: OcrOutcome,
    state: &mut SnapshotState,
    delivery: &mut Delivery<'_>,
) {
    match outcome {
        OcrOutcome::Recognized {
            data,
            image,
            capture,
            recognize,
            lines,
        } => {
            trace_recognition(&walked, "recognized", Some((capture, recognize, lines)));
            send(
                walked,
                SnapshotBody::recognized(data, image),
                state,
                delivery,
            );
        }
        OcrOutcome::Unchanged => trace_recognition(&walked, "unchanged", None),
        OcrOutcome::Unavailable(reason) => {
            if reason.is_failure() {
                delivery.health.failures.fetch_add(1, Ordering::Relaxed);
            }
            trace_recognition(&walked, reason.trace_name(), None);
            send_ax(walked, state, delivery);
        }
    }
}

fn send_ax(walked: WalkedCandidate, state: &mut SnapshotState, delivery: &mut Delivery<'_>) {
    if walked.output.text.is_empty() {
        trace_output(&walked.candidate, "empty", &walked.output);
        return;
    }
    let body = SnapshotBody::ax(
        walked.output.text.clone(),
        walked.output.cutoff,
        walked.candidate.trigger,
    );
    send(walked, body, state, delivery);
}

fn send(
    walked: WalkedCandidate,
    body: SnapshotBody,
    state: &mut SnapshotState,
    delivery: &mut Delivery<'_>,
) {
    if !delivery.policy.secure_input_allows() {
        trace_output(&walked.candidate, "secure_input", &walked.output);
        return;
    }
    emit(
        walked.candidate,
        &walked.output,
        body,
        walked.key,
        delivery.policy,
        &walked.decision,
        walked.observed_at,
        Instant::now(),
        state,
        delivery.sender,
        delivery.health,
        delivery.quarantine,
    );
}

fn trace_recognition(
    walked: &WalkedCandidate,
    outcome: &str,
    recognized: Option<(Duration, Duration, usize)>,
) {
    let (capture, recognize, lines) = recognized.unwrap_or_default();
    crate::trace::trace!(
        "component=content_snapshot phase=ocr outcome={} blank_coverage={:.3} capture_ms={} recognize_ms={} ocr_lines={} pid={} window_id={}",
        outcome,
        walked.blank_coverage,
        capture.as_millis(),
        recognize.as_millis(),
        lines,
        walked.key.pid,
        walked.key.window_id
    );
}
