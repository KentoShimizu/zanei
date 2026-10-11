//! A recognized line's stored box and colours, sampled from the window image.

use std::ops::Range;

use zanei_core::schema::{ContentSnapshotLine, RgbColor, WindowFraction};

use super::{RecognizedLine, WindowImage};

/// `bg` is sampled on a ring this many window points wide around the line box.
const RING_POINTS: f64 = 5.0;
/// A pixel inside the box is ink when its RGB distance from `bg` exceeds 80.
const INK_MIN_DISTANCE_SQUARED: u32 = 80 * 80;
/// Keeping each channel's high four bits quantises it to 16 levels.
const LEVEL_SHIFT: u32 = 4;
const BUCKETS: usize = 1 << (3 * (8 - LEVEL_SHIFT));

pub(super) fn lay_out(image: &WindowImage, line: RecognizedLine) -> ContentSnapshotLine {
    let RecognizedLine { text, bounds } = line;
    let columns = span(bounds.x, bounds.width, image.width);
    let rows = span(bounds.y, bounds.height, image.height);
    let ring = (RING_POINTS * image.scale).round() as usize;
    let around_columns = columns.start.saturating_sub(ring)..(columns.end + ring).min(image.width);
    let around_rows = rows.start.saturating_sub(ring)..(rows.end + ring).min(image.height);

    let mut ring_colours = Histogram::new();
    for row in around_rows {
        for column in around_columns.clone() {
            if !(columns.contains(&column) && rows.contains(&row)) {
                ring_colours.add(pixel(image, column, row));
            }
        }
    }
    let bg = ring_colours.dominant().unwrap_or_else(|| {
        // A line filling the whole image has no ring; the most common colour
        // within it is still its background, as glyphs cover less of a box.
        let mut inside = Histogram::new();
        pixels(image, &columns, &rows).for_each(|rgb| inside.add(rgb));
        inside
            .dominant()
            .expect("an image with a recognized line has a pixel inside or around it")
    });

    let mut ink = Bucket::default();
    for rgb in pixels(image, &columns, &rows) {
        if distance_squared(rgb, bg) > INK_MIN_DISTANCE_SQUARED {
            ink.add(rgb);
        }
    }

    ContentSnapshotLine {
        text,
        x: fraction(bounds.x),
        y: fraction(bounds.y),
        w: fraction(bounds.width),
        h: fraction(bounds.height),
        bg,
        ink: ink.mean(),
    }
}

/// Pixel indexes covered by a window-relative extent along one image axis.
fn span(start: f64, length: f64, pixels: usize) -> Range<usize> {
    let size = pixels as f64;
    // Clamped because `start + length` can round past 1.
    let index = |edge: f64| edge.clamp(0.0, size) as usize;
    index((start * size).floor())..index(((start + length) * size).ceil())
}

fn pixels<'a>(
    image: &'a WindowImage,
    columns: &Range<usize>,
    rows: &Range<usize>,
) -> impl Iterator<Item = [u8; 3]> + 'a {
    let columns = columns.clone();
    rows.clone()
        .flat_map(move |row| columns.clone().map(move |column| pixel(image, column, row)))
}

/// Straight alpha: the colour channels already hold the colour, so alpha is not applied.
fn pixel(image: &WindowImage, column: usize, row: usize) -> [u8; 3] {
    let offset = (row * image.width + column) * 4;
    [
        image.rgba[offset],
        image.rgba[offset + 1],
        image.rgba[offset + 2],
    ]
}

fn distance_squared(rgb: [u8; 3], colour: RgbColor) -> u32 {
    [colour.red, colour.green, colour.blue]
        .into_iter()
        .zip(rgb)
        .map(|(a, b)| u32::from(a.abs_diff(b)).pow(2))
        .sum()
}

fn fraction(ratio: f64) -> WindowFraction {
    WindowFraction::from_ratio(ratio).expect("recognized line boxes lie within the window")
}

/// The mean colour of the most common quantised colour.
struct Histogram(Vec<Bucket>);

impl Histogram {
    fn new() -> Self {
        Self(vec![Bucket::default(); BUCKETS])
    }

    fn add(&mut self, rgb: [u8; 3]) {
        let index = rgb.into_iter().fold(0, |index, channel| {
            (index << (8 - LEVEL_SHIFT)) | usize::from(channel >> LEVEL_SHIFT)
        });
        self.0[index].add(rgb);
    }

    fn dominant(&self) -> Option<RgbColor> {
        self.0
            .iter()
            .max_by_key(|bucket| bucket.count)
            .and_then(Bucket::mean)
    }
}

#[derive(Clone, Copy, Default)]
struct Bucket {
    count: u64,
    sums: [u64; 3],
}

impl Bucket {
    fn add(&mut self, rgb: [u8; 3]) {
        self.count += 1;
        for (sum, channel) in self.sums.iter_mut().zip(rgb) {
            *sum += u64::from(channel);
        }
    }

    fn mean(&self) -> Option<RgbColor> {
        (self.count > 0).then(|| {
            // A rounded mean of u8 channels fits in u8.
            let [red, green, blue] = self
                .sums
                .map(|sum| ((sum + self.count / 2) / self.count) as u8);
            RgbColor { red, green, blue }
        })
    }
}
