use std::{
    ffi::CString,
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use rustix::{
    fd::OwnedFd,
    fs::{CWD, FlockOperation, Mode, OFlags, SeekFrom, flock, fsync, ftruncate, openat, seek},
    io::Errno,
};
use serde::{Deserialize, Serialize};
use zanei_core::store::DaemonMode;

use super::DaemonError;

const OWNER_READ_ATTEMPTS: usize = 6;
const OWNER_READ_RETRY_INTERVAL: Duration = Duration::from_millis(10);
// Ownership metadata has four short scalar fields; cap reads so a replaced lock file cannot grow
// memory use, while retrying for at most 50 ms covers the acquire-then-write startup window.
const MAX_OWNER_METADATA_BYTES: usize = 1_024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoreOwner {
    pub(crate) pid: u32,
    pub(crate) instance_id: String,
    pub(crate) mode: DaemonMode,
    pub(crate) started_at: String,
}

impl StoreOwner {
    pub(crate) fn new(mode: DaemonMode, started_at: String) -> Self {
        let pid = std::process::id();
        Self {
            pid,
            instance_id: format!("{pid}@{started_at}"),
            mode,
            started_at,
        }
    }
}

pub(crate) struct StoreOwnership {
    _file: OwnedFd,
}

impl StoreOwnership {
    pub(crate) fn acquire(store_path: &Path, owner: StoreOwner) -> Result<Self, DaemonError> {
        let path = lock_path(store_path);
        let file = open_lock_file(&path, true)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(Errno::WOULDBLOCK) => {
                let existing = read_owner_with_retry(&file, &path)?;
                return Err(DaemonError::StoreOwned { pid: existing.pid });
            }
            Err(error) => return Err(ownership_io_error("lock", &path, errno_io(error))),
        }
        write_owner(&file, &path, &owner)?;
        Ok(Self { _file: file })
    }

    pub(crate) fn probe(store_path: &Path) -> Result<Option<StoreOwner>, DaemonError> {
        let path = lock_path(store_path);
        let file = match open_lock_file(&path, false) {
            Ok(file) => file,
            Err(DaemonError::OwnershipFile { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(None),
            Err(Errno::WOULDBLOCK) => read_owner_with_retry(&file, &path).map(Some),
            Err(error) => Err(ownership_io_error("probe", &path, errno_io(error))),
        }
    }
}

fn lock_path(store_path: &Path) -> PathBuf {
    let mut path = store_path.as_os_str().to_os_string();
    path.push(".lock");
    PathBuf::from(path)
}

fn open_lock_file(path: &Path, create: bool) -> Result<OwnedFd, DaemonError> {
    let path_argument = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        ownership_io_error(
            "open",
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"),
        )
    })?;
    let mut flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    if create {
        flags |= OFlags::CREATE;
    }
    openat(
        CWD,
        path_argument.as_c_str(),
        flags,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| ownership_io_error("open", path, errno_io(error)))
}

fn write_owner(file: &OwnedFd, path: &Path, owner: &StoreOwner) -> Result<(), DaemonError> {
    let metadata = OwnerMetadata::from(owner);
    let mut encoded = serde_json::to_vec(&metadata).map_err(|error| {
        invalid_ownership_metadata(path, format!("failed to serialize owner: {error}"))
    })?;
    encoded.push(b'\n');
    ftruncate(file, 0).map_err(|error| ownership_io_error("truncate", path, errno_io(error)))?;
    seek(file, SeekFrom::Start(0))
        .map_err(|error| ownership_io_error("seek", path, errno_io(error)))?;
    write_all(file, &encoded)
        .map_err(|error| ownership_io_error("write", path, errno_io(error)))?;
    fsync(file).map_err(|error| ownership_io_error("sync", path, errno_io(error)))
}

fn read_owner_with_retry(file: &OwnedFd, path: &Path) -> Result<StoreOwner, DaemonError> {
    let mut last_error = "metadata is empty".to_owned();
    for attempt in 0..OWNER_READ_ATTEMPTS {
        let mut encoded = [0_u8; MAX_OWNER_METADATA_BYTES];
        if let Err(error) = seek(file, SeekFrom::Start(0)) {
            last_error = format!("failed to seek: {error}");
        } else {
            match rustix::io::read(file, &mut encoded[..]) {
                Ok(read) => match serde_json::from_slice::<OwnerMetadata>(&encoded[..read]) {
                    Ok(metadata) => return metadata.into_owner(path),
                    Err(error) => last_error = error.to_string(),
                },
                Err(error) => last_error = format!("failed to read: {error}"),
            }
        }
        if attempt + 1 < OWNER_READ_ATTEMPTS {
            thread::sleep(OWNER_READ_RETRY_INTERVAL);
        }
    }
    Err(invalid_ownership_metadata(path, last_error))
}

fn write_all(file: &OwnedFd, mut bytes: &[u8]) -> Result<(), Errno> {
    while !bytes.is_empty() {
        let written = rustix::io::write(file, bytes)?;
        if written == 0 {
            return Err(Errno::IO);
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

fn errno_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

fn ownership_io_error(operation: &'static str, path: &Path, source: std::io::Error) -> DaemonError {
    DaemonError::OwnershipFile {
        operation,
        path: path.to_owned(),
        source,
    }
}

fn invalid_ownership_metadata(path: &Path, reason: String) -> DaemonError {
    DaemonError::InvalidOwnershipMetadata {
        path: path.to_owned(),
        reason,
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OwnerMetadata {
    pid: u32,
    instance_id: String,
    mode: String,
    started_at: String,
}

impl OwnerMetadata {
    fn into_owner(self, path: &Path) -> Result<StoreOwner, DaemonError> {
        if self.pid == 0 {
            return Err(invalid_ownership_metadata(
                path,
                "pid must be greater than zero".to_owned(),
            ));
        }
        let mode = match self.mode.as_str() {
            "foreground" => DaemonMode::Foreground,
            "launchd" => DaemonMode::Launchd,
            _ => {
                return Err(invalid_ownership_metadata(
                    path,
                    format!("unknown recorder mode {:?}", self.mode),
                ));
            }
        };
        let expected = format!("{}@{}", self.pid, self.started_at);
        if self.instance_id != expected {
            return Err(invalid_ownership_metadata(
                path,
                "instance_id does not match pid and started_at".to_owned(),
            ));
        }
        Ok(StoreOwner {
            pid: self.pid,
            instance_id: self.instance_id,
            mode,
            started_at: self.started_at,
        })
    }
}

impl From<&StoreOwner> for OwnerMetadata {
    fn from(owner: &StoreOwner) -> Self {
        Self {
            pid: owner.pid,
            instance_id: owner.instance_id.clone(),
            mode: mode_name(&owner.mode).to_owned(),
            started_at: owner.started_at.clone(),
        }
    }
}

pub(crate) const fn mode_name(mode: &DaemonMode) -> &'static str {
    match mode {
        DaemonMode::Foreground => "foreground",
        DaemonMode::Launchd => "launchd",
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use zanei_core::store::DaemonMode;

    use super::{StoreOwner, StoreOwnership, lock_path};

    #[test]
    fn lock_path_appends_suffix_to_complete_store_name() {
        assert_eq!(
            lock_path(std::path::Path::new("/tmp/store.sqlite")),
            std::path::Path::new("/tmp/store.sqlite.lock")
        );
    }

    #[test]
    fn ownership_probe_reports_owner_only_while_lock_is_held() {
        let directory = TempDir::new().expect("temporary directory");
        let store = directory.path().join("store.sqlite");
        let owner = StoreOwner::new(
            DaemonMode::Foreground,
            "2026-08-17T10:00:00.000Z".to_owned(),
        );
        let ownership = StoreOwnership::acquire(&store, owner.clone()).expect("acquire ownership");

        assert_eq!(
            StoreOwnership::probe(&store).expect("probe held ownership"),
            Some(owner)
        );
        drop(ownership);
        assert_eq!(
            StoreOwnership::probe(&store).expect("probe released ownership"),
            None
        );
    }
}
