use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    Search,
    Url,
    Email,
    Number,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Cmd,
    Shift,
    Opt,
    Ctrl,
    Fn,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClickButton {
    Left,
    Right,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKeyKind {
    Text,
    Shortcut,
    Navigation,
    Delete,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserMode {
    Normal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserTransition {
    Navigate,
    TabSwitch,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BrowserUrl(Option<String>);

impl BrowserUrl {
    #[must_use]
    pub fn as_deref(&self) -> Option<&str> {
        self.0.as_deref()
    }

    pub(crate) fn as_option_mut(&mut self) -> &mut Option<String> {
        &mut self.0
    }
}

impl From<String> for BrowserUrl {
    fn from(value: String) -> Self {
        Self(Some(value))
    }
}

impl PartialEq<&str> for BrowserUrl {
    fn eq(&self, other: &&str) -> bool {
        self.as_deref() == Some(*other)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    Image,
    File,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardOrigin {
    CopyShortcut,
    Unknown,
}

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

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyData {}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WindowTitleData {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub prev_title: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UiFocusData {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub field_kind: Option<FieldKind>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UiClickData {
    pub button: ClickButton,
    #[serde(
        deserialize_with = "deserialize_positive_u64",
        serialize_with = "serialize_positive_u64"
    )]
    pub click_count: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UiValueData {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub field_kind: Option<FieldKind>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub value_len: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub text: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputKeyData {
    pub kind: InputKeyKind,
    #[serde(
        deserialize_with = "deserialize_normalized_modifiers",
        serialize_with = "serialize_unique_modifiers"
    )]
    pub modifiers: Vec<Modifier>,
    #[serde(
        deserialize_with = "deserialize_positive_u64",
        serialize_with = "serialize_positive_u64"
    )]
    pub count: u64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub combo: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub text: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub field_kind: Option<FieldKind>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputScrollData {
    pub direction: ScrollDirection,
    #[serde(
        deserialize_with = "deserialize_finite_f64",
        serialize_with = "serialize_finite_f64"
    )]
    pub amount: f64,
    #[serde(
        deserialize_with = "deserialize_positive_u64",
        serialize_with = "serialize_positive_u64"
    )]
    pub count: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserNavigateData {
    pub url: BrowserUrl,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub tab_title: Option<String>,
    pub mode: BrowserMode,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub transition: Option<BrowserTransition>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClipboardCopyData {
    pub origin: ClipboardOrigin,
    pub content_kind: ContentKind,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub size_bytes: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub text: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClipboardPasteData {
    pub content_kind: ContentKind,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub size_bytes: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub text: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub field_kind: Option<FieldKind>,
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

#[derive(Clone, Debug, PartialEq)]
pub enum EventData {
    AppActivate(EmptyData),
    AppLaunch(EmptyData),
    AppTerminate(EmptyData),
    WindowFocus(EmptyData),
    WindowTitle(WindowTitleData),
    UiFocus(UiFocusData),
    UiClick(UiClickData),
    UiValue(UiValueData),
    InputKey(InputKeyData),
    InputScroll(InputScrollData),
    BrowserNavigate(BrowserNavigateData),
    ClipboardCopy(ClipboardCopyData),
    ClipboardPaste(ClipboardPasteData),
    ContentSnapshot(ContentSnapshotData),
}

impl EventData {
    pub fn from_type_and_value(event_type: &str, value: Value) -> Result<Self, serde_json::Error> {
        if !value.is_object() {
            return Err(<serde_json::Error as serde::de::Error>::custom(
                "event data must be a JSON object",
            ));
        }

        match event_type {
            "app.activate" => from_value(value, Self::AppActivate),
            "app.launch" => from_value(value, Self::AppLaunch),
            "app.terminate" => from_value(value, Self::AppTerminate),
            "window.focus" => from_value(value, Self::WindowFocus),
            "window.title" => from_value(value, Self::WindowTitle),
            "ui.focus" => from_value(value, Self::UiFocus),
            "ui.click" => from_value(value, Self::UiClick),
            "ui.value" => from_value(value, Self::UiValue),
            "input.key" => from_value(value, Self::InputKey),
            "input.scroll" => from_value(value, Self::InputScroll),
            "browser.navigate" => from_value(value, Self::BrowserNavigate),
            "clipboard.copy" => from_value(value, Self::ClipboardCopy),
            "clipboard.paste" => from_value(value, Self::ClipboardPaste),
            "content.snapshot" => from_value(value, Self::ContentSnapshot),
            _ => Err(<serde_json::Error as serde::de::Error>::custom(format!(
                "unknown event type: {event_type}"
            ))),
        }
    }

    pub fn event_type(&self) -> &'static str {
        match self {
            Self::AppActivate(_) => "app.activate",
            Self::AppLaunch(_) => "app.launch",
            Self::AppTerminate(_) => "app.terminate",
            Self::WindowFocus(_) => "window.focus",
            Self::WindowTitle(_) => "window.title",
            Self::UiFocus(_) => "ui.focus",
            Self::UiClick(_) => "ui.click",
            Self::UiValue(_) => "ui.value",
            Self::InputKey(_) => "input.key",
            Self::InputScroll(_) => "input.scroll",
            Self::BrowserNavigate(_) => "browser.navigate",
            Self::ClipboardCopy(_) => "clipboard.copy",
            Self::ClipboardPaste(_) => "clipboard.paste",
            Self::ContentSnapshot(_) => "content.snapshot",
        }
    }

    pub(crate) fn matches_event_type(&self, event_type: &str) -> bool {
        self.event_type() == event_type
    }
}

impl Serialize for EventData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::AppActivate(value)
            | Self::AppLaunch(value)
            | Self::AppTerminate(value)
            | Self::WindowFocus(value) => value.serialize(serializer),
            Self::WindowTitle(value) => value.serialize(serializer),
            Self::UiFocus(value) => value.serialize(serializer),
            Self::UiClick(value) => value.serialize(serializer),
            Self::UiValue(value) => value.serialize(serializer),
            Self::InputKey(value) => value.serialize(serializer),
            Self::InputScroll(value) => value.serialize(serializer),
            Self::BrowserNavigate(value) => value.serialize(serializer),
            Self::ClipboardCopy(value) => value.serialize(serializer),
            Self::ClipboardPaste(value) => value.serialize(serializer),
            Self::ContentSnapshot(value) => value.serialize(serializer),
        }
    }
}

fn from_value<T>(
    value: Value,
    wrap: impl FnOnce(T) -> EventData,
) -> Result<EventData, serde_json::Error>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(value).map(wrap)
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn deserialize_normalized_modifiers<'de, D>(deserializer: D) -> Result<Vec<Modifier>, D::Error>
where
    D: Deserializer<'de>,
{
    let mut modifiers = Vec::<Modifier>::deserialize(deserializer)?;
    modifiers.sort_unstable_by_key(|modifier| modifier_rank(*modifier));
    if modifiers.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(serde::de::Error::custom("modifiers must be unique"));
    }
    Ok(modifiers)
}

fn modifier_rank(modifier: Modifier) -> u8 {
    match modifier {
        Modifier::Cmd => 0,
        Modifier::Shift => 1,
        Modifier::Opt => 2,
        Modifier::Ctrl => 3,
        Modifier::Fn => 4,
    }
}

fn serialize_unique_modifiers<S>(modifiers: &[Modifier], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if modifiers
        .windows(2)
        .all(|pair| modifier_rank(pair[0]) < modifier_rank(pair[1]))
    {
        modifiers.serialize(serializer)
    } else {
        Err(serde::ser::Error::custom(
            "modifiers must be sorted and unique",
        ))
    }
}

fn deserialize_positive_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    if value == 0 {
        Err(serde::de::Error::custom("count must be at least 1"))
    } else {
        Ok(value)
    }
}

fn serialize_positive_u64<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if *value == 0 {
        Err(serde::ser::Error::custom("count must be at least 1"))
    } else {
        serializer.serialize_u64(*value)
    }
}

fn deserialize_finite_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(serde::de::Error::custom("amount must be finite"))
    }
}

fn serialize_finite_f64<S>(value: &f64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if value.is_finite() {
        serializer.serialize_f64(*value)
    } else {
        Err(serde::ser::Error::custom("amount must be finite"))
    }
}
