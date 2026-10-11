//! Pure analysis of one Accessibility walk: how much of the window shows
//! content that Accessibility does not expose (blank coverage), where input
//! fields are (mask regions), and which large elements partition the window
//! (order containers).

use crate::{content_snapshot::SnapshotNodeClass, ffi::ax::AxFrame};

/// Blank coverage is sampled on this many columns and rows of the window.
const COVERAGE_GRID: usize = 48;
const COVERAGE_CELLS: usize = COVERAGE_GRID * COVERAGE_GRID;
/// Accessibility is insufficient when at least a quarter of the window is
/// occupied by elements that exist to present content but expose none.
const BLANK_COVERAGE_MIN: f64 = 0.25;
/// Order containers cover at least 3 % of the window area.
const ORDER_CONTAINER_MIN_AREA: f64 = 0.03;
/// Roles that repeat per item or per column; as containers they would split
/// one list or table into many reading regions.
const ORDER_CONTAINER_EXCLUDED_ROLES: [&str; 5] =
    ["AXRow", "AXCell", "AXStaticText", "AXButton", "AXColumn"];

/// Rectangle relative to the walked window. `x` and `width` are fractions of
/// the window width, `y` and `height` fractions of the window height, with the
/// origin at the window's top-left corner and `y` growing downward. Every
/// value lies in 0–1 because rectangles are clipped to the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct WindowRect {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl WindowRect {
    /// Clips an Accessibility frame to the window frame. Both frames are in
    /// global screen points with a top-left origin. Returns `None` when the
    /// overlap has no area.
    pub(crate) fn within(frame: AxFrame, window: AxFrame) -> Option<Self> {
        let left = frame.origin.x.max(window.origin.x);
        let top = frame.origin.y.max(window.origin.y);
        let right = (frame.origin.x + frame.size.width).min(window.origin.x + window.size.width);
        let bottom = (frame.origin.y + frame.size.height).min(window.origin.y + window.size.height);
        if right <= left || bottom <= top {
            return None;
        }
        Some(Self {
            x: (left - window.origin.x) / window.size.width,
            y: (top - window.origin.y) / window.size.height,
            width: (right - left) / window.size.width,
            height: (bottom - top) / window.size.height,
        })
    }

    pub(crate) fn area(self) -> f64 {
        self.width * self.height
    }

    /// Half-open range of grid indexes whose cell centres lie in
    /// `[start, start + length)`.
    fn cells(start: f64, length: f64) -> std::ops::Range<usize> {
        let grid = COVERAGE_GRID as f64;
        let index = |edge: f64| ((edge * grid - 0.5).ceil().max(0.0) as usize).min(COVERAGE_GRID);
        index(start)..index(start + length)
    }
}

/// What the walk observed about one node whose frame overlaps the window.
pub(crate) struct WalkedNode<'a> {
    pub(crate) role: Option<&'a str>,
    pub(crate) class: SnapshotNodeClass,
    pub(crate) rect: WindowRect,
    pub(crate) is_root: bool,
    pub(crate) contributed_text: bool,
    pub(crate) degraded: bool,
    /// `None` when the walk did not read the child count or the read failed.
    pub(crate) children: Option<usize>,
}

impl WalkedNode<'_> {
    /// True when everything except the child count already qualifies the node
    /// as a blank slot, so the walk knows when a child count is worth reading.
    pub(crate) fn may_be_blank_slot(&self) -> bool {
        !self.contributed_text
            && !self.degraded
            && match self.class {
                SnapshotNodeClass::ReadableText | SnapshotNodeClass::MultiLineText => true,
                SnapshotNodeClass::Container => self.role == Some("AXRow"),
                _ => false,
            }
    }

    fn is_blank_slot(&self) -> bool {
        self.may_be_blank_slot() && self.children == Some(0)
    }

    fn is_text_region(&self) -> bool {
        self.contributed_text && self.class != SnapshotNodeClass::Container
    }

    fn is_mask_region(&self) -> bool {
        matches!(
            self.class,
            SnapshotNodeClass::SecureInput | SnapshotNodeClass::SingleLineInput
        )
    }

    fn is_order_container(&self) -> bool {
        !self.is_root
            && self.rect.area() >= ORDER_CONTAINER_MIN_AREA
            && !self
                .role
                .is_some_and(|role| ORDER_CONTAINER_EXCLUDED_ROLES.contains(&role))
    }
}

/// Analysis of one walk, for deciding on and post-processing text recognition.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WalkAnalysis {
    /// Fraction (0–1) of the window covered by blank slots and not by text
    /// regions, sampled at the centres of a `COVERAGE_GRID` square grid.
    pub(crate) blank_coverage: f64,
    /// Secure and single-line input fields, in walk order.
    pub(crate) mask_regions: Vec<WindowRect>,
    /// Large non-root elements that partition the window, in walk order.
    pub(crate) order_containers: Vec<WindowRect>,
}

impl WalkAnalysis {
    pub(crate) fn ax_insufficient(&self) -> bool {
        self.blank_coverage >= BLANK_COVERAGE_MIN
    }
}

/// Accumulates `WalkAnalysis` while the walk visits nodes.
pub(crate) struct WalkAnalyzer {
    blank_cells: [bool; COVERAGE_CELLS],
    text_cells: [bool; COVERAGE_CELLS],
    mask_regions: Vec<WindowRect>,
    order_containers: Vec<WindowRect>,
}

impl WalkAnalyzer {
    pub(crate) fn new() -> Self {
        Self {
            blank_cells: [false; COVERAGE_CELLS],
            text_cells: [false; COVERAGE_CELLS],
            mask_regions: Vec::new(),
            order_containers: Vec::new(),
        }
    }

    pub(crate) fn record(&mut self, node: &WalkedNode<'_>) {
        if node.is_blank_slot() {
            mark(&mut self.blank_cells, node.rect);
        }
        if node.is_text_region() {
            mark(&mut self.text_cells, node.rect);
        }
        if node.is_mask_region() {
            self.mask_regions.push(node.rect);
        }
        if node.is_order_container() {
            self.order_containers.push(node.rect);
        }
    }

    pub(crate) fn finish(self) -> WalkAnalysis {
        let blank = self
            .blank_cells
            .iter()
            .zip(self.text_cells.iter())
            .filter(|(blank, text)| **blank && !**text)
            .count();
        WalkAnalysis {
            blank_coverage: blank as f64 / COVERAGE_CELLS as f64,
            mask_regions: self.mask_regions,
            order_containers: self.order_containers,
        }
    }
}

fn mark(cells: &mut [bool; COVERAGE_CELLS], rect: WindowRect) {
    for row in WindowRect::cells(rect.y, rect.height) {
        for column in WindowRect::cells(rect.x, rect.width) {
            cells[row * COVERAGE_GRID + column] = true;
        }
    }
}

#[cfg(test)]
#[path = "tests/coverage.rs"]
mod tests;
