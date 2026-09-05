//! Committed positions in one store's append history, independent of event time.

use super::{STORE_SCHEMA_VERSION, StoreError, StoreReader};

/// The identity and highest committed sequence of the active store only.
///
/// Sequence zero denotes a store with no committed appends. Deleted events do
/// not lower this bound. Set-aside stores have independent identities and are
/// deliberately excluded; the timestamp-based reader can still merge them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendHead {
    pub store_identity: String,
    pub sequence: u64,
}

impl StoreReader {
    /// Reads a consistent upper bound for a subsequent append-order scan.
    ///
    /// Legacy stores need a writer migration before they have an append history;
    /// reading them never invents one from timestamps, event IDs, or implicit rowids.
    pub fn append_head(&self) -> Result<AppendHead, StoreError> {
        if self.schema_version < STORE_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchemaVersion(self.schema_version));
        }
        Ok(self.connection.query_row(
            "SELECT identity, COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'events'), 0) \
             FROM store_identity WHERE id = 1",
            [],
            |row| {
                Ok(AppendHead {
                    store_identity: row.get(0)?,
                    sequence: row.get(1)?,
                })
            },
        )?)
    }
}

/// Migration assigns a fresh identity and a baseline sequence while copying the
/// retained legacy rows. Their pre-migration commit order is unknowable: these
/// positions describe the copy, never a continuation of an old cursor.
#[cfg(feature = "write")]
pub(super) fn migrate_events(transaction: &rusqlite::Transaction<'_>) -> Result<(), StoreError> {
    transaction.execute_batch("ALTER TABLE events RENAME TO events_before_append_sequence;")?;
    transaction.execute_batch(super::STORE_TABLES)?;
    transaction.execute_batch(
        "INSERT INTO events(id, ts, mono_ns, source, type, bundle_id, app_name, pid, \
         window_title, window_id, element_json, data_json, redaction_json) \
         SELECT id, ts, mono_ns, source, type, bundle_id, app_name, pid, \
         window_title, window_id, element_json, data_json, redaction_json \
         FROM events_before_append_sequence; \
         DROP TABLE events_before_append_sequence;",
    )?;
    // The old table owned the index names until DROP; recreate those indexes on
    // the new table using the same canonical DDL as a newly created store.
    transaction.execute_batch(super::STORE_TABLES)?;
    Ok(())
}
