//! Input-field masking and the reading order of recognized lines.

use zanei_core::schema::ContentSnapshotLine;

use super::{RecognizedLine, WindowRect};

/// Drops lines whose box centre lies in a secure or single-line input field, so
/// recognition never stores what Accessibility capture would not read.
pub(super) fn unmasked(
    lines: Vec<RecognizedLine>,
    mask_regions: &[WindowRect],
) -> impl Iterator<Item = RecognizedLine> {
    lines.into_iter().filter(move |line| {
        let centre = centre(
            line.bounds.x,
            line.bounds.y,
            line.bounds.width,
            line.bounds.height,
        );
        !mask_regions.iter().any(|region| contains(*region, centre))
    })
}

/// Orders lines by region, then top to bottom, then left to right.
///
/// Each line belongs to the smallest container holding its box centre. Lines in
/// no container come first; containers follow by left edge, then top edge.
/// Positions compare as stored, at four decimals, so lines that `lines` reports
/// at the same height read left to right.
pub(super) fn reading_order(
    lines: Vec<ContentSnapshotLine>,
    containers: &[WindowRect],
) -> Vec<ContentSnapshotLine> {
    let mut regions: Vec<WindowRect> = containers.to_vec();
    // Stable, so containers at the same position keep their walk order.
    regions.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    let mut keyed: Vec<_> = lines
        .into_iter()
        .map(|line| {
            let centre = centre(
                line.x.ratio(),
                line.y.ratio(),
                line.w.ratio(),
                line.h.ratio(),
            );
            (region(&regions, centre), line)
        })
        .collect();
    keyed.sort_by(|(region_a, a), (region_b, b)| {
        region_a
            .cmp(region_b)
            .then(a.y.ratio().total_cmp(&b.y.ratio()))
            .then(a.x.ratio().total_cmp(&b.x.ratio()))
    });
    keyed.into_iter().map(|(_, line)| line).collect()
}

/// Reading position of the smallest region holding `point`; `None`, which sorts
/// first, when no region holds it.
fn region(regions: &[WindowRect], point: (f64, f64)) -> Option<usize> {
    regions
        .iter()
        .enumerate()
        .filter(|(_, region)| contains(**region, point))
        .min_by(|(_, a), (_, b)| a.area().total_cmp(&b.area()))
        .map(|(position, _)| position)
}

fn centre(x: f64, y: f64, width: f64, height: f64) -> (f64, f64) {
    (x + width / 2.0, y + height / 2.0)
}

fn contains(rect: WindowRect, (x, y): (f64, f64)) -> bool {
    rect.x <= x && x < rect.x + rect.width && rect.y <= y && y < rect.y + rect.height
}
