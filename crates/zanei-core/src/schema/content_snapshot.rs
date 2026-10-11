use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::payload::deserialize_required_nullable;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentSnapshotTrigger {
    Settle,
    Refresh,
    FocusOut,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentSnapshotCutoff {
    Time,
    Nodes,
    Bytes,
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentSnapshotData {
    pub text: Option<String>,
    pub chars: u64,
    pub trigger: ContentSnapshotTrigger,
    completion: ContentSnapshotCompletion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContentSnapshotCompletion {
    Legacy {
        complete: bool,
    },
    Current {
        cutoff: Option<ContentSnapshotCutoff>,
    },
}

impl ContentSnapshotData {
    #[must_use]
    pub fn new(
        text: Option<String>,
        chars: u64,
        cutoff: Option<ContentSnapshotCutoff>,
        trigger: ContentSnapshotTrigger,
    ) -> Self {
        Self {
            text,
            chars,
            trigger,
            completion: ContentSnapshotCompletion::Current { cutoff },
        }
    }

    /// Distinguishes a retained v2 payload (`None`) from both a completed v3
    /// payload (`Some(None)`) and a cut-off v3 payload (`Some(Some(reason))`).
    #[must_use]
    pub const fn cutoff(&self) -> Option<Option<ContentSnapshotCutoff>> {
        match self.completion {
            ContentSnapshotCompletion::Legacy { .. } => None,
            ContentSnapshotCompletion::Current { cutoff } => Some(cutoff),
        }
    }

    /// Returns the completion marker retained from a v2 payload. Current v3
    /// payloads return `None` because `cutoff` is their single source of truth.
    #[must_use]
    pub const fn legacy_complete(&self) -> Option<bool> {
        match self.completion {
            ContentSnapshotCompletion::Legacy { complete } => Some(complete),
            ContentSnapshotCompletion::Current { .. } => None,
        }
    }

    pub(super) const fn is_legacy(&self) -> bool {
        matches!(self.completion, ContentSnapshotCompletion::Legacy { .. })
    }
}

impl<'de> Deserialize<'de> for ContentSnapshotData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match ContentSnapshotWireData::deserialize(deserializer)? {
            ContentSnapshotWireData::Current(data) => {
                Ok(Self::new(data.text, data.chars, data.cutoff, data.trigger))
            }
            ContentSnapshotWireData::Legacy(data) => Ok(Self {
                text: data.text,
                chars: data.chars,
                trigger: data.trigger,
                completion: ContentSnapshotCompletion::Legacy {
                    complete: data.complete,
                },
            }),
        }
    }
}

impl Serialize for ContentSnapshotData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.completion {
            ContentSnapshotCompletion::Legacy { complete } => LegacyContentSnapshotDataRef {
                text: &self.text,
                chars: self.chars,
                complete,
                trigger: self.trigger,
            }
            .serialize(serializer),
            ContentSnapshotCompletion::Current { cutoff } => CurrentContentSnapshotDataRef {
                text: &self.text,
                chars: self.chars,
                cutoff,
                trigger: self.trigger,
            }
            .serialize(serializer),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ContentSnapshotWireData {
    Current(CurrentContentSnapshotData),
    Legacy(LegacyContentSnapshotData),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentContentSnapshotData {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    text: Option<String>,
    chars: u64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    cutoff: Option<ContentSnapshotCutoff>,
    trigger: ContentSnapshotTrigger,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyContentSnapshotData {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    text: Option<String>,
    chars: u64,
    complete: bool,
    trigger: ContentSnapshotTrigger,
}

#[derive(Serialize)]
struct CurrentContentSnapshotDataRef<'a> {
    text: &'a Option<String>,
    chars: u64,
    cutoff: Option<ContentSnapshotCutoff>,
    trigger: ContentSnapshotTrigger,
}

#[derive(Serialize)]
struct LegacyContentSnapshotDataRef<'a> {
    text: &'a Option<String>,
    chars: u64,
    complete: bool,
    trigger: ContentSnapshotTrigger,
}
