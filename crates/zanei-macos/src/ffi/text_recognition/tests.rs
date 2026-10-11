use std::{
    ffi::{CString, c_char, c_long, c_void},
    ptr,
    time::{Duration, Instant},
};

use super::{AxFrame, CfRef, OwnedCf, RecognizedLine, normalized_box, recognize_text};
use crate::ffi::{
    geometry::{AxPoint, AxSize},
    screen_capture::{
        CaptureError, WindowImage, bitmap_context, capture_window, initialize_application,
        screen_capture_access, screen_capture_available,
    },
    window_list::front_window,
    workspace::frontmost_application,
};

const LIVE_CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);

fn box_of(x: f64, y: f64, width: f64, height: f64) -> [f64; 4] {
    let (origin, size) = (AxPoint { x, y }, AxSize { width, height });
    let bounds = normalized_box(AxFrame { origin, size });
    [bounds.x, bounds.y, bounds.width, bounds.height].map(|value| (value * 1e6).round() / 1e6)
}

#[test]
fn vision_box_becomes_a_top_left_origin_box_clamped_to_the_window() {
    // A line near the top of the window has a large Vision y.
    assert_eq!(box_of(0.1, 0.8, 0.4, 0.1), [0.1, 0.1, 0.4, 0.1]);
    // Vision can place an edge slightly outside the image.
    assert_eq!(box_of(-0.02, 0.95, 0.32, 0.06), [0.0, 0.0, 0.3, 0.05]);
}

const SYNTHETIC_LINES: [&str; 6] = [
    "Quarterly planning notes",
    "会議は午後三時から始まります",
    "Build finished in 42 seconds",
    "明日の予定を確認してください",
    "Release checklist updated",
    "資料は共有フォルダにあります",
];

/// Recognizes `image` and prints only its size, timing, and counts, never recognized text.
fn recognize_and_report(label: &str, image: &WindowImage) -> Vec<RecognizedLine> {
    let started = Instant::now();
    let lines = recognize_text(image).expect("recognition");
    let chars: usize = lines.iter().map(|line| line.text.chars().count()).sum();
    let (width, height, scale) = (image.width, image.height, image.scale);
    let recognize_ms = started.elapsed().as_millis();
    let count = lines.len();
    eprintln!(
        "{label} px={width}x{height} scale={scale:.3} recognize_ms={recognize_ms} lines={count} chars={chars}"
    );
    lines
}

#[test]
#[ignore = "runs Vision on this Mac"]
fn live_vision_recognizes_synthetic_lines_top_to_bottom() {
    for scale in [1.0, 2.0] {
        let lines = recognize_and_report("synthetic", &render_lines(&SYNTHETIC_LINES, scale));
        let tops: Vec<_> = SYNTHETIC_LINES
            .iter()
            .map(|expected| {
                let line = lines.iter().find(|line| line.text == *expected);
                line.unwrap_or_else(|| panic!("missing {expected:?} at {scale}x"))
                    .bounds
                    .y
            })
            .collect();
        let top_down = tops.windows(2).all(|pair| pair[0] < pair[1]);
        assert!(
            top_down,
            "first drawn line must be nearest the top: {tops:?}"
        );
    }
}

#[test]
#[ignore = "captures a real window; needs Screen Recording for the terminal running it"]
fn live_capture_and_recognition_at_the_backing_scale() {
    initialize_application();
    assert!(screen_capture_available(), "requires macOS 14 or later");
    assert!(screen_capture_access(), "needs Screen Recording");
    let pid = std::env::var("ZANEI_OCR_TEST_PID").map_or_else(
        |_| frontmost_application().map(|app| i64::from(app.pid)),
        |pid| pid.parse().ok(),
    );
    let window = pid.and_then(front_window).expect("an on-screen window");
    let window_id = window.id.expect("a window id");
    // An expired deadline must not block.
    let expired = capture_window(window_id, Duration::ZERO);
    assert_eq!(expired.err(), Some(CaptureError::Timeout));

    for attempt in 1..=2 {
        let started = Instant::now();
        let image = capture_window(window_id, LIVE_CAPTURE_TIMEOUT).expect("capture");
        let capture_ms = started.elapsed().as_millis();
        recognize_and_report(
            &format!("capture attempt={attempt} capture_ms={capture_ms}"),
            &image,
        );
    }
}

/// Draws black text lines on an opaque white image, `scale` pixels per point, 13 pt type.
fn render_lines(lines: &[&str], scale: f64) -> WindowImage {
    const LINE_PITCH: f64 = 28.0;
    const MARGIN: f64 = 40.0;
    let width = (720.0 * scale) as usize;
    let height = ((2.0 * MARGIN + LINE_PITCH * lines.len() as f64) * scale) as usize;
    let mut rgba = vec![u8::MAX; width * height * 4];
    // SAFETY: the context is dropped before `rgba` moves into the image.
    let context = unsafe { bitmap_context(&mut rgba, width, height) }.expect("bitmap context");
    let null = ptr::null();
    unsafe {
        let name = cf_string("Hiragino Sans");
        let font = owned(CTFontCreateWithName(name.as_ptr(), 13.0 * scale, null));
        let keys = [kCTFontAttributeName];
        let values = [font.as_ptr()];
        let attributes = owned(CFDictionaryCreate(
            null,
            keys.as_ptr(),
            values.as_ptr(),
            1,
            (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
            (&raw const kCFTypeDictionaryValueCallBacks).cast(),
        ));
        for (index, text) in lines.iter().enumerate() {
            let string = cf_string(text);
            let attributed = CFAttributedStringCreate(null, string.as_ptr(), attributes.as_ptr());
            let attributed = owned(attributed);
            let line = owned(CTLineCreateWithAttributedString(attributed.as_ptr()));
            // Core Graphics text positions count up from the bottom edge.
            let baseline_from_top = MARGIN + LINE_PITCH * index as f64 + 13.0;
            let y = height as f64 - baseline_from_top * scale;
            CGContextSetTextPosition(context.as_ptr(), MARGIN * scale, y);
            CTLineDraw(line.as_ptr(), context.as_ptr());
        }
    }
    drop(context);
    WindowImage {
        width,
        height,
        scale,
        rgba,
    }
}

unsafe fn owned(value: CfRef) -> OwnedCf {
    unsafe { OwnedCf::from_created(value) }.expect("Core Foundation object")
}

unsafe fn cf_string(text: &str) -> OwnedCf {
    let text = CString::new(text).expect("no interior NUL");
    let string = unsafe { CFStringCreateWithCString(ptr::null(), text.as_ptr(), 0x0800_0100) };
    unsafe { owned(string) }
}

#[link(name = "CoreText", kind = "framework")]
unsafe extern "C" {
    static kCTFontAttributeName: CfRef;
    fn CTFontCreateWithName(name: CfRef, size: f64, matrix: CfRef) -> CfRef;
    fn CTLineCreateWithAttributedString(string: CfRef) -> CfRef;
    fn CTLineDraw(line: CfRef, context: CfRef);
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGContextSetTextPosition(context: CfRef, x: f64, y: f64);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    fn CFStringCreateWithCString(allocator: CfRef, text: *const c_char, encoding: u32) -> CfRef;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const CfRef,
        values: *const CfRef,
        count: c_long,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CfRef;
    fn CFAttributedStringCreate(allocator: CfRef, string: CfRef, attributes: CfRef) -> CfRef;
}
