//! Conversion from native AX observations to stable raw events.

use std::collections::HashMap;

use zanei_collector::RawEvent;
use zanei_core::{
    privacy::PrivacyScope,
    schema::{
        Element, EmptyData, EventData, UiClickData, UiFocusData, UiValueData, Window,
        WindowTitleData,
    },
};

use crate::{
    capture_policy::{CaptureDecision, CapturePolicy},
    ffi::ax::{NativeAxEvent, NativeElement, NativeHitTest, NativeUiValueEvent, NativeWindow},
    focused_field::field_class,
    workspace::ApplicationInfo,
};

use super::ClickObservation;

pub(super) struct AxEvent {
    event: RawEvent,
    read_decision: Option<CaptureDecision>,
}

impl AxEvent {
    pub(super) fn new(event: RawEvent) -> Self {
        Self {
            event,
            read_decision: None,
        }
    }

    fn with_read_decision(mut self, read_decision: Option<CaptureDecision>) -> Self {
        self.read_decision = read_decision;
        self
    }

    pub(super) fn into_parts(self) -> (RawEvent, Option<CaptureDecision>) {
        (self.event, self.read_decision)
    }
}

pub(super) struct AxEventBuilder {
    apps: HashMap<i32, ApplicationInfo>,
    previous_titles: HashMap<(i32, Option<i64>), Option<String>>,
    capture_policy: CapturePolicy,
}

impl AxEventBuilder {
    pub(super) fn new(capture_policy: CapturePolicy) -> Self {
        Self {
            apps: HashMap::new(),
            previous_titles: HashMap::new(),
            capture_policy,
        }
    }

    pub(super) fn add_app(&mut self, app: ApplicationInfo) {
        if let Ok(pid) = i32::try_from(app.pid) {
            self.apps.insert(pid, app);
        }
    }

    pub(super) fn remove_app(&mut self, pid: i32) {
        self.apps.remove(&pid);
        self.previous_titles
            .retain(|(window_pid, _), _| *window_pid != pid);
    }

    pub(super) fn event(&mut self, event: NativeAxEvent) -> Option<AxEvent> {
        match event {
            NativeAxEvent::WindowFocused {
                pid,
                window,
                observed_at,
            } => {
                self.previous_titles
                    .insert((pid, window.id), window.title.clone());
                self.raw(
                    pid,
                    "window.focus",
                    Some(window),
                    None,
                    EventData::WindowFocus(EmptyData {}),
                    observed_at,
                )
            }
            NativeAxEvent::WindowTitleChanged {
                pid,
                window,
                observed_at,
            } => {
                let previous = self
                    .previous_titles
                    .insert((pid, window.id), window.title.clone())
                    .flatten();
                self.raw(
                    pid,
                    "window.title",
                    Some(window),
                    None,
                    EventData::WindowTitle(WindowTitleData {
                        prev_title: previous,
                    }),
                    observed_at,
                )
            }
            NativeAxEvent::UiFocused {
                pid,
                window,
                element,
                observed_at,
                ..
            } => {
                let Some(element) = element else {
                    crate::trace::trace!(
                        "component=ax phase=builder action=drop pid={} event=ui.focus reason=missing_element",
                        pid
                    );
                    return None;
                };
                let kind =
                    field_class(element.role.as_deref(), element.subrole.as_deref()).field_kind();
                self.raw(
                    pid,
                    "ui.focus",
                    window,
                    Some(element),
                    EventData::UiFocus(UiFocusData { field_kind: kind }),
                    observed_at,
                )
            }
            NativeAxEvent::UiValueChanged(event) => {
                let NativeUiValueEvent {
                    pid,
                    window,
                    element,
                    text,
                    capture_decision,
                    observed_at,
                } = *event;
                let value_len = element.value_len;
                let kind =
                    field_class(element.role.as_deref(), element.subrole.as_deref()).field_kind();
                self.raw(
                    pid,
                    "ui.value",
                    window,
                    Some(element),
                    EventData::UiValue(UiValueData {
                        field_kind: kind,
                        value_len,
                        text,
                    }),
                    observed_at,
                )
                .map(|event| event.with_read_decision(capture_decision))
            }
            NativeAxEvent::PageLoaded { .. } => None,
        }
    }

    pub(super) fn click_event(
        &self,
        hit: NativeHitTest,
        click: ClickObservation,
    ) -> Option<AxEvent> {
        self.raw(
            hit.pid,
            "ui.click",
            hit.window,
            Some(hit.element),
            EventData::UiClick(UiClickData {
                button: click.button,
                click_count: click.click_count,
            }),
            click.observed_at,
        )
    }

    fn raw(
        &self,
        pid: i32,
        event_type: &str,
        window: Option<NativeWindow>,
        element: Option<NativeElement>,
        data: EventData,
        observed_at: time::OffsetDateTime,
    ) -> Option<AxEvent> {
        let Some(app) = self.apps.get(&pid) else {
            crate::trace::trace!(
                "component=ax phase=builder action=drop pid={} event={} reason=missing_app",
                pid,
                event_type
            );
            return None;
        };
        let Some(window) = window else {
            crate::trace::trace!(
                "component=ax phase=builder action=drop pid={} event={} reason=missing_window",
                pid,
                event_type
            );
            return None;
        };
        let capture_context = self
            .capture_policy
            .decision(PrivacyScope::TextContent, &app.raw_app(), window.id)
            .capture_context();
        let event = RawEvent {
            observed_at: Some(observed_at),
            source: "macos.ax".to_owned(),
            event_type: event_type.to_owned(),
            app: zanei_core::schema::App {
                name: app.name.clone(),
                bundle_id: app.bundle_id.clone(),
                pid: Some(app.pid),
            },
            window: Some(Window {
                title: window.title,
                id: window.id,
            }),
            element: element.map(|element| Element {
                role: element.role,
                title: element.title,
                value: element.value,
            }),
            data,
            capture_context,
        };
        crate::trace::trace!(
            "component=ax phase=builder action=emit pid={} event={}",
            pid,
            event_type
        );
        Some(AxEvent::new(event))
    }
}
