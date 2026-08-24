use std::time::Instant;

use zanei_core::schema::ContentSnapshotTrigger;

use crate::content_snapshot::{
    SnapshotTriggerKind, output::test_trace_summary, scheduler::ScheduledSnapshot,
};

use super::support::trigger;

#[test]
fn trace_contains_only_metrics_and_identifiers_never_content_context() {
    let mut target = trigger(7, 11, SnapshotTriggerKind::Focus, Instant::now());
    target.window.title = Some("TOP SECRET TITLE".to_owned());
    let candidate = ScheduledSnapshot {
        target,
        trigger: ContentSnapshotTrigger::Settle,
        activity_window: None,
    };

    let trace = test_trace_summary(&candidate);
    for required in [
        "component=content_snapshot",
        "trigger=settle",
        "gate=emit",
        "nodes=42",
        "frameless_nodes=0",
        "elapsed_ms=17",
        "bytes=512",
        "complete=true",
        "cutoff=none",
        "pid=7",
        "window_id=11",
    ] {
        assert!(trace.contains(required), "missing {required}: {trace}");
    }
    for forbidden in ["TOP SECRET", "title=", "text=", "url=", "host="] {
        assert!(
            !trace.contains(forbidden),
            "trace leaked {forbidden}: {trace}"
        );
    }
}
