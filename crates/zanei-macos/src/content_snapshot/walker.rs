//! Budgeted depth-first Accessibility traversal and text assembly.

use std::{
    fmt,
    time::{Duration, Instant},
};

use crate::ffi::ax::{AxFrame, AxTextRange, SnapshotAxError};

use super::{
    budget::{CHILDREN_BATCH_SIZE, WalkBudget},
    role::{SnapshotNodeClass, classify_role},
};

#[path = "walker/text.rs"]
mod text;
use text::TextAssembler;

#[path = "walker/ax_node.rs"]
mod ax_node;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotCutoff {
    Time,
    Nodes,
    Bytes,
    Stopped,
}

impl SnapshotCutoff {
    pub(crate) const fn trace_name(self) -> &'static str {
        match self {
            Self::Time => "time",
            Self::Nodes => "nodes",
            Self::Bytes => "bytes",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug)]
pub enum SnapshotReadError {
    Ax(SnapshotAxError),
    Contract(&'static str),
}

impl fmt::Display for SnapshotReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ax(error) => error.fmt(formatter),
            Self::Contract(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SnapshotReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ax(error) => Some(error),
            Self::Contract(_) => None,
        }
    }
}

impl From<SnapshotAxError> for SnapshotReadError {
    fn from(error: SnapshotAxError) -> Self {
        Self::Ax(error)
    }
}

impl SnapshotReadError {
    fn ends_root_walk(&self) -> bool {
        matches!(self, Self::Ax(error) if error.is_invalid_ui_element())
    }
}

#[derive(Debug)]
pub struct SnapshotWalkError {
    pub(crate) source: SnapshotReadError,
    pub(crate) nodes: usize,
    pub(crate) elapsed: Duration,
}

impl fmt::Display for SnapshotWalkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for SnapshotWalkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NodeSafeAttributes {
    pub(crate) role: Option<String>,
    pub(crate) subrole: Option<String>,
    pub(crate) frame: Option<AxFrame>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NodeContentAttributes {
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
}

pub(crate) trait SnapshotNode: Sized {
    fn safe_attributes(&self) -> Result<NodeSafeAttributes, SnapshotReadError>;
    fn content_attributes(&self) -> Result<NodeContentAttributes, SnapshotReadError>;
    fn value(&self) -> Result<Option<String>, SnapshotReadError>;
    fn visible_range(&self) -> Result<Option<AxTextRange>, SnapshotReadError>;
    fn string_for_range(&self, range: AxTextRange) -> Result<Option<String>, SnapshotReadError>;
    fn children_count(&self) -> Result<usize, SnapshotReadError>;
    fn children_range(
        &self,
        index: usize,
        maximum_count: usize,
    ) -> Result<Vec<Self>, SnapshotReadError>;
}

pub(crate) trait WalkClock {
    fn elapsed(&self) -> Duration;
}

pub(crate) struct InstantWalkClock {
    started_at: Instant,
}

impl InstantWalkClock {
    pub(crate) fn start() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }
}

impl WalkClock for InstantWalkClock {
    fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotWalkOutput {
    pub text: String,
    pub nodes: usize,
    pub ax_calls: usize,
    pub elapsed: Duration,
    pub complete: bool,
    pub cutoff: Option<SnapshotCutoff>,
    pub degraded_nodes: usize,
    pub frameless_nodes: usize,
}

enum Work<N> {
    Visit {
        node: N,
        is_root: bool,
    },
    Children {
        node: N,
        index: usize,
        count: usize,
        is_root: bool,
        degraded: bool,
    },
}

pub(crate) fn walk_snapshot<N: SnapshotNode>(
    root: N,
    window_frame: AxFrame,
    budget: WalkBudget,
    clock: &impl WalkClock,
    stopped: impl Fn() -> bool,
) -> Result<SnapshotWalkOutput, SnapshotWalkError> {
    let mut context = WalkContext {
        budget,
        clock,
        stopped: &stopped,
        nodes: 0,
        ax_calls: 0,
        text: TextAssembler::new(budget.text_bytes),
        cutoff: None,
        degraded_nodes: 0,
        frameless_nodes: 0,
    };
    let mut stack = vec![Work::Visit {
        node: root,
        is_root: true,
    }];
    while let Some(work) = stack.pop() {
        if !context.can_continue() {
            break;
        }
        match work {
            Work::Visit { node, is_root } => {
                visit_node(node, is_root, window_frame, &mut stack, &mut context)?;
            }
            Work::Children {
                node,
                index,
                count,
                is_root,
                degraded,
            } => {
                load_children(
                    node,
                    index,
                    count,
                    is_root,
                    degraded,
                    &mut stack,
                    &mut context,
                )?;
            }
        }
    }
    // Account for the final native call crossing the wall-time boundary even
    // when it discovers that no work remains.
    let _ = context.can_continue();
    Ok(SnapshotWalkOutput {
        text: context.text.finish(),
        nodes: context.nodes,
        ax_calls: context.ax_calls,
        elapsed: clock.elapsed(),
        complete: context.cutoff.is_none(),
        cutoff: context.cutoff,
        degraded_nodes: context.degraded_nodes,
        frameless_nodes: context.frameless_nodes,
    })
}

fn visit_node<'a, N: SnapshotNode>(
    node: N,
    is_root: bool,
    window_frame: AxFrame,
    stack: &mut Vec<Work<N>>,
    context: &mut WalkContext<'a>,
) -> Result<(), SnapshotWalkError> {
    if context.nodes >= context.budget.nodes {
        context.cutoff = Some(SnapshotCutoff::Nodes);
        return Ok(());
    }
    context.nodes += 1;
    let attributes = match context.read_node(is_root, || node.safe_attributes())? {
        NodeRead::Value(attributes) => attributes,
        NodeRead::Degraded => {
            context.degraded_nodes = context.degraded_nodes.saturating_add(1);
            if context.can_continue()
                && let NodeRead::Value(count) =
                    context.read_node(is_root, || node.children_count())?
                && count > 0
            {
                stack.push(Work::Children {
                    node,
                    index: 0,
                    count,
                    is_root,
                    degraded: true,
                });
            }
            return Ok(());
        }
    };
    if !context.can_continue() {
        return Ok(());
    }
    let class = classify_role(attributes.role.as_deref(), attributes.subrole.as_deref());
    let Some(frame) = attributes.frame else {
        context.frameless_nodes = context.frameless_nodes.saturating_add(1);
        descend_if_container(node, is_root, class, false, stack, context)?;
        return Ok(());
    };
    if !frames_intersect(frame, window_frame) {
        return Ok(());
    }
    let (fragments, degraded) = node_fragments(&node, is_root, class, context)?;
    if degraded {
        context.degraded_nodes = context.degraded_nodes.saturating_add(1);
    }
    for fragment in fragments {
        if !context.text.push(&fragment) {
            context.cutoff = Some(SnapshotCutoff::Bytes);
            return Ok(());
        }
    }
    descend_if_container(node, is_root, class, degraded, stack, context)
}

fn descend_if_container<'a, N: SnapshotNode>(
    node: N,
    is_root: bool,
    class: SnapshotNodeClass,
    degraded: bool,
    stack: &mut Vec<Work<N>>,
    context: &mut WalkContext<'a>,
) -> Result<(), SnapshotWalkError> {
    if class.descends() && context.can_continue() {
        match context.read_node(is_root, || node.children_count())? {
            NodeRead::Value(0) => {}
            NodeRead::Value(count) => stack.push(Work::Children {
                node,
                index: 0,
                count,
                is_root,
                degraded,
            }),
            NodeRead::Degraded => {
                if !degraded {
                    context.degraded_nodes = context.degraded_nodes.saturating_add(1);
                }
            }
        }
    }
    Ok(())
}

fn load_children<'a, N: SnapshotNode>(
    node: N,
    index: usize,
    count: usize,
    is_root: bool,
    degraded: bool,
    stack: &mut Vec<Work<N>>,
    context: &mut WalkContext<'a>,
) -> Result<(), SnapshotWalkError> {
    let maximum_count = CHILDREN_BATCH_SIZE.min(count.saturating_sub(index));
    let children = match context.read_node(is_root, || node.children_range(index, maximum_count))? {
        NodeRead::Value(children) => children,
        NodeRead::Degraded => {
            if !degraded {
                context.degraded_nodes = context.degraded_nodes.saturating_add(1);
            }
            return Ok(());
        }
    };
    if !context.can_continue() {
        return Ok(());
    }
    if index.saturating_add(children.len()) < count && children.len() == maximum_count {
        stack.push(Work::Children {
            node,
            index: index.saturating_add(children.len()),
            count,
            is_root,
            degraded,
        });
    }
    stack.extend(children.into_iter().rev().map(|node| Work::Visit {
        node,
        is_root: false,
    }));
    Ok(())
}

fn node_fragments(
    node: &impl SnapshotNode,
    is_root: bool,
    class: SnapshotNodeClass,
    context: &mut WalkContext<'_>,
) -> Result<(Vec<String>, bool), SnapshotWalkError> {
    let mut fragments = Vec::new();
    let mut degraded = false;
    let content = if matches!(
        class,
        SnapshotNodeClass::SecureInput | SnapshotNodeClass::Menu | SnapshotNodeClass::MultiLineText
    ) {
        NodeContentAttributes::default()
    } else {
        match context.read_node(is_root, || node.content_attributes())? {
            NodeRead::Value(content) => content,
            NodeRead::Degraded => return Ok((fragments, true)),
        }
    };
    match class {
        SnapshotNodeClass::SecureInput | SnapshotNodeClass::Menu => {}
        SnapshotNodeClass::SingleLineInput | SnapshotNodeClass::Container => {
            push_unique(&mut fragments, content.title.as_deref());
        }
        SnapshotNodeClass::MultiLineText => {
            let range = match context.read_node(is_root, || node.visible_range())? {
                NodeRead::Value(range) => range,
                NodeRead::Degraded => {
                    degraded = true;
                    None
                }
            };
            if let Some(range) = range
                && context.can_continue()
            {
                match context.read_node(is_root, || node.string_for_range(range))? {
                    NodeRead::Value(text) => push_unique(&mut fragments, text.as_deref()),
                    NodeRead::Degraded => degraded = true,
                }
            }
        }
        SnapshotNodeClass::ReadableText => {
            let value = if context.can_continue() {
                match context.read_node(is_root, || node.value())? {
                    NodeRead::Value(value) => value,
                    NodeRead::Degraded => {
                        degraded = true;
                        None
                    }
                }
            } else {
                None
            };
            push_unique(&mut fragments, value.as_deref());
            push_unique(&mut fragments, content.title.as_deref());
            push_unique(&mut fragments, content.description.as_deref());
        }
        SnapshotNodeClass::Image => {
            push_unique(&mut fragments, content.description.as_deref());
        }
        SnapshotNodeClass::Unknown => {
            push_unique(&mut fragments, content.title.as_deref());
            push_unique(&mut fragments, content.description.as_deref());
        }
    }
    if degraded {
        fragments.clear();
    }
    Ok((fragments, degraded))
}

fn push_unique(fragments: &mut Vec<String>, value: Option<&str>) {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    if !fragments.iter().any(|existing| existing == value) {
        fragments.push(value.to_owned());
    }
}

struct WalkContext<'a> {
    budget: WalkBudget,
    clock: &'a dyn WalkClock,
    stopped: &'a dyn Fn() -> bool,
    nodes: usize,
    ax_calls: usize,
    text: TextAssembler,
    cutoff: Option<SnapshotCutoff>,
    degraded_nodes: usize,
    frameless_nodes: usize,
}

enum NodeRead<T> {
    Value(T),
    Degraded,
}

impl WalkContext<'_> {
    fn can_continue(&mut self) -> bool {
        if self.cutoff.is_some() {
            return false;
        }
        if (self.stopped)() {
            self.cutoff = Some(SnapshotCutoff::Stopped);
            return false;
        }
        if self.clock.elapsed() >= self.budget.wall_time {
            self.cutoff = Some(SnapshotCutoff::Time);
            return false;
        }
        true
    }

    fn read_node<T>(
        &mut self,
        is_root: bool,
        call: impl FnOnce() -> Result<T, SnapshotReadError>,
    ) -> Result<NodeRead<T>, SnapshotWalkError> {
        self.ax_calls = self.ax_calls.saturating_add(1);
        match call() {
            Ok(value) => Ok(NodeRead::Value(value)),
            Err(source) if is_root && source.ends_root_walk() => Err(SnapshotWalkError {
                source,
                nodes: self.nodes,
                elapsed: self.clock.elapsed(),
            }),
            Err(_) => Ok(NodeRead::Degraded),
        }
    }
}

fn frames_intersect(left: AxFrame, right: AxFrame) -> bool {
    let left_max_x = left.origin.x + left.size.width;
    let left_max_y = left.origin.y + left.size.height;
    let right_max_x = right.origin.x + right.size.width;
    let right_max_y = right.origin.y + right.size.height;
    left.size.width > 0.0
        && left.size.height > 0.0
        && right.size.width > 0.0
        && right.size.height > 0.0
        && left.origin.x < right_max_x
        && left_max_x > right.origin.x
        && left.origin.y < right_max_y
        && left_max_y > right.origin.y
}
