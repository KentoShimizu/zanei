//! Browser navigation event assembly with read-time surface provenance.

use zanei_collector::RawEvent;
use zanei_core::schema::{App, BrowserMode, BrowserNavigateData, EventData, Window};

use super::{EVENT_SOURCE, EVENT_TYPE, Navigation};
use crate::workspace::ApplicationInfo;

pub(super) fn raw_event(app: &ApplicationInfo, navigation: Navigation) -> RawEvent {
    let capture_context = zanei_core::schema::CaptureContext {
        url: Some(navigation.snapshot.url.as_str().into()),
        surface: Some(Box::new(zanei_core::schema::CaptureSurface {
            cg_window_id: navigation.snapshot.window_id,
            applescript_window_id: Some(
                navigation
                    .snapshot
                    .applescript_window_id
                    .as_str()
                    .to_owned(),
            ),
            tab_id: Some(navigation.snapshot.tab_key.clone()),
        })),
    };
    RawEvent {
        observed_at: None,
        source: EVENT_SOURCE.to_owned(),
        event_type: EVENT_TYPE.to_owned(),
        app: App {
            name: app.name.clone(),
            bundle_id: app.bundle_id.clone(),
            pid: Some(app.pid),
        },
        window: Some(Window {
            title: navigation.snapshot.window_title,
            // Chrome's AppleScript ID is a browser session ID, not CGWindowNumber.
            id: None,
        }),
        element: None,
        data: EventData::BrowserNavigate(BrowserNavigateData {
            url: navigation.snapshot.url.into(),
            tab_title: navigation.snapshot.tab_title,
            mode: BrowserMode::Normal,
            transition: navigation.transition,
        }),
        capture_context,
    }
}
