//! `zanei start --paused`: the recorder must come up with capture suspended,
//! with no window between its start and a later `pause` in which it records.

mod support;

use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use zanei_core::normalize::format_timestamp;
use zanei_core::store::{PAUSE_INDEFINITE, StoreReader, StoreStatus};

use support::Fixture;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Longer than the recorder's one-second pause poll, so a recorder that starts
/// capture despite the request has had several chances to record an event.
const PAUSE_OBSERVATION_WINDOW: Duration = Duration::from_millis(2_500);

/// A foreground recorder must not outlive a test that fails an assertion.
struct RecorderGuard(Child);

impl Drop for RecorderGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn foreground_start_paused_records_nothing_until_resume() {
    let fixture = Fixture::uninitialized();
    let mut recorder = spawn_foreground(&fixture, &["start", "--foreground", "--paused"]);
    let running = wait_for_running(&mut recorder, &fixture);

    assert!(running.paused, "the recorder must come up paused");
    assert_eq!(running.paused_until.as_deref(), Some(PAUSE_INDEFINITE));
    thread::sleep(PAUSE_OBSERVATION_WINDOW);
    let observed = fixture.open_reader().status().expect("paused status");
    assert!(
        observed.paused,
        "the pause must hold while the recorder runs"
    );
    assert_eq!(
        observed.events_captured, 0,
        "a recorder started paused must not record"
    );

    let resume = fixture
        .command()
        .arg("resume")
        .output()
        .expect("resume output");
    assert_eq!(resume.status.code(), Some(0));
    let resumed = fixture.open_reader().status().expect("resumed status");
    assert!(!resumed.paused);
    assert!(!resumed.pause_requested);
    assert!(
        recorder.0.try_wait().expect("poll recorder").is_none(),
        "resume must not stop the recorder"
    );
}

#[test]
fn start_paused_never_weakens_a_pause_the_store_already_carries() {
    let fixture = Fixture::empty();
    let timed = format_timestamp(OffsetDateTime::now_utc() + time::Duration::minutes(30));
    fixture
        .open_writer()
        .set_paused_until(Some(&timed))
        .expect("timed pause");

    let mut recorder = spawn_foreground(&fixture, &["start", "--foreground", "--paused"]);
    let running = wait_for_running(&mut recorder, &fixture);

    assert!(running.paused);
    assert_eq!(
        running.paused_until.as_deref(),
        Some(PAUSE_INDEFINITE),
        "the stricter of the two pauses is kept"
    );
}

#[test]
fn start_paused_leaves_a_running_recorder_alone() {
    let fixture = Fixture::uninitialized();
    let mut recorder = spawn_foreground(&fixture, &["start", "--foreground"]);
    let running = wait_for_running(&mut recorder, &fixture);
    assert!(!running.pause_requested);

    let rejected = fixture
        .command()
        .args(["start", "--foreground", "--paused"])
        .output()
        .expect("second start output");

    assert_eq!(rejected.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("owns this store"), "{stderr}");
    assert!(
        !fixture
            .open_reader()
            .status()
            .expect("owner status")
            .pause_requested,
        "a rejected start must not pause the recorder that owns the store"
    );
}

#[test]
fn status_reports_the_stored_pause_while_no_recorder_runs() {
    let fixture = Fixture::empty();
    fixture
        .open_writer()
        .set_paused_until(Some(PAUSE_INDEFINITE))
        .expect("stored pause");

    let output = fixture
        .command()
        .args(["status", "--json"])
        .output()
        .expect("status output");

    assert_eq!(output.status.code(), Some(4));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(report["state"], "stopped");
    assert_eq!(
        report["paused"], false,
        "no recorder is suspending capture right now"
    );
    assert_eq!(
        report["paused_persisted"], true,
        "the store's pause must be visible without starting the recorder"
    );
    let human = fixture.command().arg("status").output().expect("status");
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("STORED PAUSE      true"),
        "{}",
        String::from_utf8_lossy(&human.stdout)
    );
}

fn spawn_foreground(fixture: &Fixture, args: &[&str]) -> RecorderGuard {
    let mut command = fixture.process_command();
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    RecorderGuard(command.spawn().expect("spawn foreground recorder"))
}

/// Waits for the spawned recorder to publish its first heartbeat and returns
/// the status it published, so assertions see the recorder's own startup
/// decision rather than the request the CLI wrote before it.
fn wait_for_running(recorder: &mut RecorderGuard, fixture: &Fixture) -> StoreStatus {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        // The store does not exist yet on the first passes, and the recorder
        // publishes its identity only once it owns the store.
        if let Ok(status) = StoreReader::open_with_key(&fixture.store, Some(&fixture.key()))
            .and_then(|reader| reader.status())
            && status.running
        {
            return status;
        }
        if let Some(exit) = recorder.0.try_wait().expect("poll recorder startup") {
            panic!("foreground recorder exited before recording: {exit}");
        }
        assert!(
            Instant::now() < deadline,
            "foreground recorder did not start within {STARTUP_TIMEOUT:?}"
        );
        thread::sleep(POLL_INTERVAL);
    }
}
