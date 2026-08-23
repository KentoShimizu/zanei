use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use crate::{
    content_snapshot::{
        SnapshotCutoff,
        budget::WalkBudget,
        walker::{NodeSafeAttributes, SnapshotNode, SnapshotReadError, WalkClock, walk_snapshot},
    },
    ffi::ax::AxTextRange,
};

use super::support::frame;

#[derive(Clone, Default)]
struct Metrics {
    value_reads: Arc<AtomicUsize>,
    visible_range_reads: Arc<AtomicUsize>,
    string_range_reads: Arc<AtomicUsize>,
    children_count_reads: Arc<AtomicUsize>,
    children_range_reads: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub(super) struct FakeNode {
    attributes: NodeSafeAttributes,
    window_number: Option<i64>,
    value: Option<String>,
    visible: Option<String>,
    children: Vec<Self>,
    fail_safe_read: bool,
    fail_value_read: bool,
    numeric_value: bool,
    illegal_leaf_range: bool,
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
            window_number: None,
            value: None,
            visible: None,
            children: Vec::new(),
            fail_safe_read: false,
            fail_value_read: false,
            numeric_value: false,
            illegal_leaf_range: false,
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
        checkbox.attributes.title = Some("Checked option".to_owned());
        checkbox.numeric_value = true;
        checkbox.illegal_leaf_range = true;
        let mut heading = Self::new("AXHeading");
        heading.attributes.title = Some("Heading".to_owned());
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

    fn tick(&self) {
        self.elapsed_micros
            .fetch_add(self.call_micros, Ordering::Relaxed);
    }
}

impl crate::content_snapshot::worker::SnapshotWindow for FakeNode {
    fn frame(
        &self,
    ) -> Result<Option<crate::ffi::ax::AxFrame>, crate::content_snapshot::SnapshotAxError> {
        Ok(self.attributes.frame)
    }

    fn window_number(&self) -> Result<Option<i64>, crate::content_snapshot::SnapshotAxError> {
        Ok(self.window_number)
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
        Ok(self.children.len())
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
    walk_snapshot(root, frame(0.0, 0.0, 100.0, 100.0), budget, &clock, || {
        false
    })
    .expect("fake walk")
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
    single.attributes.title = Some("Label".to_owned());
    single.value = Some("4111111111111111".to_owned());
    let mut area = FakeNode::new("AXTextArea");
    area.value = Some("hidden scrollback".to_owned());
    area.visible = Some("visible output".to_owned());
    let mut unknown = FakeNode::new("AXFutureControl");
    unknown.attributes.title = Some("Mystery".to_owned());
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
fn node_time_and_utf8_byte_budgets_report_specific_cutoffs() {
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
    assert_eq!(nodes.cutoff, Some(SnapshotCutoff::Nodes));
    assert!(!nodes.complete);

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
    assert_eq!(timed.cutoff, Some(SnapshotCutoff::Time));
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
    assert_eq!(bytes.cutoff, Some(SnapshotCutoff::Bytes));
}

#[test]
fn node_failures_are_degraded_and_offscreen_nodes_are_excluded() {
    let mut failed = FakeNode::new("AXGroup");
    let mut failed_child = FakeNode::new("AXStaticText");
    failed_child.fail_safe_read = true;
    failed.children.push(failed_child);
    failed.fail_safe_read = true;
    let output = walk(failed, generous_budget());
    assert!(output.complete);
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
    assert!(output.complete);
}

#[test]
fn numeric_values_drop_only_the_affected_nodes() {
    let mut root = FakeNode::new("AXGroup");
    let mut checkbox = FakeNode::new("AXCheckBox");
    checkbox.fail_value_read = true;
    checkbox.attributes.title = Some("Checked option".to_owned());
    let mut heading = FakeNode::new("AXHeading");
    heading.fail_value_read = true;
    heading.attributes.title = Some("Heading".to_owned());
    let mut text = FakeNode::new("AXStaticText");
    text.value = Some("Visible text".to_owned());
    root.children = vec![checkbox, heading, text];
    let shared = root.clone();
    root = root.with_shared(&shared);

    let output = walk(root, generous_budget());

    assert!(output.complete);
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
    assert_eq!(output.ax_calls, 9);
}

#[test]
fn invalid_window_root_ends_the_walk() {
    let mut root = FakeNode::new("AXWindow");
    root.root_invalid = true;
    let clock = FakeClock(Arc::clone(&root.elapsed_micros));

    let error = walk_snapshot(
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
