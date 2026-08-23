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
}

#[derive(Clone)]
struct FakeNode {
    attributes: NodeSafeAttributes,
    value: Option<String>,
    visible: Option<String>,
    children: Vec<Self>,
    fail_safe_read: bool,
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
            value: None,
            visible: None,
            children: Vec::new(),
            fail_safe_read: false,
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

    fn tick(&self) {
        self.elapsed_micros
            .fetch_add(self.call_micros, Ordering::Relaxed);
    }
}

impl SnapshotNode for FakeNode {
    fn safe_attributes(&self) -> Result<NodeSafeAttributes, SnapshotReadError> {
        self.tick();
        if self.fail_safe_read {
            Err(SnapshotReadError::Contract("CopyMultiple failed"))
        } else {
            Ok(self.attributes.clone())
        }
    }

    fn value(&self) -> Result<Option<String>, SnapshotReadError> {
        self.tick();
        self.metrics.value_reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.value.clone())
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

    fn children_range(
        &self,
        index: usize,
        maximum_count: usize,
    ) -> Result<Vec<Self>, SnapshotReadError> {
        self.tick();
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
fn copy_multiple_failure_aborts_the_snapshot_and_offscreen_nodes_are_excluded() {
    let mut failed = FakeNode::new("AXStaticText");
    failed.fail_safe_read = true;
    let clock = FakeClock(Arc::clone(&failed.elapsed_micros));
    let error = walk_snapshot(
        failed,
        frame(0.0, 0.0, 100.0, 100.0),
        generous_budget(),
        &clock,
        || false,
    )
    .expect_err("CopyMultiple failure");
    assert!(matches!(
        error.source,
        SnapshotReadError::Contract("CopyMultiple failed")
    ));

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
