use std::time::Instant;

use crate::{
    ax::NativeWindow,
    content_snapshot::{SnapshotTrigger, SnapshotTriggerKind},
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
