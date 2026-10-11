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
use crate::ffi::{screen_capture::WindowImage, text_recognition::RecognizedLine};

/// Builds the snapshot body from the lines recognized in `image`.
///
/// Returns `None` when every line lies in an input field, so the caller records
/// the Accessibility body instead of an empty recognized one.
pub(crate) fn recognized_snapshot(
    image: &WindowImage,
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
