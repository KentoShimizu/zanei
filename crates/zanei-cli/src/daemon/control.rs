use std::{ffi::CString, fs, os::unix::ffi::OsStrExt, path::Path, thread, time::Duration};

use rustix::{
    fd::OwnedFd,
    fs::{FileType, FlockOperation, Mode, OFlags, fchmod, flock, fstat, open},
    io::Errno,
};

use super::{DaemonError, StoreOwner, StoreOwnership};

mod launchd;
mod plist;
#[allow(dead_code)]
mod wait;

pub use launchd::{bootout, is_bootstrapped, terminate};
use launchd::{command_succeeded, gui_domain};
pub use plist::{bootstrap, launch_agent_path};
#[cfg(test)]
use plist::{
    build_launch_agent_plist, launch_agent_paths, prepare_launch_agent_logs,
    render_launch_agent_plist,
};
pub use wait::wait_for_launch_agent_removal;
#[cfg(test)]
use wait::{daemon_is_alive_with, owner_has_fresh_heartbeat, start_launch_agent_with};

const LABEL: &str = "dev.zanei.agent";
const PLIST_FILE_NAME: &str = "dev.zanei.agent.plist";
const LAUNCHCTL: &str = "/bin/launchctl";
const ID: &str = "/usr/bin/id";
const KILL: &str = "/bin/kill";
pub(crate) const DAEMON_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const DAEMON_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const START_LOCK_MODE: Mode = Mode::RUSR.union(Mode::WUSR);

pub fn start_launch_agent(
    executable: &Path,
    config_path: &Path,
    store_path: &Path,
) -> Result<bool, DaemonError> {
    let _start_lock = acquire_start_lock()?;
    start_launch_agent_after_preparation_with(
        DAEMON_CONTROL_TIMEOUT,
        || plist::prepare_launch_agent(executable, config_path, store_path),
        bootout,
        is_bootstrapped,
        thread::sleep,
        |prepared| prepared.install_and_bootstrap(),
        || {
            wait::daemon_is_alive_with(store_path, || {
                crate::store_access::load_store_key(
                    crate::store_access::KeyAccess::Existing,
                    crate::store_access::KeyPrompt::Suppressed,
                )
            })
        },
    )
}

fn acquire_start_lock() -> Result<OwnedFd, DaemonError> {
    let plist_path = launch_agent_path()?;
    let lock_path = plist_path.with_file_name(format!(".{PLIST_FILE_NAME}.start.lock"));
    let parent = lock_path.parent().ok_or_else(|| DaemonError::File {
        operation: "resolve parent directory for",
        path: lock_path.clone(),
        source: std::io::Error::other("launch agent start lock has no parent"),
    })?;
    fs::create_dir_all(parent).map_err(|source| DaemonError::File {
        operation: "create directory for",
        path: lock_path.clone(),
        source,
    })?;
    acquire_start_lock_at(&lock_path)
}

fn acquire_start_lock_at(path: &Path) -> Result<OwnedFd, DaemonError> {
    let argument = CString::new(path.as_os_str().as_bytes()).map_err(|_| DaemonError::File {
        operation: "open launch agent start lock at",
        path: path.to_owned(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains a NUL byte"),
    })?;
    let flags = OFlags::CREATE | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let file = open(argument.as_c_str(), flags, START_LOCK_MODE)
        .map_err(|source| control_file_error("open launch agent start lock at", path, source))?;
    let metadata = fstat(&file)
        .map_err(|source| control_file_error("inspect launch agent start lock at", path, source))?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        return Err(DaemonError::File {
            operation: "use as a launch agent start lock",
            path: path.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path is not a regular file; remove it before starting Zanei",
            ),
        });
    }
    fchmod(&file, START_LOCK_MODE).map_err(|source| {
        control_file_error("restrict launch agent start lock at", path, source)
    })?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(Errno::WOULDBLOCK) => Err(DaemonError::File {
            operation: "lock launch agent start at",
            path: path.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "another Zanei start is in progress; wait for it to finish before starting again",
            ),
        }),
        Err(source) => Err(control_file_error(
            "lock launch agent start at",
            path,
            source,
        )),
    }
}

fn control_file_error(operation: &'static str, path: &Path, source: Errno) -> DaemonError {
    DaemonError::File {
        operation,
        path: path.to_owned(),
        source: std::io::Error::from_raw_os_error(source.raw_os_error()),
    }
}

fn start_launch_agent_after_preparation_with<T>(
    timeout: Duration,
    prepare: impl FnOnce() -> Result<T, DaemonError>,
    bootout: impl FnMut() -> Result<(), DaemonError>,
    mut is_registered: impl FnMut() -> Result<bool, DaemonError>,
    sleep: impl FnMut(Duration),
    mut install_and_bootstrap: impl FnMut(&mut T) -> Result<(), DaemonError>,
    is_daemon_alive: impl FnMut() -> Result<bool, DaemonError>,
) -> Result<bool, DaemonError> {
    let mut prepared = prepare()?;
    let registered = is_registered()?;
    wait::start_launch_agent_with(
        registered,
        timeout,
        bootout,
        is_registered,
        sleep,
        || install_and_bootstrap(&mut prepared),
        is_daemon_alive,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
        process::Command,
        time::Duration,
    };

    use tempfile::TempDir;
    use zanei_core::store::{DaemonMode, LockedReason, StoreError, StoreStatus, StoreWriter};

    #[test]
    fn a_locked_key_store_fails_the_wait_while_the_store_is_missing_or_plaintext() {
        let directory = TempDir::new().expect("store directory");
        let store = directory.path().join("store.sqlite");
        let locked = || {
            Err(StoreError::Locked(LockedReason::KeyStoreLocked(
                "unlock the login keychain".to_owned(),
            )))
        };

        assert!(
            matches!(
                super::daemon_is_alive_with(&store, locked),
                Err(crate::daemon::DaemonError::Store(StoreError::Locked(_)))
            ),
            "missing store: the recorder is about to create the key"
        );
        StoreWriter::open(&store).expect("plaintext store");
        assert!(
            matches!(
                super::daemon_is_alive_with(&store, locked),
                Err(crate::daemon::DaemonError::Store(StoreError::Locked(_)))
            ),
            "plaintext store: the recorder is about to create the key"
        );
        assert!(
            !super::daemon_is_alive_with(&store, || Ok(None)).expect("no key yet is not a failure"),
            "nothing owns the store yet"
        );
    }

    use super::{
        DaemonError, StoreOwner, build_launch_agent_plist, launch_agent_paths,
        owner_has_fresh_heartbeat, prepare_launch_agent_logs, render_launch_agent_plist,
        start_launch_agent_after_preparation_with, start_launch_agent_with,
    };

    fn plist_string_value<'a>(plist: &'a str, key: &str) -> &'a str {
        let marker = format!("<key>{key}</key>");
        let after_key = plist.split_once(&marker).expect("plist key").1;
        after_key
            .split_once("<string>")
            .expect("string value")
            .1
            .split_once("</string>")
            .expect("string terminator")
            .0
    }

    #[test]
    fn liveness_does_not_require_a_permission_snapshot() {
        let owner = StoreOwner {
            pid: 42,
            instance_id: "42@2026-08-17T10:00:00.000Z".to_owned(),
            mode: DaemonMode::Launchd,
            started_at: "2026-08-17T10:00:00.000Z".to_owned(),
        };
        let status = StoreStatus {
            running: true,
            instance_id: Some(owner.instance_id.clone()),
            permissions: None,
            ..StoreStatus::default()
        };

        assert!(owner_has_fresh_heartbeat(&owner, &status));
    }

    #[test]
    fn start_lock_rejects_concurrent_starts() {
        let directory = TempDir::new().expect("start lock fixture");
        let path = directory.path().join("start.lock");
        let first = super::acquire_start_lock_at(&path).expect("first start lock");
        let error = super::acquire_start_lock_at(&path).expect_err("second start must fail");
        assert!(matches!(error, DaemonError::File { source, .. }
            if source.kind() == std::io::ErrorKind::WouldBlock));
        drop(first);
        super::acquire_start_lock_at(&path).expect("released start lock");
    }

    #[test]
    fn plist_uses_the_hidden_daemon_contract_and_escapes_paths() {
        let store_path = Path::new("/tmp/<A&B>/store.sqlite");
        let paths = launch_agent_paths(store_path).expect("launch agent paths");
        let plist = render_launch_agent_plist(
            Path::new("/Applications/A&B/zanei"),
            Path::new("/tmp/<config>.toml"),
            None,
            &paths,
        );

        assert!(plist.contains("<string>__daemon</string>"));
        assert!(plist.contains("<string>--config</string>"));
        assert!(plist.contains("<string>--store</string>"));
        assert!(plist.contains("/Applications/A&amp;B/zanei"));
        assert!(plist.contains("/tmp/&lt;config&gt;.toml"));
        assert!(plist.contains("/tmp/&lt;A&amp;B&gt;/store.sqlite"));
        assert_eq!(
            plist_string_value(&plist, "StandardOutPath"),
            "/tmp/&lt;A&amp;B&gt;/store.sqlite.daemon.stdout.log"
        );
        assert_eq!(
            plist_string_value(&plist, "StandardErrorPath"),
            "/tmp/&lt;A&amp;B&gt;/store.sqlite.daemon.stderr.log"
        );
        assert!(!plist.contains("A&B"));
        assert!(!plist.contains("EnvironmentVariables"));
    }

    #[test]
    fn launch_agent_logs_follow_default_and_custom_store_directories() {
        let default_store = crate::paths::Paths::resolve(None, None)
            .expect("default paths")
            .store;
        for store in [
            default_store,
            Path::new("/Volumes/Zanei Data/custom.sqlite").to_owned(),
            Path::new("custom/store.sqlite").to_owned(),
        ] {
            let expected = std::path::absolute(&store).expect("absolute store path");
            let paths = launch_agent_paths(&store).expect("launch agent paths");
            assert_eq!(paths.store_path(), expected);
            assert_eq!(
                paths.standard_out,
                format!("{}.daemon.stdout.log", expected.display())
            );
            assert_eq!(
                paths.standard_error,
                format!("{}.daemon.stderr.log", expected.display())
            );
        }
    }

    #[test]
    fn launch_agent_logs_are_created_owner_only_without_truncating_existing_output() {
        let directory = TempDir::new().expect("log directory fixture");
        let store_path = directory.path().join("nested/store.sqlite");
        let paths = launch_agent_paths(&store_path).expect("launch agent paths");
        let store_directory = store_path.parent().expect("store directory");
        assert!(!store_directory.exists());

        let paths = prepare_launch_agent_logs(&paths).expect("prepare fresh launch agent logs");

        assert!(store_directory.is_dir());
        fs::write(&paths.standard_out, b"existing output").expect("write existing log");
        fs::set_permissions(&paths.standard_out, fs::Permissions::from_mode(0o644))
            .expect("make existing log too permissive");

        prepare_launch_agent_logs(&paths).expect("prepare existing launch agent logs");

        assert_eq!(
            fs::read(&paths.standard_out).expect("read existing log"),
            b"existing output"
        );
        for path in [
            Path::new(&paths.standard_out),
            Path::new(&paths.standard_error),
        ] {
            let mode = fs::metadata(path)
                .expect("log metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{} must be owner-only", path.display());
        }
    }

    #[test]
    fn sticky_writable_ancestor_keeps_an_owner_only_log_parent_safe() {
        let directory = TempDir::new().expect("sticky ancestor fixture");
        let ancestor = directory.path().join("shared");
        let parent = ancestor.join("private");
        fs::create_dir_all(&parent).expect("create log parent");
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o1777))
            .expect("set sticky ancestor");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).expect("set safe parent");
        let paths = launch_agent_paths(&parent.join("store.sqlite")).expect("log paths");
        prepare_launch_agent_logs(&paths).expect("sticky ancestor must be allowed");
    }

    #[test]
    fn plist_carries_the_key_file_override_to_the_recorder() {
        let paths = launch_agent_paths(Path::new("/tmp/store.sqlite")).expect("launch agent paths");
        let plist = render_launch_agent_plist(
            Path::new("/Applications/Zanei.app/Contents/MacOS/zanei"),
            Path::new("/tmp/config.toml"),
            Some(Path::new("/tmp/dev <key>.hex")),
            &paths,
        );

        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("<key>ZANEI_STORE_KEY_FILE</key>"));
        assert!(plist.contains("<string>/tmp/dev &lt;key&gt;.hex</string>"));
    }

    #[test]
    fn installed_plist_uses_the_canonical_bundle_executable_and_current_label() {
        let directory = TempDir::new().expect("launch agent fixture");
        let executable = directory.path().join("Zanei.app/Contents/MacOS/zanei");
        fs::create_dir_all(executable.parent().expect("executable parent"))
            .expect("create app bundle fixture");
        fs::write(&executable, b"fixture").expect("write executable fixture");
        let canonical_executable =
            fs::canonicalize(&executable).expect("canonical executable fixture");
        let symlink_path = directory.path().join("bin/zanei");
        fs::create_dir_all(symlink_path.parent().expect("symlink parent"))
            .expect("create symlink directory");
        symlink(&executable, &symlink_path).expect("create executable symlink");

        let plist = build_launch_agent_plist(
            &symlink_path,
            Path::new("/tmp/config.toml"),
            &launch_agent_paths(Path::new("/tmp/store.sqlite")).expect("launch agent paths"),
        )
        .expect("build launch agent plist");

        assert!(plist.contains("<string>dev.zanei.agent</string>"));
        assert!(plist.contains(&canonical_executable.to_string_lossy().into_owned()));
        assert!(!plist.contains(&symlink_path.to_string_lossy().into_owned()));
    }

    #[test]
    fn generated_launch_agent_plist_passes_plutil_lint() {
        let directory = TempDir::new().expect("plist fixture");
        let executable = directory.path().join("zanei");
        fs::write(&executable, b"fixture").expect("write executable fixture");
        let store_path = directory.path().join("<A&B>/store.sqlite");
        let paths = launch_agent_paths(&store_path).expect("launch agent paths");
        let plist =
            build_launch_agent_plist(&executable, &directory.path().join("<config>.toml"), &paths)
                .expect("build launch agent plist");
        let plist_path = directory.path().join("dev.zanei.agent.plist");
        fs::write(&plist_path, plist).expect("write launch agent plist");

        let output = Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&plist_path)
            .output()
            .expect("run plutil");

        assert!(
            output.status.success(),
            "plutil failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn preparation_completes_before_a_registered_agent_is_replaced() {
        let events = RefCell::new(Vec::new());
        let registered = RefCell::new(VecDeque::from([true, true, false]));

        let restarted = start_launch_agent_after_preparation_with(
            Duration::from_secs(1),
            || {
                events.borrow_mut().push("prepare");
                Ok(())
            },
            || {
                events.borrow_mut().push("bootout");
                Ok(())
            },
            || {
                events.borrow_mut().push("print");
                Ok(registered
                    .borrow_mut()
                    .pop_front()
                    .expect("registered state"))
            },
            |_| {},
            |_| {
                events.borrow_mut().push("bootstrap");
                Ok(())
            },
            || {
                events.borrow_mut().push("alive");
                Ok(true)
            },
        )
        .expect("restart launch agent");

        assert!(restarted);
        assert_eq!(
            events.into_inner().join(","),
            "prepare,print,bootout,print,print,bootstrap,alive"
        );

        let error = start_launch_agent_after_preparation_with(
            Duration::ZERO,
            || {
                Err::<(), _>(DaemonError::Store(StoreError::Locked(
                    LockedReason::KeyMissing,
                )))
            },
            || panic!("preparation failure must not boot out the registered agent"),
            || panic!("registration must not be inspected after preparation fails"),
            |_| {},
            |_| panic!("failed preparation must not be installed"),
            || panic!("failed preparation must not reach readiness checks"),
        )
        .expect_err("preparation failure must be returned");
        assert!(matches!(error, DaemonError::Store(StoreError::Locked(_))));
    }

    #[test]
    fn registered_agent_timeout_does_not_attempt_bootstrap() {
        let bootstrap_called = Cell::new(false);

        let error = start_launch_agent_with(
            true,
            Duration::ZERO,
            || Ok(()),
            || Ok(true),
            |_| panic!("timeout must not sleep after the deadline"),
            || {
                bootstrap_called.set(true);
                Ok(())
            },
            || panic!("readiness must not be checked before bootstrap"),
        )
        .expect_err("registered launch agent must time out");

        assert!(error.to_string().contains("`zanei stop`"));
        assert!(error.to_string().contains("`zanei start`"));
        assert!(matches!(
            error,
            DaemonError::LaunchAgentStillLoaded { timeout_seconds: 0 }
        ));
        assert!(!bootstrap_called.get());
    }

    #[test]
    fn daemon_start_succeeds_after_repeated_not_ready_probes() {
        let events = RefCell::new(Vec::new());
        let readiness = RefCell::new(VecDeque::from([false, false, true]));
        let sleeps = RefCell::new(Vec::new());

        let restarted = start_launch_agent_with(
            false,
            Duration::from_secs(1),
            || panic!("unregistered launch agent must not be booted out"),
            || panic!("unregistered launch agent must not be polled"),
            |duration| sleeps.borrow_mut().push(duration),
            || {
                events.borrow_mut().push("bootstrap");
                Ok(())
            },
            || {
                events.borrow_mut().push("probe");
                Ok(readiness.borrow_mut().pop_front().expect("readiness state"))
            },
        )
        .expect("start unregistered launch agent");

        assert!(!restarted);
        assert_eq!(
            events.into_inner(),
            ["bootstrap", "probe", "probe", "probe"]
        );
        assert_eq!(
            sleeps.into_inner(),
            [Duration::from_millis(100), Duration::from_millis(100)]
        );
    }

    #[test]
    fn daemon_start_timeout_keeps_the_bootstrapped_agent_registered() {
        let bootstrap_called = Cell::new(false);
        let bootout_called = Cell::new(false);

        let error = start_launch_agent_with(
            false,
            Duration::ZERO,
            || {
                bootout_called.set(true);
                Ok(())
            },
            || panic!("unregistered launch agent must not be polled"),
            |_| panic!("timeout must not sleep after the deadline"),
            || {
                bootstrap_called.set(true);
                Ok(())
            },
            || Ok(false),
        )
        .expect_err("daemon readiness must time out");

        let message = error.to_string();
        assert!(message.contains("`zanei status`"));
        assert!(message.contains("last exit code"));
        assert!(message.contains("launchctl print gui/$(id -u)/dev.zanei.agent"));
        assert!(message.contains("`zanei start --foreground`"));
        assert!(matches!(
            &error,
            DaemonError::DaemonDidNotStart { timeout_seconds: 0 }
        ));
        assert_eq!(crate::error::CliError::from(error).exit_code(), 1);
        assert!(bootstrap_called.get());
        assert!(!bootout_called.get());
    }

    #[test]
    fn locked_store_aborts_the_startup_wait_instead_of_timing_out() {
        let mut sleeps = 0;
        let mut boot_outs = 0;
        let error = super::start_launch_agent_with(
            false,
            Duration::from_secs(10),
            || {
                boot_outs += 1;
                Ok(())
            },
            || Ok(false),
            |_| sleeps += 1,
            || Ok(()),
            || {
                Err(crate::daemon::DaemonError::Store(StoreError::Locked(
                    LockedReason::KeyMissing,
                )))
            },
        )
        .expect_err("a locked store is not a not-ready observation");

        assert!(matches!(
            error,
            crate::daemon::DaemonError::Store(StoreError::Locked(LockedReason::KeyMissing))
        ));
        assert_eq!(
            sleeps, 0,
            "the wait must stop at the first locked observation"
        );
        assert_eq!(
            boot_outs, 1,
            "nothing is left registered for launchd to relaunch"
        );
    }
}
