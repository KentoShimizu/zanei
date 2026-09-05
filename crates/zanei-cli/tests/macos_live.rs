#![cfg(target_os = "macos")]

use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const FINDER_BUNDLE_ID: &str = "com.apple.finder";
const TEXT_EDIT_BUNDLE_ID: &str = "com.apple.TextEdit";

#[test]
#[ignore = "requires an interactive macOS session; no TCC permissions"]
fn record_stream_emits_app_activate_without_tcc() {
    let directory = TempDir::new().expect("record fixture");
    let config = directory.path().join("config.toml");
    let store = directory.path().join("must-not-exist.sqlite");
    fs::write(
        &config,
        "[capture]\nsources = [\"app\"]\ntext_content = false\n",
    )
    .expect("record config");
    let mut process = RecordProcess::start(&config, &store);

    let initial = wait_for_activation(&process.events, None);
    let initial_bundle_id = initial["app"]["bundle_id"].as_str();
    let (app_name, target_bundle_id) = if initial_bundle_id == Some(FINDER_BUNDLE_ID) {
        ("TextEdit", TEXT_EDIT_BUNDLE_ID)
    } else {
        ("Finder", FINDER_BUNDLE_ID)
    };

    let status = Command::new("/usr/bin/open")
        .args(["-a", app_name])
        .status()
        .expect("run open -a");
    assert!(status.success(), "open -a {app_name} failed: {status}");

    let activated = wait_for_activation(&process.events, Some(target_bundle_id));
    assert_eq!(activated["type"], "app.activate");
    assert_eq!(activated["app"]["bundle_id"], target_bundle_id);

    process.stop();
    assert!(!store.exists());
}

#[test]
#[ignore = "touches the login Keychain under a throwaway service name; run by hand"]
fn keychain_store_key_round_trips_and_deletes() {
    use zanei_core::store::{KeyStore, KeyStoreError, KeyStoreInteraction, StoreKey};
    use zanei_macos::store_key::KeychainStoreKey;

    let item = KeychainStoreKey::with_service(
        format!("dev.zanei.store.test-{}", std::process::id()),
        "Zanei test store key",
    );
    assert!(
        item.load(KeyStoreInteraction::NoPrompt)
            .expect("load absent item")
            .is_none()
    );
    let key = StoreKey::generate().expect("generate key");
    item.store(&key).expect("store key");
    let loaded = item
        .load(KeyStoreInteraction::NoPrompt)
        .expect("load stored key")
        .expect("key present");
    assert_eq!(loaded.to_hex().as_str(), key.to_hex().as_str());
    let duplicate = item.store(&key).expect_err("second store is a duplicate");
    assert_eq!(duplicate, KeyStoreError::AlreadyExists);
    assert!(item.delete().expect("delete key"));
    assert!(!item.delete().expect("delete absent key"));
    assert!(
        item.load(KeyStoreInteraction::NoPrompt)
            .expect("load deleted item")
            .is_none()
    );
}

fn wait_for_activation(
    receiver: &Receiver<Result<Value, String>>,
    bundle_id: Option<&str>,
) -> Value {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for app.activate");
        let event = receiver
            .recv_timeout(remaining)
            .unwrap_or_else(|error| {
                panic!("failed waiting for app.activate {bundle_id:?}: {error}")
            })
            .unwrap_or_else(|error| panic!("invalid record event: {error}"));
        if event["type"] == "app.activate"
            && bundle_id.is_none_or(|expected| event["app"]["bundle_id"] == expected)
        {
            return event;
        }
    }
}

struct RecordProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<Result<Value, String>>,
    reader: Option<JoinHandle<()>>,
}

impl RecordProcess {
    fn start(config: &std::path::Path, store: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zanei"))
            .arg("--config")
            .arg(config)
            .arg("--store")
            .arg(store)
            .args(["record", "--stream"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start zanei record --stream");
        let stdin = child.stdin.take().expect("record stdin");
        let stdout = child.stdout.take().expect("record stdout");
        let (sender, events) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let parsed = line
                    .map_err(|error| format!("failed to read record output: {error}"))
                    .and_then(|line| {
                        serde_json::from_str(&line)
                            .map_err(|error| format!("invalid NDJSON event {line:?}: {error}"))
                    });
                if sender.send(parsed).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            events,
            reader: Some(reader),
        }
    }

    fn stop(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    assert!(status.success(), "zanei record failed: {status}");
                    break;
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(PROCESS_POLL_INTERVAL),
                Ok(None) | Err(_) => {
                    let _ = self.child.kill();
                    let status = self.child.wait().expect("wait for zanei record");
                    panic!("zanei record did not stop after stdin EOF: {status}");
                }
            }
        }
        if let Some(reader) = self.reader.take() {
            reader.join().expect("join record output reader");
        }
    }
}

impl Drop for RecordProcess {
    fn drop(&mut self) {
        if self.stdin.is_some() {
            self.stdin.take();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
