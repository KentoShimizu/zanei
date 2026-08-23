//! Capture trigger surface used by the Stage C content worker.

mod trigger;

pub use trigger::{
    SnapshotTrigger, SnapshotTriggerKind, SnapshotTriggerPublisher, SnapshotTriggerReceiver,
    snapshot_trigger_channel,
};

pub use crate::ffi::activity::{ActivityError, seconds_since_last_input};
pub use crate::ffi::ax::{
    AxFrame, AxPoint, AxSize, AxTextRange, SnapshotAttribute, SnapshotAttributeResult,
    SnapshotAttributeValue, SnapshotAxApplication, SnapshotAxElement, SnapshotAxError,
};
