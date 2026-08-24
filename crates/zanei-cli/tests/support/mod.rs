// Cargo compiles this shared module once per integration-test binary, and each binary uses a
// different subset of the helpers.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use rustix::fs::{FlockOperation, flock};
use serde::Serialize;
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use zanei_core::config::DEFAULT_RETENTION_HOURS;
use zanei_core::normalize::{format_timestamp, normalize};
use zanei_core::schema::{
    App, BrowserMode, BrowserNavigateData, BrowserTransition, ClickButton, ClipboardCopyData,
    ClipboardOrigin, ClipboardPasteData, ContentKind, ContentSnapshotData, ContentSnapshotTrigger,
    Element, EmptyData, Event, EventData, FieldKind, InputKeyData, InputKeyKind, InputScrollData,
    KNOWN_EVENT_TYPES, RawEvent, ScrollDirection, UiClickData, UiFocusData, UiValueData, Window,
    WindowTitleData,
};
use zanei_core::store::{
    DaemonMode, DaemonPermissions, DaemonState, PermissionState, StoreKey, StoreReader, StoreWriter,
};

/// Environment variable the CLI reads the store key from instead of the Keychain.
pub const STORE_KEY_FILE_ENV: &str = "ZANEI_STORE_KEY_FILE";

/// A temporary config, store, and key file. Stores are created encrypted with
/// the key in `key_file`, and every command the fixture spawns reads that key
/// through `ZANEI_STORE_KEY_FILE`, so tests never touch the real Keychain.
pub struct Fixture {
    pub directory: TempDir,
    pub config: PathBuf,
    pub store: PathBuf,
    pub key_file: PathBuf,
}

impl Fixture {
    pub fn populated() -> Self {
        Self::create(StoreContents::Populated)
    }

    pub fn empty() -> Self {
        Self::create(StoreContents::Empty)
    }

    pub fn uninitialized() -> Self {
        Self::create(StoreContents::Uninitialized)
    }

    pub fn command(&self) -> Command {
        let mut command = Command::cargo_bin("zanei").expect("zanei binary");
        command
            .env(STORE_KEY_FILE_ENV, &self.key_file)
            .arg("--config")
            .arg(&self.config)
            .arg("--store")
            .arg(&self.store);
        command
    }

    pub fn process_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_zanei"));
        command
            .env(STORE_KEY_FILE_ENV, &self.key_file)
            .arg("--config")
            .arg(&self.config)
            .arg("--store")
            .arg(&self.store);
        command
    }

    /// The key every store in this fixture is encrypted with.
    pub fn key(&self) -> StoreKey {
        read_key(&self.key_file)
    }

    pub fn open_reader(&self) -> StoreReader {
        StoreReader::open_with_key(&self.store, Some(&self.key())).expect("fixture reader")
    }

    pub fn open_writer(&self) -> StoreWriter {
        StoreWriter::open_with_key(&self.store, Some(&self.key())).expect("fixture writer")
    }

    /// Holds recorder ownership matching the daemon state already stored until the file is dropped.
    ///
    /// The private production ownership type is not reachable from integration tests, so this is
    /// a minimal mirror of its lock-file contract whose behavior the CLI's own probe verifies;
    /// open/Drop differences from production are acceptable inside the trusted TempDir.
    pub fn hold_store_owner(&self) -> File {
        let status = self.open_reader().status().expect("fixture daemon status");
        let owner = FixtureStoreOwner {
            pid: u32::try_from(status.pid.expect("fixture daemon pid"))
                .expect("fixture daemon pid fits u32"),
            instance_id: status.instance_id.expect("fixture daemon instance id"),
            mode: match status.mode.expect("fixture daemon mode") {
                DaemonMode::Foreground => "foreground",
                DaemonMode::Launchd => "launchd",
            },
            started_at: status.started_at.expect("fixture daemon start time"),
        };
        let mut lock_path = self.store.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        let mut lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .expect("fixture store ownership file");
        flock(&lock, FlockOperation::NonBlockingLockExclusive)
            .expect("fixture store ownership lock");
        lock.set_len(0).expect("truncate fixture store owner");
        lock.rewind().expect("rewind fixture store owner");
        serde_json::to_writer(&mut lock, &owner).expect("serialize fixture store owner");
        lock.write_all(b"\n").expect("write fixture store owner");
        lock.sync_all().expect("sync fixture store owner");
        lock
    }

    pub fn set_recorder_permissions(&self, permissions_ok: bool) {
        let status = self.open_reader().status().expect("fixture daemon status");
        StoreWriter::open_with_key(&self.store, Some(&self.key()))
            .and_then(|writer| {
                writer.write_daemon_state(&DaemonState {
                    pid: status.pid,
                    started_at: status.started_at,
                    instance_id: status.instance_id,
                    mode: status.mode,
                    heartbeat_at: Some(format_timestamp(OffsetDateTime::now_utc())),
                    retention_hours: status.retention_hours,
                    paused_until: status.paused_until,
                    events_captured: status.events_captured,
                    events_dropped: status.events_dropped,
                    last_event_ts: status.last_event_ts,
                    degraded: status.degraded,
                    collector_failures: status.collector_failures,
                    permissions: Some(DaemonPermissions {
                        permissions_ok,
                        accessibility: PermissionState::Granted,
                        input_monitoring: PermissionState::Granted,
                        automation: BTreeMap::new(),
                    }),
                })
            })
            .expect("set fixture recorder permissions");
    }

    fn create(contents: StoreContents) -> Self {
        let directory = TempDir::new().expect("fixture directory");
        let config = directory.path().join("config.toml");
        let store = directory.path().join("store.sqlite");
        let key_file = write_key_file(directory.path());
        fs::write(
            &config,
            "[capture]\nsources = [\"app\"]\ntext_content = false\n",
        )
        .expect("fixture config");

        match contents {
            StoreContents::Populated => populate_store(&store, &read_key(&key_file)),
            StoreContents::Empty => {
                StoreWriter::open_with_key(&store, Some(&read_key(&key_file)))
                    .expect("empty fixture store");
            }
            StoreContents::Uninitialized => {}
        }

        Self {
            directory,
            config,
            store,
            key_file,
        }
    }
}

/// Writes a fresh key file into `directory` and returns its path.
pub fn write_key_file(directory: &Path) -> PathBuf {
    let key_file = directory.join("store.key");
    let key = StoreKey::generate().expect("generate fixture key");
    fs::write(&key_file, format!("{}\n", key.to_hex().as_str())).expect("write fixture key");
    key_file
}

pub fn read_key(key_file: &Path) -> StoreKey {
    StoreKey::from_hex(&fs::read_to_string(key_file).expect("read fixture key"))
        .expect("parse fixture key")
}

enum StoreContents {
    Populated,
    Empty,
    Uninitialized,
}

#[derive(Serialize)]
struct FixtureStoreOwner {
    pid: u32,
    instance_id: String,
    mode: &'static str,
    started_at: String,
}

fn populate_store(store: &Path, key: &StoreKey) {
    let now = OffsetDateTime::now_utc();
    let started_at = format_timestamp(now - Duration::minutes(1));
    let events = synthetic_events(now);
    let mut writer = StoreWriter::open_with_key(store, Some(key)).expect("fixture writer");
    writer.append_batch(&events).expect("fixture events");
    writer
        .write_daemon_state(&DaemonState {
            pid: Some(42),
            started_at: Some(started_at.clone()),
            instance_id: Some(format!("42@{started_at}")),
            mode: Some(DaemonMode::Foreground),
            heartbeat_at: Some(format_timestamp(now)),
            retention_hours: Some(DEFAULT_RETENTION_HOURS),
            paused_until: None,
            events_captured: u64::try_from(events.len()).expect("fixture event count fits u64"),
            events_dropped: 2,
            last_event_ts: events.last().map(|event| event.ts.clone()),
            degraded: BTreeMap::new(),
            collector_failures: BTreeMap::from([("eventtap".to_owned(), 1)]),
            permissions: Some(DaemonPermissions {
                permissions_ok: true,
                accessibility: PermissionState::Granted,
                input_monitoring: PermissionState::Granted,
                automation: BTreeMap::new(),
            }),
        })
        .expect("fixture daemon state");
}

fn synthetic_events(now: OffsetDateTime) -> Vec<Event> {
    let payloads = event_payloads();
    let event_types: Vec<_> = payloads.iter().map(|(event_type, _)| *event_type).collect();
    assert_eq!(event_types.as_slice(), KNOWN_EVENT_TYPES.as_slice());

    let event_count = payloads.len();
    payloads
        .into_iter()
        .enumerate()
        .map(|(index, (event_type, data))| {
            let age_seconds = i64::try_from(event_count - index).expect("fixture age fits i64");
            let mono_ns = u64::try_from(index + 1).expect("fixture index fits u64") * 1_000_000_000;
            normalize(
                raw_event(event_type, data),
                now - Duration::seconds(age_seconds),
                mono_ns,
            )
            .expect("normalize fixture event")
            .event
        })
        .collect()
}

fn event_payloads() -> Vec<(&'static str, EventData)> {
    vec![
        ("app.activate", EventData::AppActivate(EmptyData {})),
        ("app.launch", EventData::AppLaunch(EmptyData::default())),
        (
            "app.terminate",
            EventData::AppTerminate(EmptyData::default()),
        ),
        ("window.focus", EventData::WindowFocus(EmptyData::default())),
        (
            "window.title",
            EventData::WindowTitle(WindowTitleData {
                prev_title: Some("Previous Fixture Window".to_owned()),
            }),
        ),
        (
            "ui.focus",
            EventData::UiFocus(UiFocusData {
                field_kind: Some(FieldKind::Text),
            }),
        ),
        (
            "ui.click",
            EventData::UiClick(UiClickData {
                button: ClickButton::Left,
                click_count: 1,
            }),
        ),
        (
            "ui.value",
            EventData::UiValue(UiValueData {
                field_kind: Some(FieldKind::Text),
                value_len: Some(7),
                text: None,
            }),
        ),
        (
            "input.key",
            EventData::InputKey(InputKeyData {
                kind: InputKeyKind::Text,
                modifiers: Vec::new(),
                count: 1,
                combo: None,
                text: None,
                field_kind: Some(FieldKind::Text),
            }),
        ),
        (
            "input.scroll",
            EventData::InputScroll(InputScrollData {
                direction: ScrollDirection::Down,
                amount: 1.0,
                count: 1,
            }),
        ),
        (
            "browser.navigate",
            EventData::BrowserNavigate(BrowserNavigateData {
                url: "https://example.com/fixture".to_owned().into(),
                tab_title: Some("Fixture Page".to_owned()),
                mode: BrowserMode::Normal,
                transition: Some(BrowserTransition::Navigate),
            }),
        ),
        (
            "clipboard.copy",
            EventData::ClipboardCopy(ClipboardCopyData {
                origin: ClipboardOrigin::CopyShortcut,
                content_kind: ContentKind::Text,
                size_bytes: None,
                text: None,
            }),
        ),
        (
            "clipboard.paste",
            EventData::ClipboardPaste(ClipboardPasteData {
                content_kind: ContentKind::Text,
                size_bytes: None,
                text: None,
                field_kind: Some(FieldKind::Text),
            }),
        ),
        (
            "content.snapshot",
            EventData::ContentSnapshot(ContentSnapshotData {
                text: Some("Visible fixture snapshot".to_owned()),
                chars: 24,
                complete: true,
                trigger: ContentSnapshotTrigger::Settle,
            }),
        ),
    ]
}

fn raw_event(event_type: &str, data: EventData) -> RawEvent {
    let has_window = !event_type.starts_with("app.");
    let has_element = event_type.starts_with("ui.");
    let source = if event_type.starts_with("app.") {
        "macos.workspace"
    } else if event_type.starts_with("window.")
        || event_type.starts_with("ui.")
        || event_type.starts_with("content.")
    {
        "macos.ax"
    } else if event_type.starts_with("browser.") {
        "macos.applescript"
    } else {
        "macos.eventtap"
    };

    RawEvent {
        observed_at: None,
        source: source.to_owned(),
        event_type: event_type.to_owned(),
        app: App {
            name: "FixtureApp".to_owned(),
            bundle_id: Some("com.example.FixtureApp".to_owned()),
            pid: Some(42),
        },
        window: has_window.then(|| Window {
            title: Some("Fixture Window".to_owned()),
            id: Some(7),
        }),
        element: has_element.then(|| Element {
            role: Some("AXTextField".to_owned()),
            title: Some("Fixture Field".to_owned()),
            value: None,
        }),
        data,
        capture_context: Default::default(),
    }
}

/// Writes a file next to `store` that looks like a set-aside plaintext store
/// from `hours_ago` — a SQLite header over zeroed pages: classified as
/// plaintext, unusable — and returns its path.
pub fn damaged_set_aside_store(store: &Path, hours_ago: i64) -> PathBuf {
    let at = OffsetDateTime::now_utc() - Duration::hours(hours_ago);
    let mut name = store.as_os_str().to_os_string();
    name.push(format!(
        ".plaintext-{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    ));
    let path = PathBuf::from(name);
    let mut damaged = b"SQLite format 3\0".to_vec();
    damaged.resize(4096, 0);
    fs::write(&path, damaged).expect("damaged set-aside store");
    path
}
