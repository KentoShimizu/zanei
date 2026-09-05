use super::StoreWriter;
use crate::store::{StoreKey, tests::TestDatabase};

#[test]
fn writer_commits_request_full_sync_and_disk_cache_flush() {
    let database = TestDatabase::new("append-durability");
    let key = StoreKey::generate().expect("fixture key");
    let writer = StoreWriter::open_with_key(database.path(), Some(&key)).expect("writer");
    let connection = &writer.connection;
    let journal_mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode");
    let synchronous: i64 = connection
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .expect("commit sync mode");
    let fullfsync: bool = connection
        .query_row("PRAGMA fullfsync", [], |row| row.get(0))
        .expect("full disk-cache flush");
    // This verifies the production connection settings, not a physical power cut.
    assert_eq!(journal_mode, "wal");
    assert_eq!(synchronous, 2); // SQLite FULL
    assert!(fullfsync);
}
