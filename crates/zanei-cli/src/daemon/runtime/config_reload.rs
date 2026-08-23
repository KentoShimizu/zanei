//! Retention state transitions driven by config reloads.

use time::OffsetDateTime;

use super::{ActiveDaemon, DaemonError, apply_retention, lock_writer};

impl ActiveDaemon<'_> {
    pub(super) fn request_retention_reload(
        &mut self,
        requested_retention_hours: u64,
        now: OffsetDateTime,
    ) -> Result<bool, DaemonError> {
        if requested_retention_hours == self.active_retention_hours {
            self.pending_retention_hours = None;
            self.degraded.remove("retention");
            return Ok(false);
        }
        self.pending_retention_hours = Some(requested_retention_hours);
        self.retry_pending_retention(now)
    }

    pub(super) fn retry_pending_retention(
        &mut self,
        now: OffsetDateTime,
    ) -> Result<bool, DaemonError> {
        let Some(retention_hours) = self.pending_retention_hours else {
            return Ok(false);
        };
        let purge_result = apply_retention(
            &mut *lock_writer(self.writer)?,
            self.store_path,
            now,
            retention_hours,
            self.degraded,
        );
        match purge_result {
            Ok(_) => {
                self.active_retention_hours = retention_hours;
                self.pending_retention_hours = None;
                self.degraded.remove("retention");
                self.publish_heartbeat()?;
                Ok(true)
            }
            Err(error) => {
                self.degraded
                    .insert("retention".to_owned(), error.to_string());
                Ok(false)
            }
        }
    }

    pub(super) fn purge_active_retention(
        &mut self,
        now: OffsetDateTime,
    ) -> Result<(), DaemonError> {
        let purge_result = apply_retention(
            &mut *lock_writer(self.writer)?,
            self.store_path,
            now,
            self.active_retention_hours,
            self.degraded,
        );
        match purge_result {
            Ok(_) if self.pending_retention_hours.is_none() => {
                self.degraded.remove("retention");
            }
            Ok(_) => {}
            Err(error) => {
                self.degraded
                    .insert("retention".to_owned(), error.to_string());
            }
        }
        Ok(())
    }
}
