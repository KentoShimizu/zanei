use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::schema::{
    App, ClipboardOrigin, Event, EventData, Redaction, Window, is_known_event_type,
};

use super::StoreError;

// A known row is immediately moved into QueryResult. Boxing it would add one heap allocation for
// every stored event solely to reduce this short-lived decoder enum's stack size.
#[allow(clippy::large_enum_variant)]
pub(crate) enum DecodedEventRow {
    Known(Event),
    UnknownType,
}

pub(crate) fn decode(row: &rusqlite::Row<'_>) -> Result<DecodedEventRow, StoreError> {
    let event_type: String = row.get(4)?;
    if !is_known_event_type(&event_type) {
        return Ok(DecodedEventRow::UnknownType);
    }

    let data_json: String = row.get(11)?;
    let data_value = serde_json::from_str(&data_json)
        .map_err(|error| StoreError::invalid_json("data_json", error))?;
    let data = EventData::from_type_and_value(&event_type, data_value)
        .map_err(|error| StoreError::invalid_json("data_json", error))?;
    let element = deserialize_optional(row.get(10)?, "element_json")?;
    let redaction_json: String = row.get(12)?;
    let redaction = serde_json::from_str::<Redaction>(&redaction_json)
        .map_err(|error| StoreError::invalid_json("redaction_json", error))?;
    let window_title: Option<String> = row.get(8)?;
    let window_id: Option<i64> = row.get(9)?;
    let window = (window_title.is_some() || window_id.is_some() || requires_window(&data))
        .then_some(Window {
            title: window_title,
            id: window_id,
        });
    let ts: String = row.get(1)?;
    OffsetDateTime::parse(&ts, &Rfc3339)
        .map_err(|_| StoreError::invalid_timestamp("event.ts", ts.clone()))?;

    let event = Event {
        version: crate::schema::EVENT_SCHEMA_VERSION,
        id: row.get(0)?,
        ts,
        mono_ns: unsigned("mono_ns", row.get(2)?)?,
        source: row.get(3)?,
        event_type,
        app: App {
            bundle_id: row.get(5)?,
            name: row.get(6)?,
            pid: row.get(7)?,
        },
        window,
        element,
        data,
        redaction,
    };
    serde_json::to_value(&event).map_err(|error| StoreError::invalid_json("event", error))?;
    Ok(DecodedEventRow::Known(event))
}

fn deserialize_optional<T: serde::de::DeserializeOwned>(
    json: Option<String>,
    field: &'static str,
) -> Result<Option<T>, StoreError> {
    json.as_deref()
        .map(|json| {
            serde_json::from_str(json).map_err(|error| StoreError::invalid_json(field, error))
        })
        .transpose()
}

fn requires_window(data: &EventData) -> bool {
    match data {
        EventData::WindowFocus(_)
        | EventData::WindowTitle(_)
        | EventData::UiFocus(_)
        | EventData::UiClick(_)
        | EventData::UiValue(_)
        | EventData::InputKey(_)
        | EventData::InputScroll(_)
        | EventData::BrowserNavigate(_)
        | EventData::ClipboardPaste(_) => true,
        EventData::ClipboardCopy(data) => data.origin == ClipboardOrigin::CopyShortcut,
        EventData::AppActivate(_) | EventData::AppLaunch(_) | EventData::AppTerminate(_) => false,
    }
}

fn unsigned(field: &'static str, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::NumericOverflow(field))
}
