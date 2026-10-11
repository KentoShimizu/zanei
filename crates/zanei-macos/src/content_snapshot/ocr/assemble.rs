//! The snapshot body from ordered lines, within the snapshot size limits.

use zanei_core::schema::{
    ContentSnapshotCutoff, ContentSnapshotData, ContentSnapshotLine, ContentSnapshotTrigger,
};

use crate::content_snapshot::budget::{SNAPSHOT_LINES_LIMIT_BYTES, SNAPSHOT_TEXT_LIMIT_BYTES};

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
        let next_lines_bytes = lines_bytes
            + separator
            + serde_json::to_vec(&line)
                .expect("content snapshot lines serialize to JSON")
                .len();
        if text.len() + separator + line.text.len() > SNAPSHOT_TEXT_LIMIT_BYTES
            || next_lines_bytes > SNAPSHOT_LINES_LIMIT_BYTES
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
