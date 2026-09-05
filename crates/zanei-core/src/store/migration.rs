use rusqlite::Transaction;

use super::{
    CAPABILITIES_STORE_SCHEMA_VERSION, COLLECTOR_FAILURES_STORE_SCHEMA_VERSION,
    CONTENT_SNAPSHOT_STORE_SCHEMA_VERSION, DAEMON_IDENTITY_STORE_SCHEMA_VERSION,
    LEGACY_STORE_SCHEMA_VERSION, PERMISSIONS_SNAPSHOT_STORE_SCHEMA_VERSION,
    RETENTION_STORE_SCHEMA_VERSION, STORE_SCHEMA_VERSION, StoreError,
};

pub(super) fn migrate_schema(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    let mut version =
        transaction.query_row("SELECT schema_version FROM meta", [], |row| row.get(0))?;
    if version == STORE_SCHEMA_VERSION {
        return Ok(());
    }
    if !(LEGACY_STORE_SCHEMA_VERSION..=CAPABILITIES_STORE_SCHEMA_VERSION).contains(&version) {
        return Err(StoreError::UnsupportedSchemaVersion(version));
    }
    while version < STORE_SCHEMA_VERSION {
        let next = match version {
            LEGACY_STORE_SCHEMA_VERSION => {
                transaction.execute_batch(
                    "ALTER TABLE daemon_state ADD COLUMN instance_id TEXT; \
                     ALTER TABLE daemon_state ADD COLUMN mode TEXT;",
                )?;
                DAEMON_IDENTITY_STORE_SCHEMA_VERSION
            }
            DAEMON_IDENTITY_STORE_SCHEMA_VERSION => {
                transaction.execute_batch(
                    "ALTER TABLE daemon_state ADD COLUMN retention_hours INTEGER \
                     CHECK (retention_hours > 0);",
                )?;
                RETENTION_STORE_SCHEMA_VERSION
            }
            RETENTION_STORE_SCHEMA_VERSION => {
                transaction.execute_batch(
                    "ALTER TABLE daemon_state ADD COLUMN collector_failures_json TEXT NOT NULL \
                     DEFAULT '{}';",
                )?;
                COLLECTOR_FAILURES_STORE_SCHEMA_VERSION
            }
            COLLECTOR_FAILURES_STORE_SCHEMA_VERSION => {
                transaction.execute_batch(
                    "ALTER TABLE daemon_state ADD COLUMN last_known_permissions_json TEXT;",
                )?;
                PERMISSIONS_SNAPSHOT_STORE_SCHEMA_VERSION
            }
            PERMISSIONS_SNAPSHOT_STORE_SCHEMA_VERSION => CONTENT_SNAPSHOT_STORE_SCHEMA_VERSION,
            CONTENT_SNAPSHOT_STORE_SCHEMA_VERSION => {
                transaction.execute_batch(
                    "ALTER TABLE daemon_state RENAME COLUMN last_known_permissions_json \
                         TO last_known_capabilities_json; \
                     UPDATE daemon_state SET last_known_capabilities_json = NULL WHERE id = 1; \
                     DROP TABLE IF EXISTS daemon_permissions; \
                     DELETE FROM daemon_capabilities;",
                )?;
                CAPABILITIES_STORE_SCHEMA_VERSION
            }
            CAPABILITIES_STORE_SCHEMA_VERSION => {
                super::append_sequence::migrate_events(transaction)?;
                STORE_SCHEMA_VERSION
            }
            _ => unreachable!("schema version range was validated before migration"),
        };
        transaction.execute("UPDATE meta SET schema_version = ?1", [next])?;
        version = next;
    }
    Ok(())
}
