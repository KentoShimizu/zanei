use zanei_core::schema::{
    ContentSnapshotCutoff, ContentSnapshotData, ContentSnapshotLine, ContentSnapshotTrigger,
    RgbColor, WindowFraction,
};

use super::{
    RecognizedLine, RgbaImage, WindowRect, assemble::assemble, layout::lay_out, recognized_snapshot,
};

const TRIGGER: ContentSnapshotTrigger = ContentSnapshotTrigger::Settle;
const WINDOW: [u8; 4] = [0x2D, 0x2E, 0x30, 0xFF];
const WHITE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

fn rect(x: f64, y: f64, width: f64, height: f64) -> WindowRect {
    WindowRect {
        x,
        y,
        width,
        height,
    }
}

fn line(text: &str, x: f64, y: f64, width: f64, height: f64) -> RecognizedLine {
    RecognizedLine {
        text: text.to_owned(),
        bounds: rect(x, y, width, height),
    }
}

fn fraction(ratio: f64) -> WindowFraction {
    WindowFraction::from_ratio(ratio).expect("test fraction")
}

fn colour([red, green, blue, _]: [u8; 4]) -> RgbColor {
    RgbColor { red, green, blue }
}

fn stored(
    text: &str,
    [x, y, w, h]: [f64; 4],
    bg: [u8; 4],
    ink: Option<[u8; 4]>,
) -> ContentSnapshotLine {
    ContentSnapshotLine {
        text: text.to_owned(),
        x: fraction(x),
        y: fraction(y),
        w: fraction(w),
        h: fraction(h),
        bg: colour(bg),
        ink: ink.map(colour),
    }
}

/// Straight-alpha RGBA rows, top row first.
fn paint(width: usize, height: usize, pixel: impl Fn(usize, usize) -> [u8; 4]) -> Vec<u8> {
    (0..height)
        .flat_map(|row| (0..width).map(move |column| (column, row)))
        .flat_map(|(column, row)| pixel(column, row))
        .collect()
}

fn image(width: usize, height: usize, scale: f64, rgba: &[u8]) -> RgbaImage<'_> {
    RgbaImage {
        width,
        height,
        scale,
        rgba,
    }
}

fn inside(
    column: usize,
    row: usize,
    columns: std::ops::Range<usize>,
    rows: std::ops::Range<usize>,
) -> bool {
    columns.contains(&column) && rows.contains(&row)
}

fn snapshot_text(lines: Vec<RecognizedLine>, containers: &[WindowRect]) -> String {
    let rgba = paint(64, 64, |_, _| WHITE);
    recognized_snapshot(&image(64, 64, 1.0, &rgba), lines, &[], containers, TRIGGER)
        .expect("unmasked lines")
        .text
        .expect("text")
}

#[test]
fn background_is_the_dominant_ring_colour_and_ink_the_glyph_colour() {
    // Catches sampling `bg` inside the box (glyphs cover most of it), a plain
    // mean or an unquantised mode over the ring (a red edge and two alternating
    // bubble shades), a vertical flip (the lower half is window background), a
    // BGRA or premultiplied-alpha misread (translucent bubble with R != B).
    let rgba = paint(128, 128, |column, row| {
        if inside(column, row, 32..96, 36..46) {
            [0x11, 0x11, 0x11, 0xFF]
        } else if inside(column, row, 27..101, 27..29) {
            [0xFF, 0x00, 0x00, 0xFF]
        } else if inside(column, row, 16..112, 16..64) {
            if (column + row) % 2 == 0 {
                [0x64, 0xD8, 0x6A, 0x80]
            } else {
                [0x68, 0xDC, 0x6E, 0x80]
            }
        } else {
            WINDOW
        }
    });

    let laid_out = lay_out(
        &image(128, 128, 1.0, &rgba),
        line("hello", 0.25, 0.25, 0.5, 0.125),
    );

    assert_eq!(
        laid_out,
        stored(
            "hello",
            [0.25, 0.25, 0.5, 0.125],
            [0x66, 0xDA, 0x6C, 0xFF],
            Some([0x11, 0x11, 0x11, 0xFF]),
        )
    );
}

#[test]
fn ink_needs_a_colour_distance_above_eighty() {
    // Catches an inclusive or misplaced ink threshold: a distance of exactly
    // 80 is not ink, 81 is.
    let rgba = paint(80, 20, |column, row| match (column, row) {
        (10..30, 10) => [0x2D + 80, 0x2E, 0x30, 0xFF],
        (50..70, 10) => [0x2D + 81, 0x2E, 0x30, 0xFF],
        _ => WINDOW,
    });
    let image = image(80, 20, 1.0, &rgba);

    let at_threshold = lay_out(&image, line("a", 0.125, 0.25, 0.25, 0.5));
    let above = lay_out(&image, line("b", 0.625, 0.25, 0.25, 0.5));

    assert_eq!((at_threshold.bg, at_threshold.ink), (colour(WINDOW), None));
    assert_eq!(
        (above.bg, above.ink),
        (colour(WINDOW), Some(colour([0x2D + 81, 0x2E, 0x30, 0xFF])))
    );
}

#[test]
fn background_ring_is_five_points_at_the_image_scale() {
    // Catches a ring fixed in pixels: at 2 pixels per point the ring is 10
    // pixels wide, where an outer colour outweighs a 4-pixel inner band.
    let rgba = paint(64, 64, |column, row| {
        if inside(column, row, 16..48, 28..36) {
            WINDOW
        } else if inside(column, row, 12..52, 24..40) {
            WHITE
        } else {
            [0x00, 0x00, 0x80, 0xFF]
        }
    });

    let laid_out = lay_out(
        &image(64, 64, 2.0, &rgba),
        line("a", 0.25, 0.4375, 0.5, 0.125),
    );

    assert_eq!(laid_out.bg, colour([0x00, 0x00, 0x80, 0xFF]));
}

#[test]
fn a_line_filling_the_window_takes_its_background_from_inside() {
    // Catches a panic or a missing colour when the ring lies wholly outside
    // the image.
    let rgba = paint(8, 4, |column, row| {
        if row == 1 && column < 3 {
            WHITE
        } else {
            WINDOW
        }
    });

    let laid_out = lay_out(&image(8, 4, 1.0, &rgba), line("a", 0.0, 0.0, 1.0, 1.0));

    assert_eq!(
        (laid_out.bg, laid_out.ink),
        (colour(WINDOW), Some(colour(WHITE)))
    );
}

#[test]
fn lines_centred_in_input_fields_are_dropped_from_text_and_lines() {
    // Catches a vertical flip of the field (the mirrored line at the bottom
    // would be dropped instead), masking by overlap rather than by centre, and
    // masking only one of the two stored copies.
    let rgba = paint(64, 64, |_, _| WHITE);
    let field = rect(0.1, 0.05, 0.4, 0.05);
    let lines = vec![
        line("typed query", 0.12, 0.06, 0.2, 0.03),
        line("下の行", 0.12, 0.91, 0.2, 0.03),
        line("beside", 0.45, 0.06, 0.2, 0.03),
    ];

    let snapshot = recognized_snapshot(&image(64, 64, 1.0, &rgba), lines, &[field], &[], TRIGGER);

    assert_eq!(
        snapshot,
        Some(ContentSnapshotData::recognized(
            Some("beside\n下の行".to_owned()),
            10,
            None,
            TRIGGER,
            vec![
                stored("beside", [0.45, 0.06, 0.2, 0.03], WHITE, None),
                stored("下の行", [0.12, 0.91, 0.2, 0.03], WHITE, None),
            ],
        ))
    );
}

#[test]
fn a_fully_masked_window_yields_no_recognized_snapshot() {
    // Catches emitting an empty recognized body instead of letting the caller
    // record the Accessibility body.
    let rgba = paint(64, 64, |_, _| WHITE);

    let snapshot = recognized_snapshot(
        &image(64, 64, 1.0, &rgba),
        vec![line("typed query", 0.12, 0.06, 0.2, 0.03)],
        &[rect(0.1, 0.05, 0.4, 0.05)],
        &[],
        TRIGGER,
    );

    assert_eq!(snapshot, None);
}

#[test]
fn chat_window_reads_unowned_lines_then_regions_left_to_right() {
    // Catches containers taken in walk order (the composer is walked first),
    // regions ordered top edge first (the sidebar starts lower than the chat
    // log), the enclosing split group capturing every line instead of the
    // smallest container, unowned lines read last, and assigning a line by its
    // box start rather than its centre (the wide line starts in the sidebar).
    let split_group = rect(0.0, 0.06, 1.0, 0.9);
    let composer = rect(0.3, 0.86, 0.7, 0.1);
    let sidebar = rect(0.0, 0.1, 0.3, 0.86);
    let chat_log = rect(0.3, 0.06, 0.7, 0.8);
    let lines = vec![
        line("status", 0.4, 0.97, 0.2, 0.02),
        line("draft", 0.32, 0.9, 0.3, 0.03),
        line("own 2", 0.7, 0.35, 0.25, 0.03),
        line("own 1", 0.7, 0.15, 0.25, 0.03),
        line("wide", 0.25, 0.45, 0.4, 0.03),
        line("other 1", 0.35, 0.25, 0.25, 0.03),
        line("Bob", 0.02, 0.2, 0.2, 0.03),
        line("Alice", 0.02, 0.12, 0.2, 0.03),
        line("title", 0.4, 0.01, 0.2, 0.03),
    ];

    let text = snapshot_text(lines, &[split_group, composer, sidebar, chat_log]);

    assert_eq!(
        text,
        "title\nstatus\nAlice\nBob\nown 1\nother 1\nown 2\nwide\ndraft"
    );
}

#[test]
fn window_without_containers_reads_top_to_bottom_then_left_to_right() {
    // Catches column-grouped recognizer output leaking through, and lines
    // stored at the same height (four decimals) not reading left to right.
    let lines = vec![
        line("left 1", 0.05, 0.1, 0.3, 0.03),
        line("left 2", 0.05, 0.43214, 0.3, 0.03),
        line("right 1", 0.6, 0.2, 0.3, 0.03),
        line("right 2", 0.6, 0.43211, 0.3, 0.03),
    ];

    let text = snapshot_text(lines, &[]);

    assert_eq!(text, "left 1\nright 1\nleft 2\nright 2");
}

#[test]
fn lines_limit_cuts_text_and_lines_at_the_same_line() {
    // Catches `text` and `lines` diverging at the cutoff, and a serialized
    // size that undercounts JSON escapes, number digits, or `ink`, which would
    // let the core size safety rule remove every line.
    let quoted = stored(
        "say \"hi\"\\\u{1}é",
        [0.0001, 0.25, 1.0, 0.0],
        [0, 0, 0, 0xFF],
        None,
    );
    let quoted_json = r##"{"text":"say \"hi\"\\\u0001é","x":0.0001,"y":0.25,"w":1.0,"h":0.0,"bg":"#000000","ink":null}"##;
    let short = stored(
        "b",
        [0.5, 0.125, 0.0625, 0.0313],
        WHITE,
        Some([0xAB, 0xCD, 0xEF, 0xFF]),
    );
    let short_json =
        r##"{"text":"b","x":0.5,"y":0.125,"w":0.0625,"h":0.0313,"bg":"#FFFFFF","ink":"#ABCDEF"}"##;
    let candidates: Vec<_> = (0..2_000)
        .map(|index| {
            if index % 2 == 0 {
                (quoted.clone(), quoted_json)
            } else {
                (short.clone(), short_json)
            }
        })
        .collect();
    let mut array_bytes = "[]".len() - ",".len();
    let fitting = candidates
        .iter()
        .take_while(|(_, json)| {
            array_bytes += ",".len() + json.len();
            array_bytes <= 64 * 1_024
        })
        .count();
    let kept: Vec<_> = candidates[..fitting]
        .iter()
        .map(|(line, _)| line.clone())
        .collect();
    let text = kept
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let snapshot = assemble(
        candidates.into_iter().map(|(line, _)| line).collect(),
        TRIGGER,
    );

    assert!(fitting < 2_000 && text.len() < 32 * 1_024);
    assert_eq!(
        snapshot,
        ContentSnapshotData::recognized(
            Some(text.clone()),
            text.chars().count() as u64,
            Some(ContentSnapshotCutoff::Bytes),
            TRIGGER,
            kept,
        )
    );
}

#[test]
fn text_limit_stops_at_a_whole_line_on_a_utf8_boundary() {
    // Catches truncating the text mid-line (the 32 KiB boundary falls inside
    // a three-byte character) and keeping a line in `lines` that `text` lost.
    let wide = "あ".repeat(300);
    let line = stored(&wide, [0.1, 0.1, 0.8, 0.02], WHITE, Some([0, 0, 0, 0xFF]));
    let lines = vec![line; 40];
    let kept = 36; // 36 lines of 900 bytes and 35 newlines fit; 37 do not.
    let text = vec![wide.as_str(); kept].join("\n");

    let snapshot = assemble(lines.clone(), TRIGGER);

    assert_eq!(
        snapshot,
        ContentSnapshotData::recognized(
            Some(text),
            36 * 300 + 35,
            Some(ContentSnapshotCutoff::Bytes),
            TRIGGER,
            lines[..kept].to_vec(),
        )
    );
}
