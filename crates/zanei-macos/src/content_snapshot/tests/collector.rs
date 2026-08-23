use std::{
    sync::mpsc::sync_channel,
    thread,
    time::{Duration, Instant},
};

use zanei_collector::Collector;
use zanei_core::config::FilterConfig;

use crate::{
    chrome::chrome_eligibility_channel,
    content_snapshot::{ContentSnapshotCollector, SnapshotTriggerKind, snapshot_trigger_channel},
    secure_input::secure_input_test_channel,
    workspace::notification_channel,
};

use super::support::trigger;

#[test]
fn worker_starts_with_the_named_thread_reloads_filter_and_restarts_cleanly() {
    let (publisher, trigger_receiver) = snapshot_trigger_channel();
    let (_lifecycle_publisher, lifecycle_receiver) = notification_channel();
    let (secure_input, _secure_responder) = secure_input_test_channel();
    let (_chrome_publisher, chrome) = chrome_eligibility_channel(FilterConfig::default());
    let mut collector = ContentSnapshotCollector::new(
        trigger_receiver,
        lifecycle_receiver,
        secure_input,
        chrome,
        FilterConfig::default(),
    );
    let (output, events) = sync_channel(4);

    collector
        .start(output.clone())
        .expect("start content worker");
    assert!(collector.is_running());
    collector
        .replace_filter(FilterConfig {
            exclude_apps: vec!["dev.example.App".to_owned()],
            ..FilterConfig::default()
        })
        .expect("worker acknowledges filter generation");
    collector.stop();
    assert!(!collector.is_running());

    collector.start(output).expect("restart content worker");
    collector.stop();
    assert!(events.try_recv().is_err());
    drop(publisher);
}

#[test]
fn trigger_only_updates_scheduler_and_stop_discards_the_pending_settle() {
    let (publisher, trigger_receiver) = snapshot_trigger_channel();
    let (_lifecycle_publisher, lifecycle_receiver) = notification_channel();
    let (secure_input, _secure_responder) = secure_input_test_channel();
    let (_chrome_publisher, chrome) = chrome_eligibility_channel(FilterConfig::default());
    let mut collector = ContentSnapshotCollector::new(
        trigger_receiver,
        lifecycle_receiver,
        secure_input,
        chrome,
        FilterConfig::default(),
    );
    let (output, events) = sync_channel(4);
    collector.start(output).expect("start content worker");
    assert!(publisher.publish(trigger(7, 11, SnapshotTriggerKind::Focus, Instant::now())));
    thread::sleep(Duration::from_millis(50));
    assert!(
        events.try_recv().is_err(),
        "trigger receipt must not scan immediately"
    );
    collector.stop();
    thread::sleep(Duration::from_millis(10));
    assert!(
        events.try_recv().is_err(),
        "pending settle is discarded on stop"
    );
}
