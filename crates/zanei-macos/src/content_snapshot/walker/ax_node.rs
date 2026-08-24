//! Native Accessibility node decoding for the platform-neutral walker.

use crate::ffi::ax::{
    AxFrame, AxTextRange, SnapshotAttribute, SnapshotAttributeValue, SnapshotAxElement,
    SnapshotAxError,
};

use super::{NodeContentAttributes, NodeSafeAttributes, SnapshotNode, SnapshotReadError};

impl SnapshotNode for SnapshotAxElement {
    fn safe_attributes(&self) -> Result<NodeSafeAttributes, SnapshotReadError> {
        let values = self.copy_multiple(&[
            SnapshotAttribute::Role,
            SnapshotAttribute::Subrole,
            SnapshotAttribute::Position,
            SnapshotAttribute::Size,
        ])?;
        decode_safe_attributes(values)
    }

    fn content_attributes(&self) -> Result<NodeContentAttributes, SnapshotReadError> {
        let values =
            self.copy_multiple(&[SnapshotAttribute::Title, SnapshotAttribute::Description])?;
        decode_content_attributes(values)
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

    fn children_count(&self) -> Result<usize, SnapshotReadError> {
        self.children_count().map_err(Into::into)
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
    if values.len() != 4 {
        return Err(SnapshotReadError::Contract(
            "AX safe attribute result count",
        ));
    }
    let mut values = values.into_iter();
    let role = text_result(values.next(), "AXRole result")?;
    let subrole = text_result(values.next(), "AXSubrole result")?;
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
        frame,
    })
}

fn decode_content_attributes(
    values: Vec<Result<Option<SnapshotAttributeValue>, SnapshotAxError>>,
) -> Result<NodeContentAttributes, SnapshotReadError> {
    if values.len() != 2 {
        return Err(SnapshotReadError::Contract(
            "AX content attribute result count",
        ));
    }
    let mut values = values.into_iter();
    Ok(NodeContentAttributes {
        title: text_result(values.next(), "AXTitle result")?,
        description: text_result(values.next(), "AXDescription result")?,
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
