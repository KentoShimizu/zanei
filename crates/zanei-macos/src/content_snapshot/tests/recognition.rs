use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{Receiver, Sender, channel, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use zanei_collector::RawEvent;
use zanei_core::{config::FilterConfig, schema::EventData};

use crate::{
    CapturePolicy,
    chrome::{ChromeObserver, chrome_eligibility_channel},
    content_snapshot::{
        Control, SharedHealth, SnapshotTriggerKind, SnapshotTriggerPublisher, SnapshotWalkOutput,
        budget::GLOBAL_SAVE_INTERVAL,
        coverage::{MaskGap, MaskGaps, WalkAnalysis, WindowRect},
        ocr_engine::{OcrBackend, OcrEngine, OcrUnavailable, fingerprint},
        snapshot_trigger_channel,
        state::{SnapshotOrigin, SnapshotState, SnapshotWindowKey},
        worker::run_worker_with_scanner,
    },
    ffi::{
        screen_capture::{CaptureError, WindowImage},
        text_recognition::{NormalizedBox, RecognizedLine},
    },
    focus_context::FocusContext,
    secure_input::secure_input_test_channel,
    workspace::notification_channel,
};

use super::support::trigger;

const AX_TEXT: &str = "accessibility remainder";
const RECOGNIZED: &str = "recognized message";

fn image(shade: u8) -> WindowImage {
    WindowImage {
        width: 8,
        height: 4,
        scale: 1.0,
        rgba: [shade, shade, shade, 255].repeat(32),
    }
}

fn analysis(mask_regions: Vec<WindowRect>) -> WalkAnalysis {
    WalkAnalysis {
        blank_coverage: 1.0,
        mask_regions,
        order_containers: Vec::new(),
        mask_gaps: MaskGaps::default(),
    }
}

fn lines() -> Vec<RecognizedLine> {
    vec![RecognizedLine {
        text: RECOGNIZED.to_owned(),
        bounds: NormalizedBox {
            x: 0.1,
            y: 0.1,
            width: 0.5,
            height: 0.2,
        },
    }]
}

fn backend(
    access: fn() -> Result<(), OcrUnavailable>,
    capture: impl FnMut(i64) -> Result<WindowImage, CaptureError> + Send + 'static,
) -> OcrBackend {
    OcrBackend {
        access,
        capture: Box::new(capture),
        recognize: Box::new(|_| Ok(lines())),
    }
}

struct Run {
    _triggers: SnapshotTriggerPublisher,
    control: Sender<Control>,
    stop: Arc<AtomicBool>,
    events: Receiver<RawEvent>,
    health: SharedHealth,
    worker: JoinHandle<()>,
    secure: JoinHandle<()>,
}

impl Run {
    /// Runs the worker on one insufficient window whose walk yields `AX_TEXT`, answering the
    /// Secure Input queries with `secure` in order.
    fn start(
        backend: OcrBackend,
        walk: WalkAnalysis,
        secure: &'static [bool],
        stored_image: Option<u64>,
    ) -> (Self, CapturePolicy) {
        let mut state = SnapshotState::new(Instant::now());
        if let Some(image) = stored_image {
            let key = SnapshotWindowKey {
                pid: 7,
                window_id: 11,
            };
            let earlier = Instant::now() - GLOBAL_SAVE_INTERVAL;
            state.reserve(key, 1, earlier);
            state.record_hash(key, SnapshotOrigin::Ocr, 0);
            state.record_recognized_image(key, image);
        }
        let (trigger_publisher, trigger_receiver) = snapshot_trigger_channel();
        let (_lifecycle_publisher, lifecycle_receiver) = notification_channel();
        let (control, controls) = channel();
        let (sender, events) = sync_channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let health = SharedHealth::default();
        let focus_context = FocusContext::new();
        let target = trigger(
            7,
            11,
            SnapshotTriggerKind::Focus,
            Instant::now() - Duration::from_secs(3),
        );
        focus_context.activate(target.app.clone(), Some(target.window.clone()));
        let filter = FilterConfig::default();
        let (_chrome_publisher, chrome) = chrome_eligibility_channel(filter.clone());
        let (secure_input, responder) = secure_input_test_channel();
        let policy = CapturePolicy::new(chrome, filter, Some(secure_input));
        let ocr = OcrEngine::start(backend).expect("start recognition thread");
        let (worker_stop, worker_health, worker_policy) =
            (Arc::clone(&stop), health.clone(), policy.clone());
        let worker = thread::Builder::new()
            .name("zanei-content".to_owned())
            .spawn(move || {
                run_worker_with_scanner(
                    &trigger_receiver,
                    &lifecycle_receiver,
                    controls,
                    worker_stop,
                    sender,
                    worker_policy,
                    ChromeObserver::new(),
                    worker_health,
                    &mut state,
                    focus_context,
                    ocr,
                    move |_pid, _window_id, _stop, read_allowed| {
                        read_allowed(None);
                        let output = SnapshotWalkOutput {
                            text: AX_TEXT.to_owned(),
                            nodes: 1,
                            ax_calls: 1,
                            elapsed: Duration::from_millis(1),
                            cutoff: None,
                            degraded_nodes: 0,
                            frameless_nodes: 0,
                        };
                        Ok(Some((output, walk.clone())))
                    },
                );
            })
            .expect("spawn content worker");
        assert!(trigger_publisher.publish(target));
        let secure =
            thread::spawn(move || secure.iter().for_each(|&on| responder.respond_next(on)));
        let run = Self {
            _triggers: trigger_publisher,
            control,
            stop,
            events,
            health,
            worker,
            secure,
        };
        (run, policy)
    }

    fn next_event(&self) -> RawEvent {
        self.events
            .recv_timeout(Duration::from_secs(5))
            .expect("snapshot event")
    }

    /// Stops after every Secure Input answer, returning the events not yet taken.
    fn finish(self) -> Vec<RawEvent> {
        self.secure.join().expect("Secure Input responder");
        self.stop.store(true, Ordering::Release);
        self.worker.join().expect("content worker");
        self.events.try_iter().collect()
    }
}

fn text(event: &RawEvent) -> (&str, Option<&str>) {
    let EventData::ContentSnapshot(data) = &event.data else {
        panic!("content.snapshot");
    };
    (event.source.as_str(), data.text.as_deref())
}

#[test]
fn recognized_text_replaces_the_accessibility_body() {
    // Catches emitting both bodies for one candidate, or the recognized body as `macos.ax`.
    let (run, _) = Run::start(
        backend(|| Ok(()), |_| Ok(image(1))),
        analysis(Vec::new()),
        &[false; 3],
        None,
    );
    assert_eq!(text(&run.next_event()), ("macos.ocr", Some(RECOGNIZED)));
    assert!(run.finish().is_empty());
}

#[test]
fn without_a_recognized_body_the_accessibility_body_is_recorded() {
    // Catches a regression without recognition: denied access, masks that may miss an input,
    // a capture failure, and fully masked lines each still record today's body; only a
    // failure counts toward health, and without access or complete masks nothing is captured.
    let full_window = WindowRect {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };
    let captures = Arc::new(AtomicUsize::new(0));
    let counted = |access| {
        let counted = Arc::clone(&captures);
        backend(access, move |_| {
            counted.fetch_add(1, Ordering::Relaxed);
            Ok(image(1))
        })
    };
    let denied = counted(|| Err(OcrUnavailable::Permission));
    let mut incomplete = analysis(Vec::new());
    incomplete.mask_gaps.record(MaskGap::FramelessInput);
    let timed_out = backend(|| Ok(()), |_| Err(CaptureError::Timeout));
    let masked = backend(|| Ok(()), |_| Ok(image(1)));
    for (backend, walk, secure, failures) in [
        (
            denied,
            analysis(Vec::new()),
            &[false; 2] as &'static [bool],
            0,
        ),
        (counted(|| Ok(())), incomplete, &[false; 2], 0),
        (timed_out, analysis(Vec::new()), &[false; 3], 1),
        (masked, analysis(vec![full_window]), &[false; 3], 0),
    ] {
        let (run, _) = Run::start(backend, walk, secure, None);
        assert_eq!(text(&run.next_event()), ("macos.ax", Some(AX_TEXT)));
        assert_eq!(run.health.failures.load(Ordering::Relaxed), failures);
        assert!(run.finish().is_empty());
    }
    assert_eq!(captures.load(Ordering::Relaxed), 0);
}

#[test]
fn send_time_gates_apply_to_recognized_text() {
    // Catches recognized text bypassing a Secure Input that started, or a scope change that
    // happened, while the window was being recognized.
    let (secure_on, _) = Run::start(
        backend(|| Ok(()), |_| Ok(image(1))),
        analysis(Vec::new()),
        &[false, false, true],
        None,
    );
    assert!(secure_on.finish().is_empty());

    let (excluded_sender, excluded) = channel::<CapturePolicy>();
    let (run, policy) = Run::start(
        backend(
            || Ok(()),
            move |_| {
                excluded
                    .recv()
                    .expect("policy")
                    .replace_filter(FilterConfig {
                        exclude_apps: vec!["dev.example.App".to_owned()],
                        ..FilterConfig::default()
                    });
                Ok(image(1))
            },
        ),
        analysis(Vec::new()),
        &[false; 3],
        None,
    );
    excluded_sender
        .send(policy)
        .expect("capture waits for the policy");
    assert!(run.finish().is_empty());
}

#[test]
fn recognition_keeps_the_worker_responsive_and_stop_discards_its_result() {
    // Catches the worker blocking on recognition, which would time out a filter reload, and a
    // late result escaping after stop.
    let (started_sender, started) = channel();
    let (release, released) = channel::<()>();
    let blocking = OcrBackend {
        access: || Ok(()),
        capture: Box::new(|_| Ok(image(1))),
        recognize: Box::new(move |_| {
            started_sender.send(()).expect("test waits for recognition");
            released.recv().expect("test releases recognition");
            Ok(lines())
        }),
    };
    let (run, _) = Run::start(blocking, analysis(Vec::new()), &[false; 2], None);
    started
        .recv_timeout(Duration::from_secs(5))
        .expect("recognition started");
    let (acknowledge, acknowledged) = sync_channel(0);
    run.control
        .send(Control::ReplaceFilter { acknowledge })
        .expect("worker is running");
    acknowledged
        .recv_timeout(Duration::from_secs(2))
        .expect("filter reload acknowledged during recognition");
    run.stop.store(true, Ordering::Release);
    release.send(()).expect("recognition is waiting");
    assert!(run.finish().is_empty());
}

#[test]
fn an_unchanged_window_image_records_nothing() {
    // Catches the Accessibility body, or the recognized body again, being recorded while the
    // window shows the image behind its last stored recognized body.
    let (run, policy) = Run::start(
        backend(|| Ok(()), |_| Ok(image(1))),
        analysis(Vec::new()),
        // Sending any body would take the third Secure Input answer.
        &[false; 3],
        Some(fingerprint(&image(1))),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while run.health.finished_recognitions.load(Ordering::Acquire) == 0 {
        assert!(
            Instant::now() < deadline,
            "the recognition outcome is handled"
        );
        thread::yield_now();
    }
    assert!(
        policy.secure_input_allows(),
        "the third Secure Input answer is still unused"
    );
    assert!(run.finish().is_empty());
}
