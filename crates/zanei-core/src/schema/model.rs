use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::EventData;

const LEGACY_EVENT_SCHEMA_VERSION: u8 = 1;
const LEGACY_CONTENT_SNAPSHOT_SCHEMA_VERSION: u8 = 2;
const CONTENT_SNAPSHOT_SCHEMA_VERSION: u8 = 3;
pub const KNOWN_EVENT_TYPES: [&str; 14] = [
    "app.activate",
    "app.launch",
    "app.terminate",
    "window.focus",
    "window.title",
    "ui.focus",
    "ui.click",
    "ui.value",
    "input.key",
    "input.scroll",
    "browser.navigate",
    "clipboard.copy",
    "clipboard.paste",
    "content.snapshot",
];

#[must_use]
pub fn event_schema_version(event_type: &str) -> Option<u8> {
    match event_type {
        "app.activate" | "app.launch" | "app.terminate" | "window.focus" | "window.title"
        | "ui.focus" | "ui.click" | "ui.value" | "input.key" | "input.scroll"
        | "browser.navigate" | "clipboard.copy" | "clipboard.paste" => {
            Some(LEGACY_EVENT_SCHEMA_VERSION)
        }
        "content.snapshot" => Some(CONTENT_SNAPSHOT_SCHEMA_VERSION),
        _ => None,
    }
}

fn supports_event_schema_version(event_type: &str, version: u8) -> bool {
    match event_type {
        "content.snapshot" => matches!(
            version,
            LEGACY_CONTENT_SNAPSHOT_SCHEMA_VERSION | CONTENT_SNAPSHOT_SCHEMA_VERSION
        ),
        _ => event_schema_version(event_type) == Some(version),
    }
}

pub(crate) fn event_schema_version_for_data(event_type: &str, data: &EventData) -> Option<u8> {
    match (event_type, data) {
        ("content.snapshot", EventData::ContentSnapshot(data)) if data.is_legacy() => {
            Some(LEGACY_CONTENT_SNAPSHOT_SCHEMA_VERSION)
        }
        _ => event_schema_version(event_type),
    }
}

pub fn is_known_event_type(event_type: &str) -> bool {
    KNOWN_EVENT_TYPES.contains(&event_type)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    pub name: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub bundle_id: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub pid: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub title: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub id: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Element {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub role: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub title: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub value: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Redaction {
    pub applied: bool,
    pub rules: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub version: u8,
    pub id: String,
    pub ts: String,
    pub mono_ns: u64,
    pub source: String,
    pub event_type: String,
    pub app: App,
    pub window: Option<Window>,
    pub element: Option<Element>,
    pub data: EventData,
    pub redaction: Redaction,
}

impl Event {
    pub(crate) const SIZE_LIMIT_RULE: &'static str = "size_limit";

    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.redaction
            .rules
            .iter()
            .any(|rule| rule == Self::SIZE_LIMIT_RULE)
    }

    pub(crate) fn mark_truncated(&mut self) {
        if !self.is_truncated() {
            self.redaction
                .rules
                .insert(0, Self::SIZE_LIMIT_RULE.to_owned());
        }
        self.redaction.applied = true;
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let envelope = EventEnvelope::deserialize(deserializer)?;
        if !supports_event_schema_version(&envelope.event_type, envelope.version) {
            return Err(serde::de::Error::custom(format!(
                "event type {} does not use schema version {}",
                envelope.event_type, envelope.version
            )));
        }
        let rule_marks_truncation = envelope
            .redaction
            .rules
            .iter()
            .any(|rule| rule == Self::SIZE_LIMIT_RULE);
        if envelope.truncated != rule_marks_truncation {
            return Err(serde::de::Error::custom(
                "event truncated marker must match the size_limit redaction rule",
            ));
        }
        let data = EventData::from_type_and_value(&envelope.event_type, envelope.data)
            .map_err(serde::de::Error::custom)?;
        let event = Self {
            version: envelope.version,
            id: envelope.id,
            ts: envelope.ts,
            mono_ns: envelope.mono_ns,
            source: envelope.source,
            event_type: envelope.event_type,
            app: envelope.app,
            window: envelope.window,
            element: envelope.element,
            data,
            redaction: envelope.redaction,
        };
        event.validate().map_err(serde::de::Error::custom)?;
        Ok(event)
    }
}

impl Serialize for Event {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if !supports_event_schema_version(&self.event_type, self.version) {
            return Err(serde::ser::Error::custom(format!(
                "event type {} does not use schema version {}",
                self.event_type, self.version
            )));
        }
        self.validate().map_err(serde::ser::Error::custom)?;

        EventRef {
            version: self.version,
            id: &self.id,
            ts: &self.ts,
            mono_ns: self.mono_ns,
            source: &self.source,
            event_type: &self.event_type,
            app: &self.app,
            window: &self.window,
            element: &self.element,
            data: &self.data,
            truncated: self.is_truncated(),
            redaction: &self.redaction,
        }
        .serialize(serializer)
    }
}

impl Event {
    fn validate(&self) -> Result<(), &'static str> {
        let ulid = self
            .id
            .strip_prefix("evt_")
            .ok_or("event id must start with evt_")?;
        if !is_valid_ulid_text(ulid) || ulid.parse::<ulid::Ulid>().is_err() {
            return Err("event id must contain a valid ULID");
        }
        OffsetDateTime::parse(&self.ts, &Rfc3339).map_err(|_| "event ts must be RFC3339")?;
        if !is_dotted_identifier(&self.source) {
            return Err("event source must be a dotted identifier");
        }
        if !is_dotted_identifier(&self.event_type) {
            return Err("event type must be a dotted identifier");
        }
        if !is_known_event_type(&self.event_type) {
            return Err("unknown event type");
        }
        if !self.data.matches_event_type(&self.event_type) {
            return Err("event type does not match its typed payload");
        }
        if event_schema_version_for_data(&self.event_type, &self.data) != Some(self.version) {
            return Err("event payload does not match its schema version");
        }
        self.validate_context()?;
        if let EventData::BrowserNavigate(data) = &self.data {
            match data.url.as_deref() {
                Some(url) if is_absolute_uri(url) => {}
                Some(_) => return Err("browser URL must be an absolute URI"),
                None if self.is_truncated() => {}
                None => return Err("browser URL may be null only when the event is truncated"),
            }
        }
        if let EventData::ContentSnapshot(data) = &self.data
            && data.text.is_none()
            && !self.is_truncated()
        {
            return Err("content snapshot text may be null only when the event is truncated");
        }
        Ok(())
    }

    fn validate_context(&self) -> Result<(), &'static str> {
        let has_window = self.window.is_some();
        let has_element = self.element.is_some();
        let valid = match &self.data {
            EventData::AppActivate(_) => !has_element,
            EventData::AppLaunch(_) | EventData::AppTerminate(_) => !has_window && !has_element,
            EventData::WindowFocus(_) | EventData::WindowTitle(_) => has_window && !has_element,
            EventData::UiFocus(_) | EventData::UiClick(_) | EventData::UiValue(_) => {
                has_window && has_element
            }
            EventData::InputKey(_)
            | EventData::InputScroll(_)
            | EventData::BrowserNavigate(_)
            | EventData::ClipboardPaste(_)
            | EventData::ContentSnapshot(_) => has_window && !has_element,
            EventData::ClipboardCopy(data) => match data.origin {
                super::ClipboardOrigin::CopyShortcut => has_window && !has_element,
                super::ClipboardOrigin::Unknown => {
                    !has_window
                        && !has_element
                        && self.app.name == "Unknown"
                        && self.app.bundle_id.is_none()
                        && self.app.pid.is_none()
                        && data.size_bytes.is_none()
                        && data.text.is_none()
                }
            },
        };
        if valid {
            Ok(())
        } else {
            Err("window, element, or clipboard attribution does not match the event type")
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventEnvelope {
    #[serde(rename = "v")]
    version: u8,
    id: String,
    ts: String,
    mono_ns: u64,
    source: String,
    #[serde(rename = "type")]
    event_type: String,
    app: App,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    window: Option<Window>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    element: Option<Element>,
    data: Value,
    truncated: bool,
    redaction: Redaction,
}

fn is_dotted_identifier(value: &str) -> bool {
    let Some((family, name)) = value.split_once('.') else {
        return false;
    };
    !family.is_empty()
        && !name.is_empty()
        && !name.contains('.')
        && family
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn is_valid_ulid_text(value: &str) -> bool {
    value.len() == 26
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| matches!(byte, b'0'..=b'7'))
        && value.bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(byte, b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
        })
}

fn is_absolute_uri(value: &str) -> bool {
    let Some((scheme, remainder)) = value.split_once(':') else {
        return false;
    };
    !scheme.is_empty()
        && scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic()
                || (index > 0 && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')))
        })
        && !remainder.is_empty()
        && !value.chars().any(char::is_whitespace)
}

#[derive(Serialize)]
struct EventRef<'a> {
    #[serde(rename = "v")]
    version: u8,
    id: &'a str,
    ts: &'a str,
    mono_ns: u64,
    source: &'a str,
    #[serde(rename = "type")]
    event_type: &'a str,
    app: &'a App,
    window: &'a Option<Window>,
    element: &'a Option<Element>,
    data: &'a EventData,
    truncated: bool,
    redaction: &'a Redaction,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CaptureContext {
    pub website_host: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RawEvent {
    /// Source-observed wall time. Normalization falls back to ingestion time when absent.
    pub observed_at: Option<OffsetDateTime>,
    pub source: String,
    pub event_type: String,
    pub app: App,
    pub window: Option<Window>,
    pub element: Option<Element>,
    pub data: EventData,
    pub capture_context: CaptureContext,
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}
