//! Canonical routing for events that may carry captured text.

use time::OffsetDateTime;
use zanei_collector::RawEvent;
use zanei_core::{
    privacy::{CHROME_BUNDLE_ID, suppress_text_content},
    schema::EventData,
};

use crate::capture_policy::CaptureDecision;

use super::ChromeWindowKey;

pub(crate) enum TextBodyRoute {
    Send(RawEvent),
    Quarantine {
        event: RawEvent,
        key: ChromeWindowKey,
        version: u64,
        observed_at: OffsetDateTime,
    },
}

pub(crate) fn route_text_body(
    mut event: RawEvent,
    decision: Option<&CaptureDecision>,
) -> TextBodyRoute {
    let has_body = has_text_body(&event);
    let Some(decision) = decision else {
        if has_body {
            suppress_text_content(&mut event.data, &mut event.element);
        }
        return TextBodyRoute::Send(event);
    };
    event.capture_context = decision.capture_context();
    if !has_body {
        return TextBodyRoute::Send(event);
    }
    if !decision.is_allowed() {
        suppress_text_content(&mut event.data, &mut event.element);
        return TextBodyRoute::Send(event);
    }
    if event.app.bundle_id.as_deref() != Some(CHROME_BUNDLE_ID) {
        return TextBodyRoute::Send(event);
    }
    let Some((((version, pid), window_id), observed_at)) = decision
        .chrome_version()
        .zip(event.app.pid)
        .zip(event.window.as_ref().and_then(|window| window.id))
        .zip(event.observed_at)
    else {
        suppress_text_content(&mut event.data, &mut event.element);
        return TextBodyRoute::Send(event);
    };
    TextBodyRoute::Quarantine {
        event,
        key: ChromeWindowKey { pid, window_id },
        version,
        observed_at,
    }
}

fn has_text_body(event: &RawEvent) -> bool {
    match &event.data {
        EventData::InputKey(data) => data.text.is_some(),
        EventData::ClipboardCopy(data) => data.text.is_some() || data.size_bytes.is_some(),
        EventData::ClipboardPaste(data) => data.text.is_some() || data.size_bytes.is_some(),
        EventData::UiValue(data) => {
            data.text.is_some()
                || event
                    .element
                    .as_ref()
                    .and_then(|element| element.value.as_ref())
                    .is_some()
        }
        EventData::AppActivate(_)
        | EventData::AppLaunch(_)
        | EventData::AppTerminate(_)
        | EventData::WindowFocus(_)
        | EventData::WindowTitle(_)
        | EventData::UiFocus(_)
        | EventData::UiClick(_)
        | EventData::InputScroll(_)
        | EventData::BrowserNavigate(_)
        | EventData::ContentSnapshot(_) => false,
    }
}
