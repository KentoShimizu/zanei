use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use zanei_core::schema::ContentSnapshotCutoff;

use crate::{
    content_snapshot::{
        budget::WalkBudget,
        coverage::{WalkAnalysis, WindowRect},
        walker::{
            NodeContentAttributes, NodeSafeAttributes, SnapshotNode, SnapshotReadError, WalkClock,
            walk_snapshot_analyzed,
        },
    },
    ffi::ax::{AxFrame, AxTextRange},
};

use super::support::frame;

#[derive(Clone, Default)]
struct Metrics {
    content_attribute_reads: Arc<AtomicUsize>,
    value_reads: Arc<AtomicUsize>,
    visible_range_reads: Arc<AtomicUsize>,
    string_range_reads: Arc<AtomicUsize>,
    children_count_reads: Arc<AtomicUsize>,
    children_range_reads: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub(super) struct FakeNode {
    attributes: NodeSafeAttributes,
    content_attributes: NodeContentAttributes,
    window_number: Option<i64>,
    value: Option<String>,
    visible: Option<String>,
    children: Vec<Self>,
    fail_safe_read: bool,
    fail_value_read: bool,
    fail_children_count: bool,
    numeric_value: bool,
    illegal_leaf_range: bool,
    /// Children counted but gone by the time the list is read.
    vanished_children: usize,
    root_invalid: bool,
    call_micros: u64,
    elapsed_micros: Arc<AtomicU64>,
    metrics: Metrics,
}

impl FakeNode {
    fn new(role: &str) -> Self {
        Self {
            attributes: NodeSafeAttributes {
                role: Some(role.to_owned()),
                frame: Some(frame(0.0, 0.0, 100.0, 100.0)),
                ..NodeSafeAttributes::default()
            },
            content_attributes: NodeContentAttributes::default(),
            window_number: None,
            value: None,
            visible: None,
            children: Vec::new(),
            fail_safe_read: false,
            fail_value_read: false,
            fail_children_count: false,
            numeric_value: false,
            illegal_leaf_range: false,
            vanished_children: 0,
            root_invalid: false,
            call_micros: 0,
            elapsed_micros: Arc::new(AtomicU64::new(0)),
            metrics: Metrics::default(),
        }
    }

    fn with_shared(mut self, source: &Self) -> Self {
        self.elapsed_micros = Arc::clone(&source.elapsed_micros);
        self.metrics = source.metrics.clone();
        self.call_micros = source.call_micros;
        self.children = self
            .children
            .into_iter()
            .map(|child| child.with_shared(source))
            .collect();
        self
    }

    pub(super) fn chromium_window() -> Self {
        let mut root = Self::new("AXWindow");
        let mut checkbox = Self::new("AXCheckBox");
        checkbox.content_attributes.title = Some("Checked option".to_owned());
        checkbox.numeric_value = true;
        checkbox.illegal_leaf_range = true;
        let mut heading = Self::new("AXHeading");
        heading.content_attributes.title = Some("Heading".to_owned());
        heading.numeric_value = true;
        heading.illegal_leaf_range = true;
        root.children = vec![checkbox, heading];
        let shared = root.clone();
        root.with_shared(&shared)
    }

    pub(super) fn numbered_window(window_number: i64, text: &str) -> Self {
        let mut root = Self::new("AXWindow");
        root.window_number = Some(window_number);
        let mut child = Self::new("AXStaticText");
        child.value = Some(text.to_owned());
        root.children.push(child);
        let shared = root.clone();
        root.with_shared(&shared)
    }

    /// A window without `AXWindowNumber`, resolved by its frame.
    pub(super) fn framed_window(frame: Option<crate::ffi::ax::AxFrame>) -> Self {
        let mut root = Self::new("AXWindow");
        root.attributes.frame = frame;
        root
    }

    pub(super) fn with_window_title(mut self, title: Option<&str>) -> Self {
        self.content_attributes.title = title.map(str::to_owned);
        self
    }

    pub(super) fn content_attribute_reads(&self) -> usize {
        self.metrics.content_attribute_reads.load(Ordering::Relaxed)
    }

    pub(super) fn value_reads(&self) -> usize {
        self.metrics.value_reads.load(Ordering::Relaxed)
    }

    fn tick(&self) {
        self.elapsed_micros
            .fetch_add(self.call_micros, Ordering::Relaxed);
    }
}

impl crate::content_snapshot::worker::scan::SnapshotWindow for FakeNode {
    fn frame(
        &self,
    ) -> Result<Option<crate::ffi::ax::AxFrame>, crate::content_snapshot::SnapshotAxError> {
        Ok(self.attributes.frame)
    }

    fn window_number(&self) -> Result<Option<i64>, crate::content_snapshot::SnapshotAxError> {
        Ok(self.window_number)
    }

    fn title(&self) -> Result<Option<String>, crate::content_snapshot::SnapshotAxError> {
        Ok(self.content_attributes.title.clone())
    }
}

impl SnapshotNode for FakeNode {
    fn safe_attributes(&self) -> Result<NodeSafeAttributes, SnapshotReadError> {
        self.tick();
        if self.fail_safe_read {
            Err(SnapshotReadError::Contract("CopyMultiple failed"))
        } else if self.root_invalid {
            Err(SnapshotReadError::Ax(
                crate::content_snapshot::SnapshotAxError::invalid_ui_element_for_test(7),
            ))
        } else {
            Ok(self.attributes.clone())
        }
    }

    fn content_attributes(&self) -> Result<NodeContentAttributes, SnapshotReadError> {
        self.tick();
        self.metrics
            .content_attribute_reads
            .fetch_add(1, Ordering::Relaxed);
        Ok(self.content_attributes.clone())
    }

    fn value(&self) -> Result<Option<String>, SnapshotReadError> {
        self.tick();
        self.metrics.value_reads.fetch_add(1, Ordering::Relaxed);
        if self.fail_value_read {
            Err(SnapshotReadError::Contract("non-text AXValue"))
        } else if self.numeric_value {
            Ok(None)
        } else {
            Ok(self.value.clone())
        }
    }

    fn visible_range(&self) -> Result<Option<AxTextRange>, SnapshotReadError> {
        self.tick();
        self.metrics
            .visible_range_reads
            .fetch_add(1, Ordering::Relaxed);
        Ok(self.visible.as_ref().map(|text| AxTextRange {
            location: 0,
            length: isize::try_from(text.chars().count()).expect("fake text length"),
        }))
    }

    fn string_for_range(&self, _range: AxTextRange) -> Result<Option<String>, SnapshotReadError> {
        self.tick();
        self.metrics
            .string_range_reads
            .fetch_add(1, Ordering::Relaxed);
        Ok(self.visible.clone())
    }

    fn children_count(&self) -> Result<usize, SnapshotReadError> {
        self.tick();
        self.metrics
            .children_count_reads
            .fetch_add(1, Ordering::Relaxed);
        if self.fail_children_count {
            return Err(SnapshotReadError::Ax(
                crate::content_snapshot::SnapshotAxError::illegal_argument_for_test(7),
            ));
        }
        Ok(self.children.len() + self.vanished_children)
    }

    fn children_range(
        &self,
        index: usize,
        maximum_count: usize,
    ) -> Result<Vec<Self>, SnapshotReadError> {
        self.tick();
        self.metrics
            .children_range_reads
            .fetch_add(1, Ordering::Relaxed);
        if self.illegal_leaf_range && self.children.is_empty() {
            return Err(SnapshotReadError::Ax(
                crate::content_snapshot::SnapshotAxError::illegal_argument_for_test(7),
            ));
        }
        Ok(self
            .children
            .iter()
            .skip(index)
            .take(maximum_count)
            .cloned()
            .collect())
    }
}

struct FakeClock(Arc<AtomicU64>);

impl WalkClock for FakeClock {
    fn elapsed(&self) -> Duration {
        Duration::from_micros(self.0.load(Ordering::Relaxed))
    }
}

fn walk(root: FakeNode, budget: WalkBudget) -> crate::content_snapshot::SnapshotWalkOutput {
    let clock = FakeClock(Arc::clone(&root.elapsed_micros));
    walk_snapshot_analyzed(root, frame(0.0, 0.0, 100.0, 100.0), budget, &clock, || {
        false
    })
    .expect("fake walk")
    .0
}

fn generous_budget() -> WalkBudget {
    WalkBudget {
        wall_time: Duration::from_secs(1),
        nodes: 100,
        text_bytes: 1_024,
    }
}

#[test]
fn sensitive_and_unknown_classes_never_read_values_or_descend_secure_menu_subtrees() {
    let mut root = FakeNode::new("AXGroup");
    let metrics = root.metrics.clone();
    let mut secure = FakeNode::new("AXSecureTextField");
    secure.value = Some("password".to_owned());
    let mut leaked = FakeNode::new("AXStaticText");
    leaked.value = Some("leaked".to_owned());
    secure.children.push(leaked);
    let mut single = FakeNode::new("AXTextField");
    single.content_attributes.title = Some("Label".to_owned());
    single.value = Some("4111111111111111".to_owned());
    let mut area = FakeNode::new("AXTextArea");
    area.value = Some("hidden scrollback".to_owned());
    area.visible = Some("visible output".to_owned());
    let mut unknown = FakeNode::new("AXFutureControl");
    unknown.content_attributes.title = Some("Mystery".to_owned());
    unknown.value = Some("unknown secret".to_owned());
    let mut menu = FakeNode::new("AXMenu");
    let mut menu_child = FakeNode::new("AXStaticText");
    menu_child.value = Some("menu secret".to_owned());
    menu.children.push(menu_child);
    root.children = vec![secure, single, area, unknown, menu];
    let shared = root.clone();
    root = root.with_shared(&shared);

    let output = walk(root, generous_budget());
    assert_eq!(output.text, "Label\nvisible output\nMystery");
    assert_eq!(metrics.value_reads.load(Ordering::Relaxed), 0);
    assert_eq!(metrics.visible_range_reads.load(Ordering::Relaxed), 1);
    assert_eq!(metrics.string_range_reads.load(Ordering::Relaxed), 1);
    assert_eq!(
        output.nodes, 6,
        "secure and menu descendants are not visited"
    );
}

#[test]
fn readable_text_assembly_trims_fragments_and_collapses_blank_lines() {
    let mut root = FakeNode::new("AXGroup");
    let mut first = FakeNode::new("AXStaticText");
    first.value = Some("  Alpha\n\n\nBeta  ".to_owned());
    let mut second = FakeNode::new("AXStaticText");
    second.value = Some("Gamma".to_owned());
    root.children = vec![first, second];
    let shared = root.clone();
    root = root.with_shared(&shared);

    assert_eq!(walk(root, generous_budget()).text, "Alpha\n\nBeta\nGamma");
}

#[test]
fn node_time_utf8_byte_and_stop_conditions_report_specific_cutoffs() {
    let mut node_root = FakeNode::new("AXGroup");
    node_root.children = vec![FakeNode::new("AXStaticText"), FakeNode::new("AXStaticText")];
    let shared = node_root.clone();
    node_root = node_root.with_shared(&shared);
    let nodes = walk(
        node_root,
        WalkBudget {
            nodes: 2,
            ..generous_budget()
        },
    );
    assert_eq!(nodes.cutoff, Some(ContentSnapshotCutoff::Nodes));

    let mut timed = FakeNode::new("AXGroup");
    timed.call_micros = 100_000;
    let shared = timed.clone();
    timed = timed.with_shared(&shared);
    let timed = walk(
        timed,
        WalkBudget {
            wall_time: Duration::from_millis(150),
            ..generous_budget()
        },
    );
    assert_eq!(timed.cutoff, Some(ContentSnapshotCutoff::Time));
    assert_eq!(timed.elapsed, Duration::from_millis(200));

    let mut bytes = FakeNode::new("AXStaticText");
    bytes.value = Some("ééé".to_owned());
    let shared = bytes.clone();
    bytes = bytes.with_shared(&shared);
    let bytes = walk(
        bytes,
        WalkBudget {
            text_bytes: 5,
            ..generous_budget()
        },
    );
    assert_eq!(bytes.text, "éé");
    assert_eq!(bytes.text.len(), 4);
    assert_eq!(bytes.cutoff, Some(ContentSnapshotCutoff::Bytes));

    let stopped = FakeNode::new("AXGroup");
    let clock = FakeClock(Arc::clone(&stopped.elapsed_micros));
    let (stopped, _) = walk_snapshot_analyzed(
        stopped,
        frame(0.0, 0.0, 100.0, 100.0),
        generous_budget(),
        &clock,
        || true,
    )
    .expect("stopped walk");
    assert_eq!(stopped.cutoff, Some(ContentSnapshotCutoff::Stopped));
}

#[test]
fn node_failures_are_degraded_and_offscreen_nodes_are_excluded() {
    let mut failed = FakeNode::new("AXGroup");
    let mut failed_child = FakeNode::new("AXStaticText");
    failed_child.fail_safe_read = true;
    failed.children.push(failed_child);
    failed.fail_safe_read = true;
    let output = walk(failed, generous_budget());
    assert_eq!(output.cutoff, None);
    assert_eq!(output.degraded_nodes, 2);

    let mut root = FakeNode::new("AXGroup");
    let mut outside = FakeNode::new("AXStaticText");
    outside.attributes.frame = Some(frame(200.0, 200.0, 10.0, 10.0));
    outside.value = Some("outside".to_owned());
    root.children.push(outside);
    let shared = root.clone();
    root = root.with_shared(&shared);
    let output = walk(root, generous_budget());
    assert!(output.text.is_empty());
    assert_eq!(output.cutoff, None);
}

#[test]
fn frameless_nodes_skip_own_content_and_containers_descend_into_framed_children() {
    let mut root = FakeNode::new("AXGroup");
    root.attributes.frame = None;
    root.content_attributes.title = Some("frameless title".to_owned());
    root.content_attributes.description = Some("frameless description".to_owned());
    let metrics = root.metrics.clone();
    let mut frameless_text = FakeNode::new("AXStaticText");
    frameless_text.attributes.frame = None;
    frameless_text.content_attributes.title = Some("frameless label".to_owned());
    frameless_text.content_attributes.description = Some("frameless hint".to_owned());
    frameless_text.value = Some("frameless value".to_owned());
    let mut child = FakeNode::new("AXTextField");
    child.content_attributes.title = Some("Framed child".to_owned());
    root.children = vec![frameless_text, child];
    let shared = root.clone();
    root = root.with_shared(&shared);

    let output = walk(root, generous_budget());

    assert_eq!(output.text, "Framed child");
    assert_eq!(output.frameless_nodes, 2);
    assert_eq!(output.degraded_nodes, 0);
    assert_eq!(metrics.value_reads.load(Ordering::Relaxed), 0);
    assert_eq!(
        metrics.content_attribute_reads.load(Ordering::Relaxed),
        1,
        "only the framed child reads title and description"
    );
}

#[test]
fn numeric_values_drop_only_the_affected_nodes() {
    let mut root = FakeNode::new("AXGroup");
    let mut checkbox = FakeNode::new("AXCheckBox");
    checkbox.fail_value_read = true;
    checkbox.content_attributes.title = Some("Checked option".to_owned());
    let mut heading = FakeNode::new("AXHeading");
    heading.fail_value_read = true;
    heading.content_attributes.title = Some("Heading".to_owned());
    let mut text = FakeNode::new("AXStaticText");
    text.value = Some("Visible text".to_owned());
    root.children = vec![checkbox, heading, text];
    let shared = root.clone();
    root = root.with_shared(&shared);

    let output = walk(root, generous_budget());

    assert_eq!(output.cutoff, None);
    assert_eq!(output.degraded_nodes, 2);
    assert_eq!(output.text, "Visible text");
}

#[test]
fn chromium_profile_counts_children_before_ranges_and_accepts_numeric_values() {
    let root = FakeNode::chromium_window();
    let metrics = root.metrics.clone();

    let output = walk(root, generous_budget());

    // Chromium returns -25201 for a ranged AXChildren read on a leaf. This
    // assertion fails if count-first traversal is reverted and makes that call.
    assert_eq!(metrics.children_count_reads.load(Ordering::Relaxed), 3);
    assert_eq!(metrics.children_range_reads.load(Ordering::Relaxed), 1);
    assert_eq!(output.text, "Checked option\nHeading");
    assert_eq!(output.degraded_nodes, 0);
    assert_eq!(output.ax_calls, 12);
}

#[test]
fn invalid_window_root_ends_the_walk() {
    let mut root = FakeNode::new("AXWindow");
    root.root_invalid = true;
    let clock = FakeClock(Arc::clone(&root.elapsed_micros));

    let error = walk_snapshot_analyzed(
        root,
        frame(0.0, 0.0, 100.0, 100.0),
        generous_budget(),
        &clock,
        || false,
    )
    .expect_err("invalid window root");

    assert!(matches!(
        error.source,
        SnapshotReadError::Ax(error) if error.is_invalid_ui_element()
    ));
}

fn placed(role: &str, x: f64, y: f64, width: f64, height: f64) -> FakeNode {
    let mut node = FakeNode::new(role);
    node.attributes.frame = Some(frame(x, y, width, height));
    node
}

fn with_value(mut node: FakeNode, value: &str) -> FakeNode {
    node.value = Some(value.to_owned());
    node
}

fn analyze(root: FakeNode, window: AxFrame) -> (String, WalkAnalysis) {
    let clock = FakeClock(Arc::clone(&root.elapsed_micros));
    let (output, analysis) =
        walk_snapshot_analyzed(root, window, generous_budget(), &clock, || false)
            .expect("fake walk");
    assert_eq!(output.cutoff, None);
    (output.text, analysis)
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> WindowRect {
    WindowRect {
        x,
        y,
        width,
        height,
    }
}

#[test]
fn custom_drawn_chat_window_with_blank_texts_and_childless_rows_is_ax_insufficient() {
    // Window at (100, 50), 1000 x 500 points: a sidebar of childless rows
    // under a search field, a header label, a chat log of blank static texts,
    // and an empty composer.
    let mut root = placed("AXWindow", 100.0, 50.0, 1000.0, 500.0).with_window_title(Some("Chat"));
    let mut split = placed("AXSplitGroup", 100.0, 50.0, 1000.0, 500.0);
    let mut sidebar = placed("AXScrollArea", 100.0, 50.0, 250.0, 500.0);
    let search = with_value(
        placed("AXTextField", 110.0, 60.0, 230.0, 30.0),
        "typed query",
    );
    let mut table = placed("AXTable", 100.0, 100.0, 250.0, 450.0);
    table.children = (0..6)
        .map(|row| placed("AXRow", 100.0, 100.0 + 75.0 * f64::from(row), 250.0, 75.0))
        .collect();
    sidebar.children = vec![search, table];
    let header = with_value(placed("AXStaticText", 350.0, 50.0, 200.0, 40.0), "Name");
    let mut log = placed("AXScrollArea", 350.0, 90.0, 750.0, 360.0);
    let mut list = placed("AXList", 350.0, 90.0, 750.0, 360.0);
    list.content_attributes.title = Some("Messages".to_owned());
    list.children = (0..3)
        .map(|item| {
            placed(
                "AXStaticText",
                370.0,
                100.0 + 120.0 * f64::from(item),
                700.0,
                100.0,
            )
        })
        .collect();
    log.children = vec![list];
    let composer = placed("AXTextArea", 350.0, 450.0, 750.0, 100.0);
    split.children = vec![sidebar, header, log, composer];
    root.children = vec![split];

    let (text, analysis) = analyze(root, frame(100.0, 50.0, 1000.0, 500.0));

    // Catches the window or list title counting as a text region, which
    // would cancel the blank coverage of this window.
    assert_eq!(text, "Chat\nName\nMessages");
    assert!(analysis.blank_coverage > 0.7, "{}", analysis.blank_coverage);
    assert!(analysis.ax_insufficient());
    // Catches a bottom-left flip or a missing window offset in mask regions.
    assert_eq!(analysis.mask_regions, [rect(0.01, 0.02, 0.23, 0.06)]);
    // Catches rows, static texts, the root, or small fields becoming order
    // containers.
    assert_eq!(
        analysis.order_containers,
        [
            rect(0.0, 0.0, 1.0, 1.0),
            rect(0.0, 0.0, 0.25, 1.0),
            rect(0.0, 0.1, 0.25, 0.9),
            rect(0.25, 0.08, 0.75, 0.72),
            rect(0.25, 0.08, 0.75, 0.72),
            rect(0.25, 0.8, 0.75, 0.2),
        ]
    );
}

#[test]
fn web_window_with_large_childless_decorative_groups_is_ax_sufficient() {
    let mut root = placed("AXWindow", 0.0, 0.0, 1200.0, 800.0).with_window_title(Some("App"));
    let backdrop = placed("AXGroup", 0.0, 0.0, 1200.0, 800.0);
    let rail = placed("AXGroup", 0.0, 0.0, 200.0, 800.0);
    let unknown = placed("AXFutureControl", 200.0, 0.0, 1000.0, 400.0);
    let picture = placed("AXImage", 300.0, 300.0, 400.0, 300.0);
    let icon = placed("AXButton", 10.0, 10.0, 24.0, 24.0);
    let mut page = placed("AXWebArea", 200.0, 0.0, 1000.0, 800.0);
    page.children = (0..4)
        .map(|paragraph| {
            with_value(
                placed(
                    "AXStaticText",
                    220.0,
                    20.0 + 40.0 * f64::from(paragraph),
                    900.0,
                    30.0,
                ),
                "Paragraph",
            )
        })
        .collect();
    root.children = vec![backdrop, rail, unknown, picture, icon, page];

    let (_, analysis) = analyze(root, frame(0.0, 0.0, 1200.0, 800.0));

    // Catches containers, unknown roles, or images counting as blank slots;
    // only the small icon button is blank here.
    assert!(
        analysis.blank_coverage < 0.01,
        "{}",
        analysis.blank_coverage
    );
    assert!(!analysis.ax_insufficient());
}

#[test]
fn text_areas_are_blank_slots_only_when_empty_and_childless() {
    let window = frame(0.0, 0.0, 800.0, 600.0);
    let editor = |visible: Option<&str>, children: Vec<FakeNode>| {
        let mut root = placed("AXWindow", 0.0, 0.0, 800.0, 600.0).with_window_title(Some("Doc"));
        let mut toolbar = placed("AXToolbar", 0.0, 0.0, 800.0, 50.0);
        let mut bold = placed("AXButton", 10.0, 10.0, 30.0, 30.0);
        bold.content_attributes.description = Some("Bold".to_owned());
        toolbar.children = vec![bold];
        let mut area = placed("AXTextArea", 0.0, 50.0, 800.0, 550.0);
        area.visible = visible.map(str::to_owned);
        area.children = children;
        root.children = vec![toolbar, area];
        analyze(root, window).1
    };

    // A native editor showing its text is sufficient.
    assert_eq!(editor(Some("Draft text"), Vec::new()).blank_coverage, 0.0);
    // A text area that exposes no visible text, as a custom-drawn terminal
    // does, covers most of the window.
    assert!(editor(None, Vec::new()).ax_insufficient());
    // Catches skipping the childless rule for text areas, whose children the
    // walk never reads.
    let attachment = placed("AXImage", 0.0, 50.0, 100.0, 100.0);
    assert_eq!(editor(None, vec![attachment]).blank_coverage, 0.0);
}

#[test]
fn failed_child_count_of_a_blank_text_area_is_degraded_not_blank() {
    // Catches a timed-out child-count read vanishing from degraded counts
    // while the text area silently drops out of coverage.
    let mut root = placed("AXWindow", 0.0, 0.0, 800.0, 600.0);
    let mut area = placed("AXTextArea", 0.0, 0.0, 800.0, 600.0);
    area.fail_children_count = true;
    root.children = vec![area];
    let clock = FakeClock(Arc::clone(&root.elapsed_micros));

    let (output, analysis) = walk_snapshot_analyzed(
        root,
        frame(0.0, 0.0, 800.0, 600.0),
        generous_budget(),
        &clock,
        || false,
    )
    .expect("fake walk");

    assert_eq!(output.degraded_nodes, 1);
    assert_eq!(analysis.blank_coverage, 0.0);
}

#[test]
fn masks_are_complete_only_when_every_input_field_could_be_placed() {
    // Catches recognition masking from a partial walk: a cut-off walk, an unreadable role or
    // child list, a child list shorter than its count, or a frameless input can each hide a
    // field whose text would then be stored. Also catches the trace losing which of these
    // happened and the AX error codes behind an unreadable role or child list.
    let mask_gaps = |root: FakeNode, budget: WalkBudget| {
        let clock = FakeClock(Arc::clone(&root.elapsed_micros));
        let window = frame(0.0, 0.0, 100.0, 100.0);
        let gaps = walk_snapshot_analyzed(root, window, budget, &clock, || false)
            .expect("fake walk")
            .1
            .mask_gaps;
        (!gaps.is_empty()).then(|| gaps.to_string())
    };
    let window_with = |child: FakeNode| {
        let mut root = FakeNode::new("AXGroup");
        root.children.push(child);
        root
    };
    let field = FakeNode::new("AXTextField");
    assert_eq!(
        mask_gaps(window_with(field.clone()), generous_budget()),
        None
    );

    let one_node = WalkBudget {
        nodes: 1,
        ..generous_budget()
    };
    let mut unreadable = FakeNode::new("AXGroup");
    unreadable.fail_safe_read = true;
    let mut uncounted = window_with(field.clone());
    uncounted.fail_children_count = true;
    let mut shrunk = window_with(field.clone());
    shrunk.vanished_children = 1;
    let mut frameless = field.clone();
    frameless.attributes.frame = None;
    let mut unlisted = FakeNode::new("AXGroup");
    unlisted.illegal_leaf_range = true;
    unlisted.vanished_children = 1;
    let mut both = window_with(unreadable.clone());
    both.children.push(unlisted);
    for (root, budget, expected) in [
        (
            window_with(field),
            one_node,
            "mask_gaps=cutoff:nodes ax_errors=none",
        ),
        (
            window_with(unreadable),
            generous_budget(),
            "mask_gaps=role_unreadable:1 ax_errors=contract",
        ),
        (
            uncounted,
            generous_budget(),
            "mask_gaps=children_unreadable:1 ax_errors=-25201",
        ),
        (
            shrunk,
            generous_budget(),
            "mask_gaps=children_short:1 ax_errors=none",
        ),
        (
            window_with(frameless),
            generous_budget(),
            "mask_gaps=frameless_input:1 ax_errors=none",
        ),
        (
            both,
            generous_budget(),
            "mask_gaps=role_unreadable:1,children_unreadable:1 ax_errors=contract,-25201",
        ),
    ] {
        assert_eq!(mask_gaps(root, budget).as_deref(), Some(expected));
    }
}
