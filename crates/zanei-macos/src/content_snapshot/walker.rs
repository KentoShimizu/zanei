//! Budgeted depth-first Accessibility traversal and text assembly.

use std::{
    fmt,
    time::{Duration, Instant},
};

use crate::ffi::ax::{
    AxFrame, AxTextRange, SnapshotAttribute, SnapshotAttributeValue, SnapshotAxElement,
    SnapshotAxError,
};

use super::{
    budget::{CHILDREN_BATCH_SIZE, WalkBudget},
    role::{SnapshotNodeClass, classify_role},
};

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
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) frame: Option<AxFrame>,
}

pub(crate) trait SnapshotNode: Sized {
    fn safe_attributes(&self) -> Result<NodeSafeAttributes, SnapshotReadError>;
    fn value(&self) -> Result<Option<String>, SnapshotReadError>;
    fn visible_range(&self) -> Result<Option<AxTextRange>, SnapshotReadError>;
    fn string_for_range(&self, range: AxTextRange) -> Result<Option<String>, SnapshotReadError>;
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
}

enum Work<N> {
    Visit(N),
    Children { node: N, index: usize },
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
    };
    let mut stack = vec![Work::Visit(root)];
    while let Some(work) = stack.pop() {
        if !context.can_continue() {
            break;
        }
        match work {
            Work::Visit(node) => visit_node(node, window_frame, &mut stack, &mut context)?,
            Work::Children { node, index } => {
                load_children(node, index, &mut stack, &mut context)?;
            }
        }
    }
    Ok(SnapshotWalkOutput {
        text: context.text.finish(),
        nodes: context.nodes,
        ax_calls: context.ax_calls,
        elapsed: clock.elapsed(),
        complete: context.cutoff.is_none(),
        cutoff: context.cutoff,
    })
}

fn visit_node<'a, N: SnapshotNode>(
    node: N,
    window_frame: AxFrame,
    stack: &mut Vec<Work<N>>,
    context: &mut WalkContext<'a>,
) -> Result<(), SnapshotWalkError> {
    if context.nodes >= context.budget.nodes {
        context.cutoff = Some(SnapshotCutoff::Nodes);
        return Ok(());
    }
    context.nodes += 1;
    let attributes = context.read(|_| node.safe_attributes())?;
    if !context.can_continue()
        || attributes
            .frame
            .is_some_and(|frame| !frames_intersect(frame, window_frame))
    {
        return Ok(());
    }
    let class = classify_role(attributes.role.as_deref(), attributes.subrole.as_deref());
    let fragments = node_fragments(&node, class, &attributes, context)?;
    for fragment in fragments {
        if !context.text.push(&fragment) {
            context.cutoff = Some(SnapshotCutoff::Bytes);
            return Ok(());
        }
    }
    if class.descends() && context.can_continue() {
        stack.push(Work::Children { node, index: 0 });
    }
    Ok(())
}

fn load_children<'a, N: SnapshotNode>(
    node: N,
    index: usize,
    stack: &mut Vec<Work<N>>,
    context: &mut WalkContext<'a>,
) -> Result<(), SnapshotWalkError> {
    let children = context.read(|_| node.children_range(index, CHILDREN_BATCH_SIZE))?;
    if !context.can_continue() {
        return Ok(());
    }
    if children.len() == CHILDREN_BATCH_SIZE {
        stack.push(Work::Children {
            node,
            index: index.saturating_add(children.len()),
        });
    }
    stack.extend(children.into_iter().rev().map(Work::Visit));
    Ok(())
}

fn node_fragments(
    node: &impl SnapshotNode,
    class: SnapshotNodeClass,
    attributes: &NodeSafeAttributes,
    context: &mut WalkContext<'_>,
) -> Result<Vec<String>, SnapshotWalkError> {
    let mut fragments = Vec::new();
    match class {
        SnapshotNodeClass::SecureInput | SnapshotNodeClass::Menu => {}
        SnapshotNodeClass::SingleLineInput | SnapshotNodeClass::Container => {
            push_unique(&mut fragments, attributes.title.as_deref());
        }
        SnapshotNodeClass::MultiLineText => {
            let range = context.read(|_| node.visible_range())?;
            if let Some(range) = range
                && context.can_continue()
            {
                let text = context.read(|_| node.string_for_range(range))?;
                push_unique(&mut fragments, text.as_deref());
            }
        }
        SnapshotNodeClass::ReadableText => {
            let value = if context.can_continue() {
                context.read(|_| node.value())?
            } else {
                None
            };
            push_unique(&mut fragments, value.as_deref());
            push_unique(&mut fragments, attributes.title.as_deref());
            push_unique(&mut fragments, attributes.description.as_deref());
        }
        SnapshotNodeClass::Image => {
            push_unique(&mut fragments, attributes.description.as_deref());
        }
        SnapshotNodeClass::Unknown => {
            push_unique(&mut fragments, attributes.title.as_deref());
            push_unique(&mut fragments, attributes.description.as_deref());
        }
    }
    Ok(fragments)
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

    fn read<T>(
        &mut self,
        call: impl FnOnce(&mut Self) -> Result<T, SnapshotReadError>,
    ) -> Result<T, SnapshotWalkError> {
        self.ax_calls = self.ax_calls.saturating_add(1);
        call(self).map_err(|source| SnapshotWalkError {
            source,
            nodes: self.nodes,
            elapsed: self.clock.elapsed(),
        })
    }
}

struct TextAssembler {
    text: String,
    max_bytes: usize,
    previous_empty: bool,
}

impl TextAssembler {
    fn new(max_bytes: usize) -> Self {
        Self {
            text: String::new(),
            max_bytes,
            previous_empty: false,
        }
    }

    fn push(&mut self, fragment: &str) -> bool {
        let fragment = fragment.trim();
        if fragment.is_empty() {
            return true;
        }
        for line in fragment.lines() {
            let line = line.trim_end_matches('\r');
            let empty = line.trim().is_empty();
            if empty && self.previous_empty {
                continue;
            }
            if !self.text.is_empty() && !self.append("\n") {
                return false;
            }
            if !empty && !self.append(line) {
                return false;
            }
            self.previous_empty = empty;
        }
        true
    }

    fn append(&mut self, value: &str) -> bool {
        let remaining = self.max_bytes.saturating_sub(self.text.len());
        if value.len() <= remaining {
            self.text.push_str(value);
            return true;
        }
        let mut boundary = remaining.min(value.len());
        while boundary > 0 && !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        self.text.push_str(&value[..boundary]);
        false
    }

    fn finish(mut self) -> String {
        self.text.truncate(self.text.trim_end().len());
        self.text
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

impl SnapshotNode for SnapshotAxElement {
    fn safe_attributes(&self) -> Result<NodeSafeAttributes, SnapshotReadError> {
        let values = self.copy_multiple(&[
            SnapshotAttribute::Role,
            SnapshotAttribute::Subrole,
            SnapshotAttribute::Title,
            SnapshotAttribute::Description,
            SnapshotAttribute::Position,
            SnapshotAttribute::Size,
        ])?;
        decode_safe_attributes(values)
    }

    fn value(&self) -> Result<Option<String>, SnapshotReadError> {
        let mut values = self.copy_multiple(&[SnapshotAttribute::Value])?.into_iter();
        text_result(values.next(), "AXValue result")
    }

    fn visible_range(&self) -> Result<Option<AxTextRange>, SnapshotReadError> {
        self.visible_character_range().map_err(Into::into)
    }

    fn string_for_range(&self, range: AxTextRange) -> Result<Option<String>, SnapshotReadError> {
        self.string_for_range(range).map_err(Into::into)
    }

    fn children_range(
        &self,
        index: usize,
        maximum_count: usize,
    ) -> Result<Vec<Self>, SnapshotReadError> {
        self.children_range(index, maximum_count)
            .map_err(Into::into)
    }
}

fn decode_safe_attributes(
    values: Vec<Result<Option<SnapshotAttributeValue>, SnapshotAxError>>,
) -> Result<NodeSafeAttributes, SnapshotReadError> {
    if values.len() != 6 {
        return Err(SnapshotReadError::Contract(
            "AX safe attribute result count",
        ));
    }
    let mut values = values.into_iter();
    let role = text_result(values.next(), "AXRole result")?;
    let subrole = text_result(values.next(), "AXSubrole result")?;
    let title = text_result(values.next(), "AXTitle result")?;
    let description = text_result(values.next(), "AXDescription result")?;
    let position = point_result(values.next())?;
    let size = size_result(values.next())?;
    let frame = match (position, size) {
        (Some(origin), Some(size)) => Some(AxFrame { origin, size }),
        (None, None) => None,
        _ => return Err(SnapshotReadError::Contract("AX node frame result")),
    };
    Ok(NodeSafeAttributes {
        role,
        subrole,
        title,
        description,
        frame,
    })
}

fn text_result(
    result: Option<Result<Option<SnapshotAttributeValue>, SnapshotAxError>>,
    missing: &'static str,
) -> Result<Option<String>, SnapshotReadError> {
    match result.ok_or(SnapshotReadError::Contract(missing))?? {
        Some(SnapshotAttributeValue::Text(value)) => Ok(Some(value)),
        None => Ok(None),
        Some(_) => Err(SnapshotReadError::Contract("AX text attribute type")),
    }
}

fn point_result(
    result: Option<Result<Option<SnapshotAttributeValue>, SnapshotAxError>>,
) -> Result<Option<crate::ffi::ax::AxPoint>, SnapshotReadError> {
    match result.ok_or(SnapshotReadError::Contract("AXPosition result"))?? {
        Some(SnapshotAttributeValue::Point(value)) => Ok(Some(value)),
        None => Ok(None),
        Some(_) => Err(SnapshotReadError::Contract("AXPosition attribute type")),
    }
}

fn size_result(
    result: Option<Result<Option<SnapshotAttributeValue>, SnapshotAxError>>,
) -> Result<Option<crate::ffi::ax::AxSize>, SnapshotReadError> {
    match result.ok_or(SnapshotReadError::Contract("AXSize result"))?? {
        Some(SnapshotAttributeValue::Size(value)) => Ok(Some(value)),
        None => Ok(None),
        Some(_) => Err(SnapshotReadError::Contract("AXSize attribute type")),
    }
}
