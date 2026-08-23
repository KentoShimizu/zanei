use rusqlite::Connection;

use super::{
    COLLECTOR_FAILURES_STORE_SCHEMA_VERSION, DAEMON_IDENTITY_STORE_SCHEMA_VERSION,
    LEGACY_STORE_SCHEMA_VERSION, PERMISSIONS_SNAPSHOT_STORE_SCHEMA_VERSION,
    RETENTION_STORE_SCHEMA_VERSION, STORE_SCHEMA_VERSION, StoreError,
};

pub(super) fn migrate_schema(connection: &Connection) -> Result<(), StoreError> {
    let mut version =
        connection.query_row("SELECT schema_version FROM meta", [], |row| row.get(0))?;
    if version == STORE_SCHEMA_VERSION {
        return Ok(());
    }
    if !(LEGACY_STORE_SCHEMA_VERSION..=PERMISSIONS_SNAPSHOT_STORE_SCHEMA_VERSION).contains(&version)
    {
        return Err(StoreError::UnsupportedSchemaVersion(version));
    }
    let transaction = connection.unchecked_transaction()?;
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
                    "ALTER TABLE daemon_state ADD COLUMN last_known_permissions_json TEXT; \
                     UPDATE daemon_state SET last_known_permissions_json = \
                         (SELECT snapshot_json FROM daemon_permissions WHERE id = 1) \
                     WHERE id = 1;",
                )?;
                PERMISSIONS_SNAPSHOT_STORE_SCHEMA_VERSION
            }
            PERMISSIONS_SNAPSHOT_STORE_SCHEMA_VERSION => STORE_SCHEMA_VERSION,
            _ => unreachable!("schema version range was validated before migration"),
        };
        transaction.execute("UPDATE meta SET schema_version = ?1", [next])?;
        version = next;
    }
    transaction.commit()?;
    Ok(())
}
