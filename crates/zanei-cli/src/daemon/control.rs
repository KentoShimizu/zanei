use std::time::Duration;

use super::{DaemonError, StoreOwner, StoreOwnership};

mod launchd;
mod plist;
mod wait;

pub use launchd::{bootout, is_bootstrapped, terminate};
use launchd::{command_succeeded, gui_domain};
pub use plist::{bootstrap, launch_agent_path};
#[cfg(test)]
use plist::{build_launch_agent_plist, render_launch_agent_plist};
#[cfg(test)]
use wait::{daemon_is_alive_with, owner_has_fresh_heartbeat, start_launch_agent_with};
pub use wait::{start_launch_agent, wait_for_launch_agent_removal};

const LABEL: &str = "dev.zanei.agent";
const PLIST_FILE_NAME: &str = "dev.zanei.agent.plist";
const LAUNCHCTL: &str = "/bin/launchctl";
const ID: &str = "/usr/bin/id";
const KILL: &str = "/bin/kill";
pub(crate) const DAEMON_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const DAEMON_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        fs,
        os::unix::fs::symlink,
        path::Path,
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
        DaemonError, StoreOwner, build_launch_agent_plist, owner_has_fresh_heartbeat,
        render_launch_agent_plist, start_launch_agent_with,
    };

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
    fn plist_uses_the_hidden_daemon_contract_and_escapes_paths() {
        let plist = render_launch_agent_plist(
            Path::new("/Applications/A&B/zanei"),
            Path::new("/tmp/<config>.toml"),
            Path::new("/tmp/store.sqlite"),
            None,
        );

        assert!(plist.contains("<string>__daemon</string>"));
        assert!(plist.contains("<string>--config</string>"));
        assert!(plist.contains("<string>--store</string>"));
        assert!(plist.contains("/Applications/A&amp;B/zanei"));
        assert!(plist.contains("/tmp/&lt;config&gt;.toml"));
        assert!(!plist.contains("A&B"));
        assert!(!plist.contains("EnvironmentVariables"));
    }

    #[test]
    fn plist_carries_the_key_file_override_to_the_recorder() {
        let plist = render_launch_agent_plist(
            Path::new("/Applications/Zanei.app/Contents/MacOS/zanei"),
            Path::new("/tmp/config.toml"),
            Path::new("/tmp/store.sqlite"),
            Some(Path::new("/tmp/dev <key>.hex")),
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
            Path::new("/tmp/store.sqlite"),
        )
        .expect("build launch agent plist");

        assert!(plist.contains("<string>dev.zanei.agent</string>"));
        assert!(plist.contains(&canonical_executable.to_string_lossy().into_owned()));
        assert!(!plist.contains(&symlink_path.to_string_lossy().into_owned()));
    }

    #[test]
    fn registered_agent_is_removed_before_it_is_bootstrapped_again() {
        let events = RefCell::new(Vec::new());
        let registered = RefCell::new(VecDeque::from([true, true, false]));

        let restarted = start_launch_agent_with(
            true,
            Duration::from_secs(1),
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
            || {
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
            events.into_inner(),
            ["bootout", "print", "print", "print", "bootstrap", "alive"]
        );
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
