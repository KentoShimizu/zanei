use std::{
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
    process::Command,
};

use rustix::{
    fd::OwnedFd,
    fs::{FileType, FlockOperation, Mode, OFlags, fchmod, flock, fstat, open},
    io::Errno,
};

use super::DaemonError;

const ACL_INSPECTOR: &str = "/bin/ls";
const GROUP_OR_WORLD_WRITE_MODE: u32 = 0o022;
const STICKY_MODE: u32 = 0o1000;
const ROOT_USER_ID: u32 = 0;
const START_LOCK_MODE: Mode = Mode::RUSR.union(Mode::WUSR);

pub(super) fn open_start_lock(path: &Path, user_id: u32) -> Result<OwnedFd, DaemonError> {
    let argument = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        file_error(
            "open launch agent start lock at",
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains a NUL byte"),
        )
    })?;
    let flags = OFlags::CREATE | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let file = open(argument.as_c_str(), flags, START_LOCK_MODE)
        .map_err(|source| errno_error("open launch agent start lock at", path, source))?;
    let metadata = fstat(&file)
        .map_err(|source| errno_error("inspect launch agent start lock at", path, source))?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        return Err(invalid_directory(
            "use as a launch agent start lock",
            path,
            std::io::ErrorKind::InvalidInput,
            "path is not a regular file; remove it before starting Zanei",
        ));
    }
    if metadata.st_uid != user_id {
        return Err(invalid_directory(
            "use as a launch agent start lock",
            path,
            std::io::ErrorKind::PermissionDenied,
            &format!(
                "lock file is owned by uid {}, not current uid {user_id}; remove it before starting Zanei",
                metadata.st_uid
            ),
        ));
    }
    fchmod(&file, START_LOCK_MODE)
        .map_err(|source| errno_error("restrict launch agent start lock at", path, source))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(Errno::WOULDBLOCK) => Err(invalid_directory(
            "lock launch agent start at",
            path,
            std::io::ErrorKind::WouldBlock,
            "another Zanei start is in progress; wait for it to finish before starting again",
        )),
        Err(source) => Err(errno_error("lock launch agent start at", path, source)),
    }
}

fn errno_error(operation: &'static str, path: &Path, source: Errno) -> DaemonError {
    file_error(
        operation,
        path,
        std::io::Error::from_raw_os_error(source.raw_os_error()),
    )
}

pub(super) fn validate_owner_only_directory(
    path: &Path,
    user_id: u32,
    operation: &'static str,
) -> Result<(), DaemonError> {
    let path = std::path::absolute(path).map_err(|source| file_error(operation, path, source))?;
    for directory in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = fs::symlink_metadata(directory)
            .map_err(|source| file_error(operation, directory, source))?;
        if !metadata.file_type().is_dir() {
            return Err(invalid_directory(
                operation,
                directory,
                std::io::ErrorKind::InvalidInput,
                "path component is not a directory; choose an owner-only directory",
            ));
        }
        #[cfg(not(test))]
        reject_extended_acl(directory, operation)?;
        if let Some(reason) = unsafe_reason(
            metadata.uid(),
            metadata.mode() & 0o7777,
            user_id,
            directory == path,
        ) {
            return Err(invalid_directory(
                operation,
                directory,
                std::io::ErrorKind::PermissionDenied,
                &reason,
            ));
        }
        // Test ordering avoids fork-inherited flock races; production reports ACLs first.
        #[cfg(test)]
        reject_extended_acl(directory, operation)?;
    }
    Ok(())
}

fn unsafe_reason(owner: u32, mode: u32, user_id: u32, is_final: bool) -> Option<String> {
    if is_final && owner != user_id {
        Some(format!(
            "directory owner uid {owner} is not current uid {user_id}; use an owner-only directory"
        ))
    } else if !is_final && owner != ROOT_USER_ID && owner != user_id {
        Some(format!(
            "ancestor is owned by untrusted uid {owner}; move it below a directory owned by root or current uid {user_id}"
        ))
    } else if is_final && mode & GROUP_OR_WORLD_WRITE_MODE != 0 {
        Some(format!(
            "directory mode {mode:#06o} is group/world-writable; run `chmod go-w` on it or choose an owner-only directory"
        ))
    } else if !is_final && mode & GROUP_OR_WORLD_WRITE_MODE != 0 && mode & STICKY_MODE == 0 {
        Some(format!(
            "ancestor mode {mode:#06o} lets other users replace path components because the sticky bit is not set; run `chmod go-w` on it or choose an owner-only directory"
        ))
    } else {
        None
    }
}

fn reject_extended_acl(path: &Path, operation: &'static str) -> Result<(), DaemonError> {
    let output = Command::new(ACL_INSPECTOR)
        .args(["-ldeb"])
        .arg(path)
        .output()
        .map_err(|source| file_error(operation, path, source))?;
    if !output.status.success() {
        let reason = format!(
            "could not inspect extended ACLs with {ACL_INSPECTOR}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return Err(invalid_directory(
            operation,
            path,
            std::io::ErrorKind::Other,
            &reason,
        ));
    }
    let listings = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .count();
    let invalid = |kind, reason| invalid_directory(operation, path, kind, reason);
    match listings {
        0 => Err(invalid(
            std::io::ErrorKind::InvalidData,
            "could not inspect extended ACLs because /bin/ls returned no listing",
        )),
        1 => Ok(()),
        _ => Err(invalid(
            std::io::ErrorKind::PermissionDenied,
            "an extended ACL entry exists; run `chmod -N <dir>` on this directory or choose an ACL-free owner-only directory",
        )),
    }
}

fn invalid_directory(
    operation: &'static str,
    path: &Path,
    kind: std::io::ErrorKind,
    reason: &str,
) -> DaemonError {
    file_error(operation, path, std::io::Error::new(kind, reason))
}

fn file_error(operation: &'static str, path: &Path, source: std::io::Error) -> DaemonError {
    DaemonError::File {
        operation,
        path: path.to_owned(),
        source,
    }
}
#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::MetadataExt, path::Path, process::Command};

    use super::*;

    const OPERATION: &str = "use as a test directory";
    fn chmod(path: &Path, arguments: &[&str]) {
        let status = Command::new("/bin/chmod")
            .args(arguments)
            .arg(path)
            .status();
        assert!(status.expect("change ACL").success());
    }

    #[test]
    fn rejects_wrong_owner_final_and_ancestor_directories() {
        let directory = tempfile::TempDir::new_in("/private/tmp").expect("directory fixture");
        let error = validate_owner_only_directory(directory.path(), u32::MAX, OPERATION)
            .expect_err("wrong-owner final directory must be rejected");
        assert!(matches!(error, DaemonError::File { path, .. } if path == directory.path()));
        let final_directory = directory.path().join("final");
        fs::create_dir(&final_directory).expect("final directory");
        let error = validate_owner_only_directory(&final_directory, u32::MAX, OPERATION)
            .expect_err("wrong-owner ancestor directory must be rejected");
        assert!(matches!(error, DaemonError::File { path, .. } if path == directory.path()));
    }

    #[test]
    fn rejects_directory_with_extended_acl() {
        let directory = tempfile::TempDir::new_in("/private/tmp").expect("directory fixture");
        let path = directory.path();
        let user_id = fs::metadata(path).expect("directory metadata").uid();
        chmod(path, &["+a", "everyone deny delete"]);
        let result = validate_owner_only_directory(path, user_id, OPERATION);
        chmod(path, &["-N"]);
        let error = result.expect_err("ACL must be rejected");
        assert!(matches!(&error, DaemonError::File { path: actual, .. } if actual == path));
        assert!(error.to_string().contains("extended ACL entry exists"));
    }
}
