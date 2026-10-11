//! Vision text recognition over an in-memory window image.
//!
//! Vision is loaded at runtime so processes that never recognize text do not pay for loading it.
//! Recognition is synchronous and runs on the calling thread.

use std::{
    ffi::{CStr, c_char, c_void},
    ptr::{self, NonNull},
};

use super::{
    geometry::AxPoint,
    screen_capture::{
        BYTES_PER_PIXEL, WindowImage,
        runtime::{
            AutoreleasePool, CfRef, Classes, ObjcBool, ObjcId, ObjcSel, OwnedCf, OwnedObject,
            class, error_code, load_classes, msg_send, sel, send_id_0, send_id_id, send_id_usize,
            send_usize_0, send_void_bool, srgb_color_space,
        },
    },
};

const VN_REQUEST_TEXT_RECOGNITION_LEVEL_ACCURATE: isize = 0;
/// `kCGImageAlphaLast` in the default byte order: straight R, G, B, A bytes.
const RGBA_STRAIGHT_BITMAP_INFO: u32 = 3;
const BITS_PER_COMPONENT: usize = 8;

/// A line box relative to the window: 0–1 on both axes, origin at the top-left corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NormalizedBox {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

/// One recognized line: Vision's top candidate and its box. Private user content; never log it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RecognizedLine {
    pub(crate) text: String,
    pub(crate) bounds: NormalizedBox,
}

/// Vision could not run; `code` is its `NSError` code when it reported one.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("text recognition failed (code {code:?})")]
pub(crate) struct RecognitionError {
    pub(crate) code: Option<isize>,
}

/// Recognizes text lines with the accurate level, automatic language detection, and no language
/// correction. Lines come in Vision's order; an empty result means no text was found. Requires
/// macOS 13 or later, which every captured image already implies.
pub(crate) fn recognize_text(image: &WindowImage) -> Result<Vec<RecognizedLine>, RecognitionError> {
    let failed = RecognitionError { code: None };
    let [handler_class, request_class] = vision_classes().ok_or(failed)?;
    let _pool = AutoreleasePool::new();
    let cg_image = cg_image(image).ok_or(failed)?;
    // SAFETY: Foundation is linked; `dictionary` returns an autoreleased empty dictionary.
    let options = unsafe { send_id_0(class(c"NSDictionary"), sel(c"dictionary")) };
    // SAFETY: the handler is allocated and initialized with a live CGImage, which it retains.
    let handler = unsafe {
        OwnedObject::from_retained(msg_send::<
            unsafe extern "C" fn(ObjcId, ObjcSel, CfRef, ObjcId) -> ObjcId,
        >()(
            send_id_0(handler_class as ObjcId, sel(c"alloc")),
            sel(c"initWithCGImage:options:"),
            cg_image.as_ptr(),
            options,
        ))
    }
    .ok_or(failed)?;
    // SAFETY: `new` returns a retained request; the setters take plain values.
    let request =
        unsafe { OwnedObject::from_retained(send_id_0(request_class as ObjcId, sel(c"new"))) }
            .ok_or(failed)?;
    unsafe {
        msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel, isize)>()(
            request.as_ptr(),
            sel(c"setRecognitionLevel:"),
            VN_REQUEST_TEXT_RECOGNITION_LEVEL_ACCURATE,
        );
        send_void_bool(request.as_ptr(), sel(c"setUsesLanguageCorrection:"), false);
        send_void_bool(
            request.as_ptr(),
            sel(c"setAutomaticallyDetectsLanguage:"),
            true,
        );
    }
    // SAFETY: `arrayWithObject:` returns an autoreleased array retaining the live request.
    let requests = unsafe {
        send_id_id(
            class(c"NSArray"),
            sel(c"arrayWithObject:"),
            request.as_ptr(),
        )
    };
    let mut error = ptr::null_mut();
    // SAFETY: the handler and array are live; `error` receives an autoreleased NSError or nil.
    let performed = unsafe {
        msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel, ObjcId, *mut ObjcId) -> ObjcBool>()(
            handler.as_ptr(),
            sel(c"performRequests:error:"),
            requests,
            &mut error,
        )
    };
    if performed == ObjcBool::from(false) {
        // SAFETY: the error is a live NSError or nil until the pool is drained.
        let code = unsafe { error_code(error) };
        return Err(RecognitionError { code });
    }
    // SAFETY: after a successful perform, `results` is an array of text observations.
    let results = unsafe { send_id_0(request.as_ptr(), sel(c"results")) };
    let count = unsafe { send_usize_0(results, sel(c"count")) };
    Ok((0..count)
        .filter_map(|index| unsafe {
            recognized_line(send_id_usize(results, sel(c"objectAtIndex:"), index))
        })
        .collect())
}

unsafe fn recognized_line(observation: ObjcId) -> Option<RecognizedLine> {
    let candidates = unsafe { send_id_usize(observation, sel(c"topCandidates:"), 1) };
    let candidate = unsafe { send_id_0(candidates, sel(c"firstObject")) };
    let string = unsafe { send_id_0(candidate, sel(c"string")) };
    let utf8 = msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel) -> *const c_char>();
    let bytes = NonNull::new(unsafe { utf8(string, sel(c"UTF8String")) }.cast_mut())?;
    let text = unsafe { CStr::from_ptr(bytes.as_ptr()) }.to_str().ok()?;
    // Corners rather than `boundingBox`, whose CGRect would need a struct-return call on x86_64.
    let corner = msg_send::<unsafe extern "C" fn(ObjcId, ObjcSel) -> AxPoint>();
    let top_left = unsafe { corner(observation, sel(c"topLeft")) };
    let bottom_right = unsafe { corner(observation, sel(c"bottomRight")) };
    Some(RecognizedLine {
        text: text.to_owned(),
        bounds: normalized_box(top_left, bottom_right),
    })
}

/// Converts Vision's normalized corners (bottom-left origin) to a top-left-origin box clamped to
/// the image, since Vision can place a box edge slightly outside it.
fn normalized_box(top_left: AxPoint, bottom_right: AxPoint) -> NormalizedBox {
    let left = top_left.x.min(bottom_right.x).clamp(0.0, 1.0);
    let right = top_left.x.max(bottom_right.x).clamp(0.0, 1.0);
    let top = (1.0 - top_left.y.max(bottom_right.y)).clamp(0.0, 1.0);
    let bottom = (1.0 - top_left.y.min(bottom_right.y)).clamp(0.0, 1.0);
    NormalizedBox {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

/// Wraps a copy of the pixels in a CGImage, so no reference Vision keeps can outlive the caller's
/// buffer.
fn cg_image(image: &WindowImage) -> Option<OwnedCf> {
    let rgba = &image.rgba;
    if rgba.len() != image.width * image.height * BYTES_PER_PIXEL {
        return None;
    }
    // SAFETY: CFDataCreate copies `rgba` (a Vec never exceeds isize::MAX bytes); every result
    // follows the create rule.
    let data = unsafe {
        OwnedCf::from_created(CFDataCreate(
            ptr::null(),
            rgba.as_ptr(),
            rgba.len() as isize,
        ))
    }?;
    let provider = unsafe { OwnedCf::from_created(CGDataProviderCreateWithCFData(data.as_ptr())) }?;
    let color_space = srgb_color_space()?;
    // SAFETY: the provider holds exactly `height` rows of `width * 4` bytes.
    unsafe {
        OwnedCf::from_created(CGImageCreate(
            image.width,
            image.height,
            BITS_PER_COMPONENT,
            BITS_PER_COMPONENT * BYTES_PER_PIXEL,
            image.width * BYTES_PER_PIXEL,
            color_space.as_ptr(),
            RGBA_STRAIGHT_BITMAP_INFO,
            provider.as_ptr(),
            ptr::null(),
            false,
            0,
        ))
    }
}

fn vision_classes() -> Option<[usize; 2]> {
    static CLASSES: Classes<2> = Classes::new();
    load_classes(
        &CLASSES,
        c"/System/Library/Frameworks/Vision.framework/Vision",
        [c"VNImageRequestHandler", c"VNRecognizeTextRequest"],
    )
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDataProviderCreateWithCFData(data: CfRef) -> CfRef;
    fn CGImageCreate(
        width: usize,
        height: usize,
        bits_per_component: usize,
        bits_per_pixel: usize,
        bytes_per_row: usize,
        color_space: CfRef,
        bitmap_info: u32,
        provider: CfRef,
        decode: *const f64,
        should_interpolate: bool,
        intent: i32,
    ) -> CfRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: isize) -> CfRef;
}

#[cfg(test)]
mod tests;
