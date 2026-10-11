//! The snapshot body from ordered lines, within the snapshot size limits.

use zanei_core::schema::{
    ContentSnapshotCutoff, ContentSnapshotData, ContentSnapshotLine, ContentSnapshotTrigger,
    WindowFraction,
};

use crate::content_snapshot::budget::SNAPSHOT_TEXT_LIMIT_BYTES;

// Design limit: one snapshot's `lines` are at most 64 KiB of compact JSON,
// about 590 lines at the ~110 bytes measured per line.
const LINES_LIMIT_BYTES: usize = 64 * 1_024;

/// Joins `lines` with `\n` into `text`, adding whole lines until the next one
/// would exceed the text or the `lines` limit, so both stop at the same line.
pub(super) fn assemble(
    lines: Vec<ContentSnapshotLine>,
    trigger: ContentSnapshotTrigger,
) -> ContentSnapshotData {
    let mut text = String::new();
    let mut lines_bytes = "[]".len();
    let mut kept = Vec::with_capacity(lines.len());
    let mut cutoff = None;
    for line in lines {
        // One `\n` in `text` and one `,` in the `lines` array.
        let separator = usize::from(!kept.is_empty());
        let next_lines_bytes = lines_bytes + separator + serialized_len(&line);
        if text.len() + separator + line.text.len() > SNAPSHOT_TEXT_LIMIT_BYTES
            || next_lines_bytes > LINES_LIMIT_BYTES
        {
            cutoff = Some(ContentSnapshotCutoff::Bytes);
            break;
        }
        if separator == 1 {
            text.push('\n');
        }
        text.push_str(&line.text);
        lines_bytes = next_lines_bytes;
        kept.push(line);
    }
    let chars =
        u64::try_from(text.chars().count()).expect("the 32 KiB design limit always fits in u64");
    ContentSnapshotData::recognized(Some(text), chars, cutoff, trigger, kept)
}

/// Length of `line` as compact JSON, exactly as the event serializer writes it,
/// so the core size safety rule never removes `lines` that fit this limit.
fn serialized_len(line: &ContentSnapshotLine) -> usize {
    const FIELDS: &str = r##"{"text":"","x":,"y":,"w":,"h":,"bg":"#RRGGBB","ink":}"##;
    let ink = if line.ink.is_some() {
        r##""#RRGGBB""##.len()
    } else {
        "null".len()
    };
    FIELDS.len()
        + escaped_len(&line.text)
        + [line.x, line.y, line.w, line.h]
            .into_iter()
            .map(fraction_len)
            .sum::<usize>()
        + ink
}

/// JSON escapes quotes, backslashes, and control characters; other UTF-8 stays as is.
fn escaped_len(text: &str) -> usize {
    text.bytes()
        .map(|byte| match byte {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0C => 2,
            0x00..=0x1F => "\\u0000".len(),
            _ => 1,
        })
        .sum()
}

/// Fractions are written as their shortest decimal: `0.0`, `1.0`, or `0.`
/// followed by up to four digits without trailing zeros.
fn fraction_len(fraction: WindowFraction) -> usize {
    let mut ten_thousandths = (fraction.ratio() * 10_000.0).round() as u32;
    if ten_thousandths.is_multiple_of(10_000) {
        return "0.0".len();
    }
    let mut digits = 4;
    while ten_thousandths.is_multiple_of(10) {
        ten_thousandths /= 10;
        digits -= 1;
    }
    "0.".len() + digits
}
