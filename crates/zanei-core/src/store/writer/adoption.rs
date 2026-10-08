//! The one-time carry-over of a set-aside store's daemon state into the live
//! store that replaced it.

use rusqlite::params;

use crate::store::StoreStatus;

use super::{
    StoreError, StoreWriter, serialize_capabilities, serialize_collector_failures, signed,
    validate_optional_timestamp, validate_paused_until,
};

impl StoreWriter {
    /// Whether this store still owes its one adoption step: true from the
    /// transaction that created it until [`Self::adopt_daemon_state`] runs.
    /// Stores created before this step was recorded never owe it.
    pub fn daemon_state_adoption_pending(&self) -> Result<bool, StoreError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM daemon_state_adoption_pending)",
            [],
            |row| row.get(0),
        )?)
    }

    /// Completes this store's one adoption step, carrying the parts of the
    /// previous store's daemon state that outlive a store swap when `previous`
    /// is given: an active pause request (so an upgrade never silently resumes
    /// recording), the cumulative counters, the last event time, collector
    /// failure history, and the last capability report. The recorder identity
    /// and heartbeat are left to the next heartbeat.
    ///
    /// The step runs at most once per store, so a later `resume` or `pause` is
    /// never replaced by the previous store's request. A pause already
    /// requested for this store (`start --paused`) is newer than the carried
    /// one and is kept.
    pub fn adopt_daemon_state(&self, previous: Option<&StoreStatus>) -> Result<(), StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let pending =
            transaction.execute("DELETE FROM daemon_state_adoption_pending WHERE id = 1", [])? == 1;
        if let (true, Some(previous)) = (pending, previous) {
            validate_paused_until(previous.paused_until.as_deref())?;
            validate_optional_timestamp("last_event_ts", previous.last_event_ts.as_deref())?;
            let events_captured = signed("events_captured", previous.events_captured)?;
            let events_dropped = signed("events_dropped", previous.events_dropped)?;
            let collector_failures_json =
                serialize_collector_failures(&previous.collector_failures)?;
            let last_known_capabilities_json = previous
                .last_known_capabilities
                .as_ref()
                .map(serialize_capabilities)
                .transpose()?;
            transaction.execute(
                "UPDATE daemon_state SET paused_until = COALESCE(paused_until, ?1), \
                 events_captured = ?2, events_dropped = ?3, last_event_ts = ?4, \
                 collector_failures_json = ?5, last_known_capabilities_json = ?6 WHERE id = 1",
                params![
                    previous.paused_until,
                    events_captured,
                    events_dropped,
                    previous.last_event_ts,
                    collector_failures_json,
                    last_known_capabilities_json,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}
