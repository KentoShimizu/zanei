//! ScreenCaptureKit single-window capture, Screen Recording access, and AppKit initialization.
//!
//! ScreenCaptureKit is loaded at runtime, so the binary's minimum macOS version stays below the
//! macOS 14 that `SCScreenshotManager` requires. A captured image exists only in process memory.

pub(super) mod runtime;

use std::{
    ffi::c_void,
    sync::mpsc::{RecvTimeoutError, sync_channel},
    time::{Duration, Instant},
};

use block2::RcBlock;

use self::runtime::{
    AutoreleasePool, CfRef, Classes, ObjcBool, ObjcId, ObjcSel, OwnedCf, OwnedObject, class,
    error_code, load_classes, msg_send, sel, send_frame_0, send_id_0, send_id_id, send_id_usize,
    send_usize_0, send_void_bool, srgb_color_space,
};
use super::geometry::{AxFrame, AxPoint, AxSize};

/// `SCStreamErrorUserDeclined`: ScreenCaptureKit's report that Screen Recording is not granted.
const SC_STREAM_ERROR_USER_DECLINED: isize = -3801;
const SC_CAPTURE_RESOLUTION_BEST: usize = 1;
pub(super) const BYTES_PER_PIXEL: usize = 4;
/// `kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big`: bytes in R, G, B, A order. Bitmap
/// contexts cannot hold straight alpha, so the buffer is un-premultiplied after drawing.
const RGBA_PREMULTIPLIED_BITMAP_INFO: u32 = 1 | (4 << 12);

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum CaptureError {
    #[error("single-window capture requires macOS 14 or later")]
    Unavailable,
    #[error("Screen Recording is not granted")]
    Permission,
    #[error("the window is not among the capturable on-screen windows")]
    WindowNotFound,
    #[error("the capture did not complete in time")]
    Timeout,
    #[error("the capture failed (code {code:?})")]
    Failed { code: Option<isize> },
}

/// One window's pixels: sRGB, straight RGBA, top row first, `rgba.len() == width * height * 4`.
/// It spans exactly the window frame without its shadow, so 0–1 image coordinates are
/// window-relative; `scale` is image pixels per window point.
pub(crate) struct WindowImage {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) scale: f64,
    pub(crate) rgba: Vec<u8>,
}

/// Whether this macOS provides single-window screenshots (macOS 14 or later).
pub(crate) fn screen_capture_available() -> bool {
    capture_classes().is_some()
}

/// Screen Recording state for this process's TCC identity. Never prompts.
pub(crate) fn screen_capture_access() -> bool {
    // SAFETY: the function takes no arguments and only reads this process's TCC state.
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Asks macOS for Screen Recording. macOS shows its dialog at most once per app identity, and a
/// grant takes effect only after the process restarts.
pub(crate) fn request_screen_capture_access() -> bool {
    // SAFETY: the function takes no arguments; macOS schedules any dialog itself.
    unsafe { CGRequestScreenCaptureAccess() }
}

/// Creates the shared `NSApplication`, which connects the process to the window server.
/// ScreenCaptureKit aborts the process when used without that connection. Call it on the main
/// thread before the first capture; repeated calls return the same instance, and no event loop
/// is started.
pub(crate) fn initialize_application() {
    let _pool = AutoreleasePool::new();
    // SAFETY: AppKit is linked, so the class exists; the returned instance is process-owned.
    unsafe { send_id_0(class(c"NSApplication"), sel(c"sharedApplication")) };
}

/// Captures exactly one window, without the cursor and independent of what covers it, at
/// `scale` image pixels per window point. Gives up after `timeout`; a late completion releases
/// its image without being observed.
pub(crate) fn capture_window(
    window_id: i64,
    scale: f64,
    timeout: Duration,
) -> Result<WindowImage, CaptureError> {
    let deadline = Instant::now() + timeout;
    let classes = capture_classes().ok_or(CaptureError::Unavailable)?;
    let [content_class, filter_class, config_class, manager_class] = classes;
    // ScreenCaptureKit calls can raise a TCC prompt themselves, so they are never made without
    // a granted preflight.
    if !screen_capture_access() {
        return Err(CaptureError::Permission);
    }
    let failed = CaptureError::Failed { code: None };
    let _pool = AutoreleasePool::new();
    let content = complete(
        deadline,
        // SAFETY: ScreenCaptureKit passes a live SCShareableContent or nil.
        |content| unsafe { OwnedObject::retain(content) },
        // SAFETY: the class method copies the heap block before returning.
        |handler| unsafe {
            msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel, ObjcBool, ObjcBool, ObjcId)>()(
                content_class as ObjcId,
                sel(c"getShareableContentExcludingDesktopWindows:onScreenWindowsOnly:completionHandler:"),
                ObjcBool::from(true),
                ObjcBool::from(true),
                handler,
            );
        },
    )?;
    // SAFETY: `content` is a retained SCShareableContent.
    let window =
        unsafe { find_window(content.as_ptr(), window_id) }.ok_or(CaptureError::WindowNotFound)?;
    // SAFETY: `window` is an SCWindow kept alive by `content`; `frame` returns a CGRect.
    let frame = unsafe { send_frame_0(window, sel(c"frame")) };
    // SAFETY: the filter is allocated and initialized with a live SCWindow.
    let filter = unsafe {
        OwnedObject::from_retained(send_id_id(
            send_id_0(filter_class as ObjcId, sel(c"alloc")),
            sel(c"initWithDesktopIndependentWindow:"),
            window,
        ))
    }
    .ok_or(failed)?;
    // SAFETY: `new` returns a retained SCStreamConfiguration; the setters take plain values.
    let configuration =
        unsafe { OwnedObject::from_retained(send_id_0(config_class as ObjcId, sel(c"new"))) }
            .ok_or(failed)?;
    let (width, height) = pixel_size(frame.size.width, frame.size.height, scale);
    // Integer properties are `size_t` or non-negative `NSInteger`, passed alike.
    let set_integer = msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel, usize)>();
    for (setter, value) in [
        (c"setWidth:", width),
        (c"setHeight:", height),
        // Sample backing pixels instead of letting the system pick a lower source resolution.
        (c"setCaptureResolution:", SC_CAPTURE_RESOLUTION_BEST),
    ] {
        unsafe { set_integer(configuration.as_ptr(), sel(setter), value) };
    }
    // Together these make the whole image exactly `frame` at the requested size.
    for (setter, value) in [
        (c"setShowsCursor:", false),
        // Upscale as well as downscale, instead of padding a size above the backing resolution.
        (c"setScalesToFit:", true),
        // Fill the requested size instead of letterboxing its rounding remainder.
        (c"setPreservesAspectRatio:", false),
        // macOS 14+: no shadow around `frame`, and no clipping of off-screen parts.
        (c"setIgnoreShadowsSingleWindow:", true),
        (c"setIgnoreGlobalClipSingleWindow:", true),
    ] {
        unsafe { send_void_bool(configuration.as_ptr(), sel(setter), value) };
    }
    let image = complete(
        deadline,
        // SAFETY: ScreenCaptureKit passes a CGImage it owns, or null.
        |image| unsafe { OwnedCf::retain(image.cast_const()) },
        // SAFETY: filter and configuration are live; the class method copies the heap block.
        |handler| unsafe {
            msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel, ObjcId, ObjcId, ObjcId)>()(
                manager_class as ObjcId,
                sel(c"captureImageWithFilter:configuration:completionHandler:"),
                filter.as_ptr(),
                configuration.as_ptr(),
                handler,
            );
        },
    )?;
    let (width, height, rgba) = rgba_pixels(image.as_ptr()).ok_or(failed)?;
    drop(image);
    Ok(WindowImage {
        width,
        height,
        scale: width as f64 / frame.size.width,
        rgba,
    })
}

/// Starts an asynchronous ScreenCaptureKit call whose completion handler receives an object or
/// null plus an `NSError` or nil, and waits for it until `deadline`.
fn complete<T: Send + 'static>(
    deadline: Instant,
    own: fn(ObjcId) -> Option<T>,
    start: impl FnOnce(ObjcId),
) -> Result<T, CaptureError> {
    let (sender, receiver) = sync_channel(1);
    let handler = RcBlock::new(move |value: ObjcId, error: ObjcId| {
        // SAFETY: the error is a live NSError or nil for the duration of the handler.
        let result = own(value).ok_or_else(|| unsafe { error_code(error) });
        // A send fails only after the caller timed out; dropping the value releases it.
        let _ = sender.try_send(result);
    });
    start(RcBlock::as_ptr(&handler).cast::<c_void>());
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result.map_err(capture_error),
        Err(RecvTimeoutError::Timeout) => Err(CaptureError::Timeout),
        Err(RecvTimeoutError::Disconnected) => Err(CaptureError::Failed { code: None }),
    }
}

fn pixel_size(width: f64, height: f64, scale: f64) -> (usize, usize) {
    let pixels = |points: f64| (points * scale).round() as usize;
    (pixels(width), pixels(height))
}

fn capture_error(code: Option<isize>) -> CaptureError {
    match code {
        Some(SC_STREAM_ERROR_USER_DECLINED) => CaptureError::Permission,
        code => CaptureError::Failed { code },
    }
}

unsafe fn find_window(content: ObjcId, window_id: i64) -> Option<ObjcId> {
    let window_number = msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel) -> u32>();
    let windows = unsafe { send_id_0(content, sel(c"windows")) };
    let count = unsafe { send_usize_0(windows, sel(c"count")) };
    (0..count)
        .map(|index| unsafe { send_id_usize(windows, sel(c"objectAtIndex:"), index) })
        .find(|&window| i64::from(unsafe { window_number(window, sel(c"windowID")) }) == window_id)
}

fn rgba_pixels(image: CfRef) -> Option<(usize, usize, Vec<u8>)> {
    // SAFETY: `image` is a live CGImage.
    let (width, height) = unsafe { (CGImageGetWidth(image), CGImageGetHeight(image)) };
    let mut rgba = vec![0; width * height * BYTES_PER_PIXEL];
    // SAFETY: the context is dropped before `rgba` is moved or freed.
    let context = unsafe { bitmap_context(&mut rgba, width, height) }?;
    let bounds = AxFrame {
        origin: AxPoint { x: 0.0, y: 0.0 },
        size: AxSize {
            width: width as f64,
            height: height as f64,
        },
    };
    // SAFETY: the context and image are live; drawing completes synchronously.
    unsafe { CGContextDrawImage(context.as_ptr(), bounds, image) };
    drop(context);
    unpremultiply(&mut rgba);
    Some((width, height, rgba))
}

/// An sRGB premultiplied RGBA bitmap context drawing into `rgba`.
///
/// # Safety
/// `rgba` must outlive the returned context.
pub(super) unsafe fn bitmap_context(
    rgba: &mut [u8],
    width: usize,
    height: usize,
) -> Option<OwnedCf> {
    assert_eq!(rgba.len(), width * height * BYTES_PER_PIXEL);
    let color_space = srgb_color_space()?;
    // SAFETY: `rgba` holds `height` rows of `width * 4` bytes; the result follows the create rule.
    unsafe {
        OwnedCf::from_created(CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            width,
            height,
            8,
            width * BYTES_PER_PIXEL,
            color_space.as_ptr(),
            RGBA_PREMULTIPLIED_BITMAP_INFO,
        ))
    }
}

fn unpremultiply(rgba: &mut [u8]) {
    for pixel in rgba.as_chunks_mut::<BYTES_PER_PIXEL>().0 {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            let straight = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = straight.min(255) as u8;
        }
    }
}

fn capture_classes() -> Option<[usize; 4]> {
    static CLASSES: Classes<4> = Classes::new();
    load_classes(
        &CLASSES,
        c"/System/Library/Frameworks/ScreenCaptureKit.framework/ScreenCaptureKit",
        [
            c"SCShareableContent",
            c"SCContentFilter",
            c"SCStreamConfiguration",
            // The newest of the four (macOS 14); its presence is the availability gate.
            c"SCScreenshotManager",
        ],
    )
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGImageGetWidth(image: CfRef) -> usize;
    fn CGImageGetHeight(image: CfRef) -> usize;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        color_space: CfRef,
        bitmap_info: u32,
    ) -> CfRef;
    fn CGContextDrawImage(context: CfRef, rect: AxFrame, image: CfRef);
}

#[cfg(test)]
mod tests {
    use super::{CaptureError, capture_error, pixel_size, unpremultiply};

    #[test]
    fn requested_image_spans_the_window_frame_at_the_scale() {
        assert_eq!(pixel_size(1701.0, 988.5, 1.0), (1701, 989));
        assert_eq!(pixel_size(1701.0, 988.5, 2.0), (3402, 1977));
    }

    #[test]
    fn declined_screen_recording_is_a_permission_outcome_not_a_failure() {
        assert_eq!(capture_error(Some(-3801)), CaptureError::Permission);
        assert_eq!(
            capture_error(Some(-3802)),
            CaptureError::Failed { code: Some(-3802) }
        );
        assert_eq!(capture_error(None), CaptureError::Failed { code: None });
    }

    #[test]
    fn translucent_pixels_are_restored_to_straight_alpha() {
        let mut rgba = [
            64, 0, 128, 128, // half transparent: colour channels were halved
            10, 20, 30, 255, // opaque: unchanged
            0, 0, 0, 0, // fully transparent: unchanged
        ];
        unpremultiply(&mut rgba);
        assert_eq!(rgba, [128, 0, 255, 128, 10, 20, 30, 255, 0, 0, 0, 0]);
    }
}
