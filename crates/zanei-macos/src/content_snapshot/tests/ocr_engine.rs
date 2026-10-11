use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use zanei_core::schema::ContentSnapshotTrigger;

use crate::{
    content_snapshot::{
        coverage::{WalkAnalysis, WindowRect},
        ocr_engine::{
            Capture, OcrBackend, OcrEngine, OcrOutcome, OcrRequest, OcrUnavailable, Recognize,
            fingerprint, recognize_window,
        },
    },
    ffi::{
        screen_capture::{CaptureError, WindowImage},
        text_recognition::{NormalizedBox, RecognitionError, RecognizedLine},
    },
};

fn image(shade: u8) -> WindowImage {
    WindowImage {
        width: 8,
        height: 4,
        scale: 1.0,
        rgba: [shade, shade, shade, 255].repeat(32),
    }
}

fn lines() -> Vec<RecognizedLine> {
    vec![RecognizedLine {
        text: "recognized message".to_owned(),
        bounds: NormalizedBox {
            x: 0.1,
            y: 0.1,
            width: 0.5,
            height: 0.2,
        },
    }]
}

fn request(stored_image: Option<u64>, mask_regions: Vec<WindowRect>) -> OcrRequest {
    OcrRequest {
        window_id: 11,
        stored_image,
        analysis: WalkAnalysis {
            blank_coverage: 1.0,
            mask_regions,
            order_containers: Vec::new(),
            masks_complete: true,
        },
        trigger: ContentSnapshotTrigger::Refresh,
    }
}

fn outcome(
    capture: Result<u8, CaptureError>,
    recognized: Result<Vec<RecognizedLine>, RecognitionError>,
    mask_regions: Vec<WindowRect>,
) -> OcrOutcome {
    let mut capture: Capture = Box::new(move |_| capture.map(image));
    let mut recognize: Recognize = Box::new(move |_| recognized.clone());
    recognize_window(&mut capture, &mut recognize, request(None, mask_regions))
}

#[test]
fn an_unchanged_window_image_is_not_recognized_again() {
    // Catches recognition running, and a body being produced, for the image behind the last
    // stored recognized body, and a changed image being taken for that one.
    let recognitions = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&recognitions);
    let mut capture: Capture = Box::new(|_| Ok(image(1)));
    let mut recognize: Recognize = Box::new(move |_| {
        counted.fetch_add(1, Ordering::Relaxed);
        Ok(lines())
    });

    let stored = fingerprint(&image(1));
    let unchanged = recognize_window(
        &mut capture,
        &mut recognize,
        request(Some(stored), Vec::new()),
    );
    assert!(matches!(unchanged, OcrOutcome::Unchanged));
    assert_eq!(recognitions.load(Ordering::Relaxed), 0);

    let other = Some(fingerprint(&image(2)));
    let changed = recognize_window(&mut capture, &mut recognize, request(other, Vec::new()));
    assert!(matches!(changed, OcrOutcome::Recognized { image, .. } if image == stored));
    assert_eq!(recognitions.load(Ordering::Relaxed), 1);
}

#[test]
fn only_real_failures_count_against_health() {
    // Catches a declined permission, an older macOS, a blank window, or fully masked lines
    // being counted as collector failures, and capture or recognition errors being missed.
    let whole_window = WindowRect {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };
    for (outcome, expected, failure) in [
        (
            outcome(Err(CaptureError::Permission), Ok(lines()), Vec::new()),
            OcrUnavailable::Permission,
            false,
        ),
        (
            outcome(Err(CaptureError::Unavailable), Ok(lines()), Vec::new()),
            OcrUnavailable::OsUnsupported,
            false,
        ),
        (
            outcome(Ok(1), Ok(Vec::new()), Vec::new()),
            OcrUnavailable::Blank,
            false,
        ),
        (
            outcome(Ok(1), Ok(lines()), vec![whole_window]),
            OcrUnavailable::AllMasked,
            false,
        ),
        (
            outcome(Err(CaptureError::Timeout), Ok(lines()), Vec::new()),
            OcrUnavailable::CaptureTimeout,
            true,
        ),
        (
            outcome(Err(CaptureError::WindowNotFound), Ok(lines()), Vec::new()),
            OcrUnavailable::WindowNotFound,
            true,
        ),
        (
            outcome(Ok(1), Err(RecognitionError { code: None }), Vec::new()),
            OcrUnavailable::RecognitionFailed,
            true,
        ),
    ] {
        let OcrOutcome::Unavailable(reason) = outcome else {
            panic!("expected {expected:?}");
        };
        assert_eq!(reason, expected);
        assert_eq!(reason.is_failure(), failure, "{expected:?}");
    }
}

#[test]
fn a_lost_recognition_thread_is_reported_instead_of_awaited() {
    // Catches the worker waiting forever for a result from a thread that died mid-request.
    let engine = OcrEngine::start(OcrBackend {
        access: || Ok(()),
        capture: Box::new(|_| panic!("injected capture panic")),
        recognize: Box::new(|_| Ok(lines())),
    })
    .expect("start recognition thread");
    engine.submit(request(None, Vec::new()));
    let deadline = Instant::now() + Duration::from_secs(5);
    let outcome = loop {
        if let Some(outcome) = engine.poll() {
            break outcome;
        }
        assert!(Instant::now() < deadline, "no outcome from a dead thread");
        thread::yield_now();
    };
    assert!(matches!(
        outcome,
        OcrOutcome::Unavailable(OcrUnavailable::Stopped)
    ));
}

#[test]
fn rearranged_pixels_count_as_a_changed_image() {
    // Catches a fingerprint that keeps only per-region totals, so text whose characters swap
    // places within one region (e.g. "12" becoming "21") is never recognized again.
    let mut before = image(1);
    before.width = 128;
    before.height = 1;
    before.rgba = [10, 20, 30, 255].repeat(128);
    before.rgba[..8].copy_from_slice(&[0, 0, 0, 255, 255, 255, 255, 255]);
    let mut after = WindowImage {
        rgba: before.rgba.clone(),
        ..before
    };
    after.rgba[..8].copy_from_slice(&[255, 255, 255, 255, 0, 0, 0, 255]);
    let stored = fingerprint(&before);
    assert_ne!(fingerprint(&after), stored);

    let mut capture: Capture = Box::new(move |_| {
        Ok(WindowImage {
            rgba: after.rgba.clone(),
            ..after
        })
    });
    let mut recognize: Recognize = Box::new(|_| Ok(lines()));
    let outcome = recognize_window(
        &mut capture,
        &mut recognize,
        request(Some(stored), Vec::new()),
    );
    assert!(matches!(outcome, OcrOutcome::Recognized { .. }));
}
