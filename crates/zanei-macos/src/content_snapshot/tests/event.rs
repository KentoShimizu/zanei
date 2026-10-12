use std::time::Instant;

use zanei_core::{
    config::{FilterConfig, RedactorKind},
    normalize::Normalizer,
    privacy::PrivacyFilter,
    schema::{
        CaptureContext, ContentSnapshotCutoff, ContentSnapshotData, ContentSnapshotLine,
        ContentSnapshotTrigger, EventData, RgbColor, WindowFraction,
    },
};

use crate::content_snapshot::{
    SnapshotTriggerKind,
    output::{SnapshotBody, build_raw_event},
    scheduler::ScheduledSnapshot,
    state::{SnapshotOrigin, SnapshotWindowKey},
};

use super::support::trigger;

#[test]
fn raw_event_matches_the_v4_content_snapshot_contract() {
    let candidate = ScheduledSnapshot {
        target: trigger(7, 11, SnapshotTriggerKind::Focus, Instant::now()),
        trigger: ContentSnapshotTrigger::Settle,
        activity_window: None,
        deferred: false,
    };
    let body = SnapshotBody::ax(
        "日本語 alice@example.com".to_owned(),
        None,
        candidate.trigger,
    );
    let event = build_raw_event(
        &candidate,
        SnapshotWindowKey {
            pid: 7,
            window_id: 11,
        },
        body.origin,
        body.data,
        CaptureContext {
            url: Some("https://example.com/path".into()),
            surface: None,
        },
        time::OffsetDateTime::UNIX_EPOCH,
    );

    assert_eq!(event.source, "macos.ax");
    assert_eq!(event.event_type, "content.snapshot");
    assert_eq!(event.app.pid, Some(7));
    assert_eq!(event.window.as_ref().and_then(|window| window.id), Some(11));
    assert!(event.element.is_none());
    assert_eq!(event.observed_at, Some(time::OffsetDateTime::UNIX_EPOCH));
    assert_eq!(
        event.capture_context.url.as_deref(),
        Some("https://example.com/path")
    );
    let EventData::ContentSnapshot(data) = &event.data else {
        panic!("content payload");
    };
    assert_eq!(data.text.as_deref(), Some("日本語 alice@example.com"));
    assert_eq!(data.chars, 21);
    assert_eq!(data.cutoff(), Some(None));
    assert_eq!(data.trigger, ContentSnapshotTrigger::Settle);

    let normalized = Normalizer::new()
        .push(event)
        .expect("normalize raw event")
        .pop()
        .expect("content snapshot emits immediately");
    assert_eq!(normalized.version, 4);
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

#[test]
fn raw_event_preserves_every_snapshot_cutoff_reason() {
    let candidate = ScheduledSnapshot {
        target: trigger(7, 11, SnapshotTriggerKind::Focus, Instant::now()),
        trigger: ContentSnapshotTrigger::Refresh,
        activity_window: None,
        deferred: false,
    };

    for cutoff in [
        ContentSnapshotCutoff::Time,
        ContentSnapshotCutoff::Nodes,
        ContentSnapshotCutoff::Bytes,
        ContentSnapshotCutoff::Stopped,
    ] {
        let body = SnapshotBody::ax("partial".to_owned(), Some(cutoff), candidate.trigger);
        let event = build_raw_event(
            &candidate,
            SnapshotWindowKey {
                pid: 7,
                window_id: 11,
            },
            body.origin,
            body.data,
            CaptureContext::default(),
            time::OffsetDateTime::UNIX_EPOCH,
        );
        let EventData::ContentSnapshot(data) = event.data else {
            panic!("content payload");
        };
        assert_eq!(data.cutoff(), Some(Some(cutoff)));
    }
}

#[test]
fn recognized_body_is_written_as_a_valid_ocr_snapshot() {
    // Catches recognized text written under the Accessibility source, which core rejects
    // because `lines` belongs only to `macos.ocr`.
    let candidate = ScheduledSnapshot {
        target: trigger(7, 11, SnapshotTriggerKind::Focus, Instant::now()),
        trigger: ContentSnapshotTrigger::Settle,
        activity_window: None,
        deferred: false,
    };
    let half = WindowFraction::from_ratio(0.5).expect("fraction");
    let line = ContentSnapshotLine {
        text: "hello".to_owned(),
        x: half,
        y: half,
        w: half,
        h: half,
        bg: RgbColor {
            red: 0,
            green: 0,
            blue: 0,
        },
        ink: None,
    };
    let data = ContentSnapshotData::recognized(
        Some("hello".to_owned()),
        5,
        None,
        candidate.trigger,
        vec![line],
    );
    let event = build_raw_event(
        &candidate,
        SnapshotWindowKey {
            pid: 7,
            window_id: 11,
        },
        SnapshotOrigin::Ocr,
        data,
        CaptureContext::default(),
        time::OffsetDateTime::UNIX_EPOCH,
    );

    assert_eq!(event.source, "macos.ocr");
    let normalized = Normalizer::new()
        .push(event)
        .expect("a recognized snapshot is valid")
        .pop()
        .expect("content snapshot emits immediately");
    assert_eq!(normalized.version, 4);
}
