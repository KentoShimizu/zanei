use std::time::Instant;

use crate::{
    ax::NativeWindow,
    content_snapshot::{
        SnapshotTrigger, SnapshotTriggerKind,
        coverage::{WalkAnalysis, WalkAnalyzer},
        ocr_engine::{OcrBackend, OcrEngine, OcrUnavailable},
    },
    ffi::ax::{AxFrame, AxPoint, AxSize},
    workspace::{ApplicationActivationPolicy, ApplicationInfo},
};

pub(super) fn app(pid: i64, bundle_id: &str) -> ApplicationInfo {
    ApplicationInfo {
        name: "Example".to_owned(),
        bundle_id: Some(bundle_id.to_owned()),
        pid,
        activation_policy: ApplicationActivationPolicy::Regular,
    }
}

pub(super) fn trigger(
    pid: i64,
    window_id: i64,
    kind: SnapshotTriggerKind,
    observed_at: Instant,
) -> SnapshotTrigger {
    SnapshotTrigger {
        app: app(pid, "dev.example.App"),
        window: NativeWindow {
            title: Some(format!("Window {window_id}")),
            id: Some(window_id),
        },
        kind,
        observed_at,
    }
}

pub(super) const fn frame(x: f64, y: f64, width: f64, height: f64) -> AxFrame {
    AxFrame {
        origin: AxPoint { x, y },
        size: AxSize { width, height },
    }
}

/// Analysis of a walk whose text Accessibility exposes.
pub(super) fn sufficient() -> WalkAnalysis {
    WalkAnalyzer::new().finish()
}

/// Recognition without Screen Recording: it never captures.
pub(super) fn ocr_off() -> OcrEngine {
    OcrEngine::start(OcrBackend {
        access: || Err(OcrUnavailable::Permission),
        capture: Box::new(|_| unreachable!("nothing is captured without access")),
        recognize: Box::new(|_| unreachable!("nothing is recognized without access")),
    })
    .expect("start recognition thread")
}
