//! The text-recognition thread: captures one window, skips recognition when the image equals the
//! one behind the window's last stored recognized body, and recognizes the rest. Window images
//! exist only on this thread and are dropped before a result is returned.

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, TryRecvError, channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use zanei_core::schema::{ContentSnapshotData, ContentSnapshotTrigger};

use crate::{
    content_snapshot::{budget::OCR_CAPTURE_TIMEOUT, coverage::WalkAnalysis, ocr},
    ffi::{
        screen_capture::{
            CaptureError, WindowImage, capture_window, initialize_application,
            screen_capture_access, screen_capture_available,
        },
        text_recognition::{RecognitionError, RecognizedLine, recognize_text},
    },
};

/// Cells per side of the grid a capture is summed over before comparing it with the image
/// behind the window's last stored recognized body.
const FINGERPRINT_GRID: usize = 64;
const RGBA_BYTES: usize = 4;

static WINDOW_CAPTURE_PREPARED: AtomicBool = AtomicBool::new(false);

/// Connects the process to the window server, without which window capture aborts the
/// process. Call once on the main thread before starting the content snapshot collector;
/// until then, text recognition stays off.
pub fn prepare_window_capture() {
    initialize_application();
    WINDOW_CAPTURE_PREPARED.store(true, Ordering::Release);
}

/// Why a candidate has no recognized body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::content_snapshot) enum OcrUnavailable {
    OsUnsupported,
    Permission,
    /// The process has not connected to the window server.
    Unprepared,
    WindowNotFound,
    CaptureTimeout,
    CaptureFailed,
    /// No recognized line, which includes protected and blank windows.
    Blank,
    AllMasked,
    RecognitionFailed,
    /// The recognition thread is gone.
    Stopped,
}

impl OcrUnavailable {
    /// Failures count toward the collector's health; the rest are states or empty results.
    pub(in crate::content_snapshot) const fn is_failure(self) -> bool {
        matches!(
            self,
            Self::WindowNotFound
                | Self::CaptureTimeout
                | Self::CaptureFailed
                | Self::RecognitionFailed
                | Self::Stopped
        )
    }

    pub(in crate::content_snapshot) const fn trace_name(self) -> &'static str {
        match self {
            Self::OsUnsupported => "os_unsupported",
            Self::Permission => "permission",
            Self::Unprepared => "unprepared",
            Self::WindowNotFound => "window_not_found",
            Self::CaptureTimeout => "capture_timeout",
            Self::CaptureFailed => "capture_failed",
            Self::Blank => "blank",
            Self::AllMasked => "all_masked",
            Self::RecognitionFailed => "recognition_failed",
            Self::Stopped => "stopped",
        }
    }
}

impl From<CaptureError> for OcrUnavailable {
    fn from(error: CaptureError) -> Self {
        match error {
            CaptureError::Unavailable => Self::OsUnsupported,
            CaptureError::Permission => Self::Permission,
            CaptureError::WindowNotFound => Self::WindowNotFound,
            CaptureError::Timeout => Self::CaptureTimeout,
            CaptureError::Failed { .. } => Self::CaptureFailed,
        }
    }
}

pub(in crate::content_snapshot) struct OcrRequest {
    pub(in crate::content_snapshot) window_id: i64,
    /// Fingerprint of the image behind the window's last stored recognized body.
    pub(in crate::content_snapshot) stored_image: Option<u64>,
    pub(in crate::content_snapshot) analysis: WalkAnalysis,
    pub(in crate::content_snapshot) trigger: ContentSnapshotTrigger,
}

pub(in crate::content_snapshot) enum OcrOutcome {
    Recognized {
        data: ContentSnapshotData,
        image: u64,
        capture: Duration,
        recognize: Duration,
        lines: usize,
    },
    /// The image equals the one behind the last stored recognized body.
    Unchanged,
    Unavailable(OcrUnavailable),
}

pub(in crate::content_snapshot) type Capture =
    Box<dyn FnMut(i64) -> Result<WindowImage, CaptureError> + Send>;
pub(in crate::content_snapshot) type Recognize =
    Box<dyn FnMut(&WindowImage) -> Result<Vec<RecognizedLine>, RecognitionError> + Send>;

/// The platform calls behind the engine; tests substitute them.
pub(in crate::content_snapshot) struct OcrBackend {
    /// Checked on the worker before every request, so nothing is captured without access.
    pub(in crate::content_snapshot) access: fn() -> Result<(), OcrUnavailable>,
    pub(in crate::content_snapshot) capture: Capture,
    pub(in crate::content_snapshot) recognize: Recognize,
}

impl OcrBackend {
    pub(in crate::content_snapshot) fn native() -> Self {
        Self {
            access: native_access,
            capture: Box::new(|window_id| capture_window(window_id, OCR_CAPTURE_TIMEOUT)),
            recognize: Box::new(recognize_text),
        }
    }
}

fn native_access() -> Result<(), OcrUnavailable> {
    if !screen_capture_available() {
        Err(OcrUnavailable::OsUnsupported)
    } else if !WINDOW_CAPTURE_PREPARED.load(Ordering::Acquire) {
        Err(OcrUnavailable::Unprepared)
    } else if !screen_capture_access() {
        Err(OcrUnavailable::Permission)
    } else {
        Ok(())
    }
}

/// Worker-side handle of the `zanei-ocr` thread. The worker keeps at most one request in
/// flight; dropping the handle discards an unread result and joins the thread.
pub(in crate::content_snapshot) struct OcrEngine {
    access: fn() -> Result<(), OcrUnavailable>,
    requests: Option<Sender<OcrRequest>>,
    results: Receiver<OcrOutcome>,
    thread: Option<JoinHandle<()>>,
}

impl OcrEngine {
    pub(in crate::content_snapshot) fn start(backend: OcrBackend) -> io::Result<Self> {
        let OcrBackend {
            access,
            mut capture,
            mut recognize,
        } = backend;
        let (requests, jobs) = channel::<OcrRequest>();
        let (completed, results) = channel();
        let thread = thread::Builder::new()
            .name("zanei-ocr".to_owned())
            .spawn(move || {
                for request in jobs {
                    let outcome = recognize_window(&mut capture, &mut recognize, request);
                    if completed.send(outcome).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            access,
            requests: Some(requests),
            results,
            thread: Some(thread),
        })
    }

    pub(in crate::content_snapshot) fn access(&self) -> Result<(), OcrUnavailable> {
        (self.access)()
    }

    pub(in crate::content_snapshot) fn submit(&self, request: OcrRequest) {
        // A send fails only when the thread is gone, which `poll` then reports.
        let _ = self
            .requests
            .as_ref()
            .expect("requests close only on drop")
            .send(request);
    }

    pub(in crate::content_snapshot) fn poll(&self) -> Option<OcrOutcome> {
        match self.results.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(OcrOutcome::Unavailable(OcrUnavailable::Stopped))
            }
        }
    }
}

impl Drop for OcrEngine {
    fn drop(&mut self) {
        self.requests = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(in crate::content_snapshot) fn recognize_window(
    capture: &mut Capture,
    recognize: &mut Recognize,
    request: OcrRequest,
) -> OcrOutcome {
    let started = Instant::now();
    let image = match capture(request.window_id) {
        Ok(image) => image,
        Err(error) => return OcrOutcome::Unavailable(error.into()),
    };
    let captured = started.elapsed();
    let fingerprint = fingerprint(&image);
    if request.stored_image == Some(fingerprint) {
        return OcrOutcome::Unchanged;
    }
    let Ok(lines) = recognize(&image) else {
        return OcrOutcome::Unavailable(OcrUnavailable::RecognitionFailed);
    };
    let recognized = lines.len();
    if recognized == 0 {
        return OcrOutcome::Unavailable(OcrUnavailable::Blank);
    }
    let data = ocr::recognized_snapshot(
        &image,
        lines,
        &request.analysis.mask_regions,
        &request.analysis.order_containers,
        request.trigger,
    );
    drop(image);
    match data {
        Some(data) => OcrOutcome::Recognized {
            data,
            image: fingerprint,
            capture: captured,
            recognize: started.elapsed().saturating_sub(captured),
            lines: recognized,
        },
        None => OcrOutcome::Unavailable(OcrUnavailable::AllMasked),
    }
}

/// Sums each colour channel over the cells of a `FINGERPRINT_GRID` square grid and hashes the
/// sums with the image size. A visible change alters a cell sum, so equal fingerprints mean an
/// unchanged window; the hash keeps nothing of the image.
pub(in crate::content_snapshot) fn fingerprint(image: &WindowImage) -> u64 {
    let mut sums = vec![[0_u64; 3]; FINGERPRINT_GRID * FINGERPRINT_GRID];
    let columns: Vec<usize> = (0..image.width)
        .map(|column| column * FINGERPRINT_GRID / image.width)
        .collect();
    for (row, pixels) in image
        .rgba
        .chunks_exact(image.width * RGBA_BYTES)
        .enumerate()
    {
        let cells = &mut sums[row * FINGERPRINT_GRID / image.height * FINGERPRINT_GRID..];
        for (pixel, &column) in pixels.as_chunks::<RGBA_BYTES>().0.iter().zip(&columns) {
            for (sum, &channel) in cells[column].iter_mut().zip(pixel) {
                *sum += u64::from(channel);
            }
        }
    }
    let mut hasher = DefaultHasher::new();
    (image.width, image.height, sums).hash(&mut hasher);
    hasher.finish()
}
