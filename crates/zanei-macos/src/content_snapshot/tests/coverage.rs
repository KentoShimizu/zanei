use super::*;
use crate::content_snapshot::{AxPoint, AxSize, SnapshotNodeClass::Container, classify_role};

fn ax_frame(x: f64, y: f64, width: f64, height: f64) -> AxFrame {
    AxFrame {
        origin: AxPoint { x, y },
        size: AxSize { width, height },
    }
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> WindowRect {
    WindowRect {
        x,
        y,
        width,
        height,
    }
}

fn blank(role: &str, rect: WindowRect) -> WalkedNode<'_> {
    WalkedNode {
        role: Some(role),
        class: classify_role(Some(role), None),
        rect,
        is_root: false,
        contributed_text: false,
        degraded: false,
        children: Some(0),
    }
}

fn text(role: &str, rect: WindowRect) -> WalkedNode<'_> {
    WalkedNode {
        contributed_text: true,
        ..blank(role, rect)
    }
}

fn analyze(nodes: &[WalkedNode<'_>]) -> WalkAnalysis {
    let mut analyzer = WalkAnalyzer::new();
    for node in nodes {
        analyzer.record(node);
    }
    analyzer.finish()
}

#[test]
fn global_top_left_frames_become_clipped_window_fractions() {
    // Catches a bottom-left flip or a missing window offset: the window is
    // offset and the field sits near its top-left corner, not its centre.
    let window = ax_frame(200.0, 100.0, 400.0, 200.0);
    assert_eq!(
        WindowRect::within(ax_frame(250.0, 150.0, 100.0, 20.0), window),
        Some(rect(0.125, 0.25, 0.25, 0.1))
    );
    // Catches unclipped frames, which would push coverage above 1.
    assert_eq!(
        WindowRect::within(ax_frame(150.0, 250.0, 100.0, 100.0), window),
        Some(rect(0.0, 0.75, 0.125, 0.25))
    );
    assert_eq!(
        WindowRect::within(ax_frame(0.0, 0.0, 10_000.0, 10_000.0), window),
        Some(rect(0.0, 0.0, 1.0, 1.0))
    );
    assert_eq!(
        WindowRect::within(ax_frame(600.0, 100.0, 50.0, 50.0), window),
        None,
        "a frame touching only the right edge has no area in the window"
    );
}

#[test]
fn coverage_is_the_union_of_blank_slots_minus_text_regions() {
    // Two overlapping blank slots cover 0.75 of the window; a text region
    // over the first quarter leaves 0.5. Summing areas would give 1.0, and
    // letting blank win over text would give 0.75.
    let analysis = analyze(&[
        blank("AXStaticText", rect(0.0, 0.0, 0.5, 1.0)),
        blank("AXRow", rect(0.25, 0.0, 0.5, 1.0)),
        text("AXStaticText", rect(0.0, 0.0, 0.25, 1.0)),
    ]);
    assert_eq!(analysis.blank_coverage, 0.5);

    // Text recorded before the blank slot still wins.
    let analysis = analyze(&[
        text("AXStaticText", rect(0.0, 0.0, 1.0, 0.5)),
        blank("AXCell", rect(0.0, 0.0, 1.0, 1.0)),
    ]);
    assert_eq!(analysis.blank_coverage, 0.5);

    // Many full-window blank slots never exceed the whole window.
    let full = rect(0.0, 0.0, 1.0, 1.0);
    let analysis = analyze(&[blank("AXStaticText", full), blank("AXTextArea", full)]);
    assert_eq!(analysis.blank_coverage, 1.0);
}

#[test]
fn insufficiency_starts_at_a_quarter_of_the_window() {
    // Twelve of 48 grid rows are exactly a quarter; eleven are not.
    let quarter = analyze(&[blank("AXStaticText", rect(0.0, 0.0, 1.0, 0.25))]);
    assert_eq!(quarter.blank_coverage, 0.25);
    assert!(quarter.ax_insufficient());
    let less = analyze(&[blank("AXStaticText", rect(0.0, 0.0, 1.0, 11.0 / 48.0))]);
    assert!(!less.ax_insufficient());
}

#[test]
fn only_childless_textless_content_roles_are_blank_slots() {
    let full = rect(0.0, 0.0, 1.0, 1.0);
    let with_children = WalkedNode {
        children: Some(2),
        ..blank("AXRow", full)
    };
    let unread_children = WalkedNode {
        children: None,
        ..blank("AXStaticText", full)
    };
    let degraded = WalkedNode {
        degraded: true,
        ..blank("AXCell", full)
    };
    let not_blank = [
        blank("AXGroup", full),
        blank("AXWebArea", full),
        blank("AXFutureControl", full),
        blank("AXImage", full),
        blank("AXSecureTextField", full),
        blank("AXTextField", full),
        blank("AXMenu", full),
        with_children,
        unread_children,
        degraded,
    ];
    // Catches treating containers, unknown roles, images, inputs, or nodes
    // whose text may live in children as blank slots.
    for node in &not_blank {
        assert_eq!(analyze(std::slice::from_ref(node)).blank_coverage, 0.0);
    }
    for role in ["AXStaticText", "AXCell", "AXButton", "AXTextArea", "AXRow"] {
        assert_eq!(analyze(&[blank(role, full)]).blank_coverage, 1.0, "{role}");
    }
}

#[test]
fn titled_containers_do_not_hide_blank_slots() {
    // A window or list title is a fragment, but a container is not a text
    // region; otherwise a titled window root would cancel all coverage.
    let full = rect(0.0, 0.0, 1.0, 1.0);
    let root = WalkedNode {
        is_root: true,
        ..text("AXWindow", full)
    };
    assert_eq!(root.class, Container);
    let analysis = analyze(&[root, text("AXList", full), blank("AXStaticText", full)]);
    assert_eq!(analysis.blank_coverage, 1.0);
}

#[test]
fn mask_regions_are_secure_and_single_line_inputs() {
    let field = rect(0.1, 0.2, 0.3, 0.05);
    let secure = rect(0.1, 0.3, 0.3, 0.05);
    let combo = rect(0.5, 0.6, 0.2, 0.05);
    let analysis = analyze(&[
        text("AXTextField", field),
        blank("AXSecureTextField", secure),
        text("AXComboBox", combo),
        text("AXTextArea", rect(0.0, 0.7, 1.0, 0.3)),
        text("AXStaticText", rect(0.5, 0.5, 0.1, 0.1)),
    ]);
    assert_eq!(analysis.mask_regions, [field, secure, combo]);
}

#[test]
fn order_containers_are_large_non_root_elements_outside_per_item_roles() {
    let large = rect(0.0, 0.0, 0.5, 0.5);
    let composer = rect(0.5, 0.8, 0.5, 0.2);
    let at_threshold = rect(0.0, 0.0, 0.3, 0.1);
    let root = WalkedNode {
        is_root: true,
        ..text("AXWindow", rect(0.0, 0.0, 1.0, 1.0))
    };
    let analysis = analyze(&[
        root,
        text("AXList", large),
        blank("AXTextArea", composer),
        blank("AXGroup", at_threshold),
        blank("AXGroup", rect(0.0, 0.0, 0.29, 0.1)),
        blank("AXRow", large),
        text("AXCell", large),
        text("AXStaticText", large),
        blank("AXButton", large),
        blank("AXColumn", large),
    ]);
    // Catches the window root or per-item roles becoming reading regions,
    // which would split one list into many.
    assert_eq!(analysis.order_containers, [large, composer, at_threshold]);
}
