use std::time::Instant;

use zanei_core::{
    config::{FilterConfig, RedactorKind},
    normalize::Normalizer,
    privacy::PrivacyFilter,
    schema::{CaptureContext, ContentSnapshotTrigger, EventData},
};

use crate::content_snapshot::{
    SnapshotTriggerKind, scheduler::ScheduledSnapshot, state::SnapshotWindowKey,
    worker::build_raw_event,
};

use super::support::trigger;

#[test]
fn raw_event_matches_the_v2_content_snapshot_contract() {
    let candidate = ScheduledSnapshot {
        target: trigger(7, 11, SnapshotTriggerKind::Focus, Instant::now()),
        trigger: ContentSnapshotTrigger::Settle,
        activity_window: None,
    };
    let event = build_raw_event(
        &candidate,
        SnapshotWindowKey {
            pid: 7,
            window_id: 11,
        },
        "日本語 alice@example.com".to_owned(),
        false,
        CaptureContext {
            website_host: Some("example.com".to_owned()),
        },
    );

    assert_eq!(event.source, "macos.ax");
    assert_eq!(event.event_type, "content.snapshot");
    assert_eq!(event.app.pid, Some(7));
    assert_eq!(event.window.as_ref().and_then(|window| window.id), Some(11));
    assert!(event.element.is_none());
    assert_eq!(
        event.capture_context.website_host.as_deref(),
        Some("example.com")
    );
    let EventData::ContentSnapshot(data) = &event.data else {
        panic!("content payload");
    };
    assert_eq!(data.text.as_deref(), Some("日本語 alice@example.com"));
    assert_eq!(data.chars, 21);
    assert!(!data.complete);
    assert_eq!(data.trigger, ContentSnapshotTrigger::Settle);

    let normalized = Normalizer::new()
        .push(event)
        .expect("normalize raw event")
        .pop()
        .expect("content snapshot emits immediately");
    assert_eq!(normalized.version, 2);
    let filtered = PrivacyFilter::new(FilterConfig {
        redactors: vec![RedactorKind::Email],
        ..FilterConfig::default()
    })
    .process(normalized)
    .expect("snapshot remains allowed");
    let EventData::ContentSnapshot(data) = filtered.data else {
        panic!("filtered content payload");
    };
    assert_eq!(data.text.as_deref(), Some("日本語 [REDACTED:email]"));
    assert_eq!(data.chars, 21);
}
