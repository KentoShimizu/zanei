//! Daemon heartbeat construction and publication.

use std::collections::BTreeMap;

use time::OffsetDateTime;
use zanei_core::{
    normalize::format_timestamp,
    store::{DaemonPermissions, DaemonState, StoreStatus},
};

use super::{ActiveDaemon, DaemonError, StoreOwner, merge_collector_failures, probe_permissions};

impl ActiveDaemon<'_> {
    pub(super) fn publish_heartbeat(&mut self) -> Result<(), DaemonError> {
        let permissions = self.refresh_permissions();
        self.publish_heartbeat_with_permissions(permissions)
    }

    pub(super) fn refresh_permissions(&mut self) -> Option<DaemonPermissions> {
        if self.permission_request_worker.is_some() {
            return None;
        }
        match probe_permissions(&self.collectors.required_capabilities()) {
            Ok(permissions) => {
                self.degraded.remove("permissions");
                self.last_permissions = Some(permissions.clone());
            }
            Err(error) => {
                self.degraded
                    .insert("permissions".to_owned(), error.to_string());
            }
        }
        self.last_permissions.clone()
    }

    pub(super) fn publish_heartbeat_with_permissions(
        &mut self,
        permissions: Option<DaemonPermissions>,
    ) -> Result<(), DaemonError> {
        match self.reader.status() {
            Ok(status) => {
                self.last_status = status;
                self.degraded.remove("store_read");
            }
            Err(error) => {
                self.degraded
                    .insert("store_read".to_owned(), error.to_string());
            }
        }
        let collector_health = self.collectors.health();
        let mut degraded = self.degraded.clone();
        degraded.extend(self.pipeline.degraded()?);
        degraded.extend(collector_health.degraded);
        self.pipeline.heartbeat(DaemonState {
            pid: Some(i64::from(self.owner.pid)),
            started_at: Some(self.owner.started_at.clone()),
            instance_id: Some(self.owner.instance_id.clone()),
            mode: Some(self.owner.mode),
            heartbeat_at: Some(format_timestamp(OffsetDateTime::now_utc())),
            retention_hours: Some(self.active_retention_hours),
            paused_until: self.last_status.paused_until.clone(),
            events_captured: self.last_status.events_captured,
            events_dropped: self
                .base_dropped
                .saturating_add(collector_health.dropped)
                .saturating_add(self.pipeline.dropped()),
            last_event_ts: self.last_status.last_event_ts.clone(),
            degraded,
            collector_failures: merge_collector_failures(
                self.base_collector_failures,
                &collector_health.collector_failures,
            ),
            permissions,
        })
    }
}

pub(super) fn initial_heartbeat(
    owner: &StoreOwner,
    retention_hours: u64,
    status: &StoreStatus,
) -> DaemonState {
    DaemonState {
        pid: Some(i64::from(owner.pid)),
        started_at: Some(owner.started_at.clone()),
        instance_id: Some(owner.instance_id.clone()),
        mode: Some(owner.mode),
        heartbeat_at: Some(format_timestamp(OffsetDateTime::now_utc())),
        retention_hours: Some(retention_hours),
        paused_until: status.paused_until.clone(),
        events_captured: status.events_captured,
        events_dropped: status.events_dropped,
        last_event_ts: status.last_event_ts.clone(),
        degraded: BTreeMap::new(),
        collector_failures: status.collector_failures.clone(),
        permissions: None,
    }
}
