//! Turns the lines recognized in one window image into the body of a
//! `macos.ocr` snapshot: input-field masking, line colours, reading order, and
//! the snapshot size limits. Pure; the caller owns capture and recognition.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "called by the text-recognition fallback")
)]

mod assemble;
mod layout;
mod order;

use zanei_core::schema::{ContentSnapshotData, ContentSnapshotTrigger};

use super::coverage::WindowRect;

/// One captured window image. Private user content: never log or persist it.
pub(crate) struct RgbaImage<'a> {
    pub(crate) width: usize,
    pub(crate) height: usize,
    /// Image pixels per window point.
    pub(crate) scale: f64,
    /// sRGB with straight alpha, rows top to bottom without padding;
    /// `width * height * 4` bytes.
    pub(crate) rgba: &'a [u8],
}

/// One recognized line and its box, relative to the window with a top-left origin.
pub(crate) struct RecognizedLine {
    pub(crate) text: String,
    pub(crate) bounds: WindowRect,
}

/// Builds the snapshot body from the lines recognized in `image`.
///
/// Returns `None` when every line lies in an input field, so the caller records
/// the Accessibility body instead of an empty recognized one.
pub(crate) fn recognized_snapshot(
    image: &RgbaImage<'_>,
    lines: Vec<RecognizedLine>,
    mask_regions: &[WindowRect],
    order_containers: &[WindowRect],
    trigger: ContentSnapshotTrigger,
) -> Option<ContentSnapshotData> {
    let lines: Vec<_> = order::unmasked(lines, mask_regions)
        .map(|line| layout::lay_out(image, line))
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(assemble::assemble(
        order::reading_order(lines, order_containers),
        trigger,
    ))
}

#[cfg(test)]
#[path = "../tests/ocr.rs"]
mod tests;
