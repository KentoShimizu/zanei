use std::collections::BTreeMap;

use zanei_core::store::{DaemonState, StoreReader};

use super::{
    DaemonError,
    collectors::{CollectorSet, merge_collector_failures},
    pipeline::{Pipeline, SharedStoreWriter},
};

pub(super) fn shutdown_daemon(
    loop_result: Result<(), DaemonError>,
    writer: &SharedStoreWriter,
    reader: &StoreReader,
    collectors: &mut CollectorSet,
    pipeline: &mut Pipeline,
    base_dropped: u64,
    base_collector_failures: &BTreeMap<String, u64>,
) -> Result<(), DaemonError> {
    let store_accepts_intake = pipeline
        .store_health()
        .is_ok_and(|health| health.accepts_intake());
    if pipeline.is_finished() || !store_accepts_intake {
        collectors.suspend();
    } else {
        collectors.stop();
    }
    let pipeline_result = pipeline.shutdown();
    let clear_result = clear_heartbeat(
        writer,
        reader,
        collectors,
        pipeline,
        base_dropped,
        base_collector_failures,
    );
    loop_result.and(pipeline_result).and(clear_result)
}

fn clear_heartbeat(
    writer: &SharedStoreWriter,
    reader: &StoreReader,
    collectors: &CollectorSet,
    pipeline: &Pipeline,
    base_dropped: u64,
    base_collector_failures: &BTreeMap<String, u64>,
) -> Result<(), DaemonError> {
    let writer = writer
        .lock()
        .map_err(|_| DaemonError::SynchronizationPoisoned {
            name: "store writer",
        })?;
    let status = reader.status()?;
    let collector_health = collectors.health();
    writer.write_daemon_state(&DaemonState {
        pid: None,
        started_at: None,
        instance_id: None,
        mode: None,
        heartbeat_at: None,
        retention_hours: None,
        paused_until: status.paused_until,
        events_captured: status.events_captured,
        events_dropped: base_dropped
            .saturating_add(collector_health.dropped)
            .saturating_add(pipeline.dropped()),
        last_event_ts: status.last_event_ts,
        degraded: BTreeMap::new(),
        collector_failures: merge_collector_failures(
            base_collector_failures,
            &collector_health.collector_failures,
        ),
        capabilities: None,
    })?;
    Ok(())
}
