use std::{
    fs,
    io::Write,
    ops::{Deref, DerefMut},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;
use zanei_core::store::{StoreKey, StoreReader};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const PARENT_HELPER: &str = "ZANEI_FOREGROUND_PARENT_HELPER";
const PARENT_HELPER_CONFIG: &str = "ZANEI_FOREGROUND_PARENT_CONFIG";
const PARENT_HELPER_STORE: &str = "ZANEI_FOREGROUND_PARENT_STORE";
const PARENT_HELPER_KEY: &str = "ZANEI_FOREGROUND_PARENT_KEY";
const NODE_SCRIPT: &str = r#"
const { spawn } = require("node:child_process");
const child = spawn(
  process.env.ZANEI_NODE_BIN,
  ["--config", process.env.ZANEI_NODE_CONFIG, "--store", process.env.ZANEI_NODE_STORE,
   "start", "--foreground", "--exit-on-stdin-eof"],
  { env: { ...process.env, ZANEI_STORE_KEY_FILE: process.env.ZANEI_NODE_KEY },
    stdio: ["pipe", "ignore", "ignore"] }
);
process.stdin.once("data", () => child.stdin.end());
child.on("exit", (code, signal) => process.exit(code ?? (signal ? 1 : 0)));
"#;

/// A foreground child must not outlive a test if an assertion fails.
struct ChildGuard(Child);

impl Deref for ChildGuard {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn parent_managed_foreground_stops_after_stdin_eof_and_clears_heartbeat() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn(&["start", "--foreground", "--exit-on-stdin-eof"]);
    wait_for_running(&mut child, &fixture.store, &fixture.key);

    drop(child.stdin.take());
    let status = wait_for_exit(&mut child);
    assert!(status.success(), "foreground shutdown failed: {status}");
    assert_eq!(read_status(&fixture.store, &fixture.key).pid, None);
}

#[test]
fn explicit_parent_eof_treats_dev_null_as_immediate_eof() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn_with_stdin(
        &["start", "--foreground", "--exit-on-stdin-eof"],
        Stdio::null(),
    );
    let status = wait_for_exit(&mut child);
    assert!(status.success(), "explicit EOF shutdown failed: {status}");
    assert_eq!(read_status(&fixture.store, &fixture.key).pid, None);
}

#[test]
fn explicit_parent_eof_requires_foreground() {
    let fixture = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_zanei"))
        .env("ZANEI_STORE_KEY_FILE", &fixture.key_file)
        .arg("--config")
        .arg(&fixture.config)
        .arg("--store")
        .arg(&fixture.store)
        .args(["start", "--exit-on-stdin-eof"])
        .output()
        .expect("run invalid EOF mode");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--foreground"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn normal_foreground_ignores_stdin_eof_until_explicit_stop() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn(&["start", "--foreground"]);
    wait_for_running(&mut child, &fixture.store, &fixture.key);

    drop(child.stdin.take());
    thread::sleep(POLL_INTERVAL * 5);
    assert!(
        child.try_wait().expect("poll normal foreground").is_none(),
        "normal foreground must not watch stdin"
    );

    let signal = Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(signal.success(), "failed to send SIGTERM: {signal}");
    assert!(wait_for_exit(&mut child).success());
}

#[test]
fn killed_parent_closes_its_pipe_and_child_exits_cleanly() {
    let fixture = Fixture::new();
    let parent = Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg("parent_helper")
        .arg("--nocapture")
        .env(PARENT_HELPER, "1")
        .env(PARENT_HELPER_CONFIG, &fixture.config)
        .env(PARENT_HELPER_STORE, &fixture.store)
        .env(PARENT_HELPER_KEY, &fixture.key_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn parent helper");
    let mut parent = ChildGuard(parent);
    wait_for_running(&mut parent, &fixture.store, &fixture.key);

    let signal = Command::new("/bin/kill")
        .args(["-KILL", &parent.id().to_string()])
        .status()
        .expect("send SIGKILL to parent helper");
    assert!(signal.success(), "failed to send SIGKILL: {signal}");
    wait_for_stopped(&fixture.store, &fixture.key);
}

#[test]
fn parent_helper() {
    if std::env::var_os(PARENT_HELPER).is_none() {
        return;
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_zanei"))
        .env(
            "ZANEI_STORE_KEY_FILE",
            std::env::var_os(PARENT_HELPER_KEY).expect("parent helper key"),
        )
        .arg("--config")
        .arg(std::env::var_os(PARENT_HELPER_CONFIG).expect("parent helper config"))
        .arg("--store")
        .arg(std::env::var_os(PARENT_HELPER_STORE).expect("parent helper store"))
        .args(["start", "--foreground", "--exit-on-stdin-eof"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child from parent helper");
    let _ = child.wait().expect("wait child from parent helper");
}

#[test]
fn node_closes_stdin_and_child_exits_cleanly() {
    let fixture = Fixture::new();
    let node = Command::new("node")
        .arg("-e")
        .arg(NODE_SCRIPT)
        .env("ZANEI_NODE_BIN", env!("CARGO_BIN_EXE_zanei"))
        .env("ZANEI_NODE_CONFIG", &fixture.config)
        .env("ZANEI_NODE_STORE", &fixture.store)
        .env("ZANEI_NODE_KEY", &fixture.key_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn Node parent");
    let mut node = ChildGuard(node);
    wait_for_running(&mut node, &fixture.store, &fixture.key);
    node.stdin
        .as_mut()
        .expect("Node control pipe")
        .write_all(b"stop")
        .expect("request child EOF");
    let status = wait_for_exit(&mut node);
    assert!(status.success(), "Node parent did not reap child: {status}");
    assert_eq!(read_status(&fixture.store, &fixture.key).pid, None);
}

#[test]
fn explicit_parent_eof_rejects_terminal_stdin() {
    let fixture = Fixture::new();
    let output = Command::new("/usr/bin/script")
        .args([
            "-q",
            "/dev/null",
            env!("CARGO_BIN_EXE_zanei"),
            "--config",
            fixture.config.to_str().expect("config path"),
            "--store",
            fixture.store.to_str().expect("store path"),
            "start",
            "--foreground",
            "--exit-on-stdin-eof",
        ])
        .env("ZANEI_STORE_KEY_FILE", &fixture.key_file)
        .output()
        .expect("run terminal fixture");
    assert!(
        !output.status.success(),
        "TTY must reject explicit EOF mode"
    );
    let output = String::from_utf8_lossy(&output.stdout);
    assert!(output.contains("requires non-terminal stdin"), "{output}");
}

#[test]
fn launchd_mode_does_not_treat_stdin_eof_as_a_shutdown_request() {
    let fixture = Fixture::new();
    let mut child = fixture.spawn(&["__daemon"]);
    wait_for_running(&mut child, &fixture.store, &fixture.key);

    drop(child.stdin.take());
    thread::sleep(POLL_INTERVAL * 5);
    assert!(
        child.try_wait().expect("poll launchd daemon").is_none(),
        "standalone daemon must ignore parent-pipe EOF"
    );

    let signal = Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(signal.success(), "failed to send SIGTERM: {signal}");
    assert!(wait_for_exit(&mut child).success());
}

fn wait_for_running(child: &mut ChildGuard, store: &Path, key: &StoreKey) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if store.exists() {
            if let Ok(status) =
                StoreReader::open_with_key(store, Some(key)).and_then(|reader| reader.status())
            {
                if status.pid.is_some() {
                    return;
                }
            }
        }
        if let Some(status) = child.try_wait().expect("poll foreground startup") {
            panic!("foreground daemon exited before becoming ready: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "foreground daemon did not become ready within {STARTUP_TIMEOUT:?}"
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_exit(child: &mut ChildGuard) -> ExitStatus {
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("poll foreground shutdown") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "foreground daemon did not stop within {SHUTDOWN_TIMEOUT:?}"
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_stopped(store: &Path, key: &StoreKey) {
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    loop {
        if read_status(store, key).pid.is_none() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "child did not stop after parent SIGKILL within {SHUTDOWN_TIMEOUT:?}"
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn read_status(store: &Path, key: &StoreKey) -> zanei_core::store::StoreStatus {
    StoreReader::open_with_key(store, Some(key))
        .and_then(|reader| reader.status())
        .expect("read recorder status")
}

struct Fixture {
    _directory: TempDir,
    config: std::path::PathBuf,
    store: std::path::PathBuf,
    key_file: std::path::PathBuf,
    key: StoreKey,
}

impl Fixture {
    fn new() -> Self {
        let directory = TempDir::new().expect("fixture directory");
        let config = directory.path().join("config.toml");
        let store = directory.path().join("store.sqlite");
        let key_file = directory.path().join("store.key");
        let key = StoreKey::generate().expect("fixture key");
        fs::write(&config, "[capture]\nsources = []\n").expect("fixture config");
        fs::write(&key_file, format!("{}\n", key.to_hex().as_str())).expect("fixture key file");
        Self {
            _directory: directory,
            config,
            store,
            key_file,
            key,
        }
    }

    fn spawn(&self, args: &[&str]) -> ChildGuard {
        self.spawn_with_stdin(args, Stdio::piped())
    }

    fn spawn_with_stdin(&self, args: &[&str], stdin: Stdio) -> ChildGuard {
        let mut process = Command::new(env!("CARGO_BIN_EXE_zanei"));
        process
            .env("ZANEI_STORE_KEY_FILE", &self.key_file)
            .arg("--config")
            .arg(&self.config)
            .arg("--store")
            .arg(&self.store)
            .args(args);
        ChildGuard(
            process
                .stdin(stdin)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn recorder"),
        )
    }
}
