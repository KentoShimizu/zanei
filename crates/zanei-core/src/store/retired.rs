//! Plaintext stores set aside when encryption arrived.
//!
//! The recorder never rewrites a store written before encryption existed.
//! Instead it renames the file to `<store>.plaintext-<timestamp>` and starts
//! a fresh encrypted store next to it. Readers attach every such sibling
//! read-only and return one merged history, so the CLI and the MCP server
//! keep seeing the events recorded before the upgrade. A set-aside file only
//! holds events older than its timestamp, so once that timestamp leaves the
//! retention window the recorder deletes the whole file.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use time::{OffsetDateTime, PrimitiveDateTime, format_description};

use super::{StoreError, sibling};

const RETIRED_INFIX: &str = ".plaintext-";
const TIMESTAMP_FORMAT: &str = "[year][month][day]T[hour][minute][second]Z";
const TIMESTAMP_LENGTH: usize = "20260823T031500Z".len();
const COMPANION_SUFFIXES: [&str; 2] = ["-wal", "-shm"];
const MAX_NAME_ATTEMPTS: u32 = 1_000;

/// A plaintext store the recorder set aside, found next to the live store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetiredPlaintext {
    pub path: PathBuf,
    /// When the recorder set it aside; every event inside is older than this.
    pub set_aside_at: OffsetDateTime,
}

/// The file a store path really names: symlinks are followed, so a store
/// reached through a link keeps its set-aside siblings next to the real file
/// and the link itself is never renamed. The result is canonical whether or
/// not the store file exists yet, so listings agree before and after a store
/// is created.
pub fn resolve_store_path(store_path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(store_path) {
        return real;
    }
    // The file may not exist yet (or the link may dangle); the directory does.
    let parent = store_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    match (fs::canonicalize(parent), store_path.file_name()) {
        (Ok(real_parent), Some(name)) => real_parent.join(name),
        _ => store_path.to_path_buf(),
    }
}

/// Lists the set-aside plaintext stores next to `store_path`, oldest first.
/// Companion `-wal` / `-shm` files are not stores and are left out.
pub fn retired_plaintext_stores(store_path: &Path) -> Result<Vec<RetiredPlaintext>, StoreError> {
    let store_path = resolve_store_path(store_path);
    let store_path = store_path.as_path();
    let Some(store_name) = store_path.file_name().and_then(OsStr::to_str) else {
        return Ok(Vec::new());
    };
    let parent = match store_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(StoreError::io("list the store directory", error)),
    };
    let mut retired = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| StoreError::io("list the store directory", error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(set_aside_at) = parse_retired_name(store_name, name) {
            retired.push(RetiredPlaintext {
                path: entry.path(),
                set_aside_at,
            });
        }
    }
    retired.sort_by(|a, b| {
        a.set_aside_at
            .cmp(&b.set_aside_at)
            .then(a.path.cmp(&b.path))
    });
    Ok(retired)
}

fn parse_retired_name(store_name: &str, candidate: &str) -> Option<OffsetDateTime> {
    let rest = candidate
        .strip_prefix(store_name)?
        .strip_prefix(RETIRED_INFIX)?;
    if COMPANION_SUFFIXES
        .iter()
        .any(|suffix| rest.ends_with(suffix))
    {
        return None;
    }
    if rest.len() < TIMESTAMP_LENGTH {
        return None;
    }
    let (timestamp, tail) = rest.split_at(TIMESTAMP_LENGTH);
    // Only the recorder's own collision suffix (`-1`, `-2`, …) is accepted. Any
    // other tail is a file the user made (`-backup`) and must never be touched.
    let generated_suffix = match tail.strip_prefix('-') {
        None => tail.is_empty(),
        Some(digits) => !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()),
    };
    if !generated_suffix {
        return None;
    }
    parse_timestamp(timestamp)
}

fn parse_timestamp(text: &str) -> Option<OffsetDateTime> {
    let format = format_description::parse_borrowed::<2>(TIMESTAMP_FORMAT).ok()?;
    PrimitiveDateTime::parse(text, format.as_slice())
        .ok()
        .map(PrimitiveDateTime::assume_utc)
}

fn format_timestamp(at: OffsetDateTime) -> Result<String, StoreError> {
    let format = format_description::parse_borrowed::<2>(TIMESTAMP_FORMAT)
        .map_err(|error| StoreError::KeyGeneration(format!("timestamp format: {error}")))?;
    at.format(format.as_slice())
        .map_err(|error| StoreError::KeyGeneration(format!("timestamp format: {error}")))
}

/// Deletes `retired` and its companion files.
pub fn remove_retired(retired: &RetiredPlaintext) -> Result<(), StoreError> {
    for suffix in COMPANION_SUFFIXES {
        super::remove_if_exists(&sibling(&retired.path, suffix))?;
    }
    super::remove_if_exists(&retired.path)
}

#[cfg(feature = "write")]
pub use write::{purge_retired_plaintext, set_aside_plaintext};

#[cfg(feature = "write")]
mod write {
    use std::fs;
    use std::path::Path;
    use std::time::Duration as StdDuration;

    use rusqlite::Connection;
    use time::OffsetDateTime;

    use super::resolve_store_path;
    use super::{
        COMPANION_SUFFIXES, MAX_NAME_ATTEMPTS, RETIRED_INFIX, RetiredPlaintext, format_timestamp,
        parse_timestamp, remove_retired, retired_plaintext_stores,
    };
    use crate::store::{
        StoreError, StoreFormat, StoreWriter, retention_boundary, retention_cutoff, sibling,
        store_uri,
    };

    /// Renames the plaintext store at `store_path` (and its WAL companions) to
    /// `<store>.plaintext-<timestamp>` so a fresh encrypted store can take its
    /// place. The file's contents are not touched. Returns `None` when the store
    /// is missing or already encrypted.
    pub fn set_aside_plaintext(
        store_path: &Path,
        now: OffsetDateTime,
    ) -> Result<Option<RetiredPlaintext>, StoreError> {
        if StoreFormat::probe(store_path)? != StoreFormat::Plaintext {
            return Ok(None);
        }
        // Work on the real file: a symlinked store keeps its link, which then
        // points at the fresh encrypted store created in the target's place.
        let store_path = resolve_store_path(store_path);
        let store_path = store_path.as_path();
        // Hold the plaintext store exclusively from the checkpoint through the
        // renames: no other process can commit to it in between, so nothing
        // written by a concurrent `pause` / `purge` can end up in a journal
        // named after the live store. The lock is released before the new store
        // is created, since readers must be able to attach the set-aside file.
        let guard = Connection::open(store_uri(store_path)?)?;
        guard.busy_timeout(StdDuration::from_millis(5_000))?;
        guard.execute_batch("PRAGMA locking_mode = EXCLUSIVE; PRAGMA wal_checkpoint(TRUNCATE);")?;
        let stamp = format_timestamp(now)?;
        let set_aside_at = parse_timestamp(&stamp).ok_or(StoreError::NumericOverflow("time"))?;
        let mut target = None;
        for attempt in 0..MAX_NAME_ATTEMPTS {
            let mut name = store_path.as_os_str().to_os_string();
            name.push(RETIRED_INFIX);
            name.push(&stamp);
            if attempt > 0 {
                name.push(format!("-{attempt}"));
            }
            let candidate = std::path::PathBuf::from(name);
            if !candidate.exists() {
                target = Some(candidate);
                break;
            }
        }
        let target = target.ok_or_else(|| {
            StoreError::io(
                "choose a name for the set-aside store",
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "too many set-aside stores with the same timestamp",
                ),
            )
        })?;
        fs::rename(store_path, &target)
            .map_err(|error| StoreError::io("set the plaintext store aside", error))?;
        for suffix in COMPANION_SUFFIXES {
            let companion = sibling(store_path, suffix);
            if companion.exists() {
                fs::rename(&companion, sibling(&target, suffix)).map_err(|error| {
                    StoreError::io("set the plaintext store's journal aside", error)
                })?;
            }
        }
        drop(guard);
        // Nothing may be left under the live store's name for the encrypted
        // store to mistake for its own journal.
        for suffix in COMPANION_SUFFIXES {
            crate::store::remove_if_exists(&sibling(store_path, suffix))?;
        }
        Ok(Some(RetiredPlaintext {
            path: target,
            set_aside_at,
        }))
    }

    /// Applies retention to set-aside stores: a store whose timestamp has left
    /// the window is deleted outright (every event inside is older than that),
    /// and the events that have expired inside a newer one are purged, so a
    /// store that sat idle before the upgrade does not keep expired rows
    /// readable in plaintext. Returns the files that were removed.
    pub fn purge_retired_plaintext(
        store_path: &Path,
        now: OffsetDateTime,
        retention_hours: u64,
    ) -> Result<Vec<RetiredPlaintext>, StoreError> {
        let boundary = retention_boundary(now, retention_hours)?;
        let cutoff = retention_cutoff(now, retention_hours)?;
        let mut removed = Vec::new();
        for retired in retired_plaintext_stores(store_path)? {
            if retired.set_aside_at < boundary {
                remove_retired(&retired)?;
                removed.push(retired);
                continue;
            }
            // Probe before opening: this process holds no connection to the
            // retired file yet, so the lock rule from `StoreFormat::probe` holds.
            if StoreFormat::probe(&retired.path)? == StoreFormat::Plaintext {
                StoreWriter::open_known(&retired.path, StoreFormat::Plaintext, None)?
                    .purge_before(&cutoff)?;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::parse_retired_name;

    #[test]
    fn retired_names_are_recognized_and_companions_ignored() {
        let at = parse_retired_name("store.sqlite", "store.sqlite.plaintext-20260823T031500Z")
            .expect("timestamped name");
        assert_eq!(
            at,
            time::OffsetDateTime::parse(
                "2026-08-23T03:15:00Z",
                &time::format_description::well_known::Rfc3339
            )
            .expect("reference time")
        );
        assert!(
            parse_retired_name("store.sqlite", "store.sqlite.plaintext-20260823T031500Z-2")
                .is_some()
        );
        assert!(
            parse_retired_name(
                "store.sqlite",
                "store.sqlite.plaintext-20260823T031500Z-wal"
            )
            .is_none()
        );
        assert!(
            parse_retired_name(
                "store.sqlite",
                "store.sqlite.plaintext-20260823T031500Z-shm"
            )
            .is_none()
        );
        assert!(parse_retired_name("store.sqlite", "store.sqlite").is_none());
        assert!(
            parse_retired_name("store.sqlite", "other.sqlite.plaintext-20260823T031500Z").is_none()
        );
        assert!(parse_retired_name("store.sqlite", "store.sqlite.plaintext-garbage").is_none());
        assert!(
            parse_retired_name("store.sqlite", "store.sqlite.plaintext-20260823T031500Zx")
                .is_none()
        );
        assert!(
            parse_retired_name(
                "store.sqlite",
                "store.sqlite.plaintext-20260823T031500Z-backup"
            )
            .is_none(),
            "a user's copy must never be mistaken for a generated name"
        );
        assert!(
            parse_retired_name("store.sqlite", "store.sqlite.plaintext-20260823T031500Z-")
                .is_none()
        );
        assert!(
            parse_retired_name("store.sqlite", "store.sqlite.plaintext-20260823T031500Z-1x")
                .is_none()
        );
    }
}
