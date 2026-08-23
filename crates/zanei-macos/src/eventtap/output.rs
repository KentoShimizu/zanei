//! EventTap output construction and bounded-channel delivery.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{SyncSender, TrySendError},
};

use zanei_collector::RawEvent;
use zanei_core::schema::{App, EventData, Window};

use crate::trace;
use crate::{
    ffi::eventtap::NativeContext,
    text_capture::{InputAuthorization, TextContentPolicy},
};

const SOURCE: &str = "macos.eventtap";

pub(super) fn raw_event(
    event_type: &str,
    context: &NativeContext,
    data: EventData,
    text_policy: &TextContentPolicy,
) -> Option<RawEvent> {
    let Some(window) = context.window.as_ref() else {
        trace::trace!(
            "component=eventtap event=output pid={} event_type={} result=filtered reason=missing_window",
            context.app.pid,
            event_type
        );
        return None;
    };
    let capture_context =
        text_policy.capture_context(context.app.bundle_id.as_deref(), context.app.pid, window.id);
    Some(RawEvent {
        source: SOURCE.to_owned(),
        event_type: event_type.to_owned(),
        app: App {
            name: context.app.name.clone(),
            bundle_id: context.app.bundle_id.clone(),
            pid: Some(context.app.pid),
        },
        window: Some(Window {
            title: window.title.clone(),
            id: window.id,
        }),
        element: None,
        data,
        capture_context,
    })
}

pub(super) fn unknown_clipboard_event(data: EventData) -> RawEvent {
    RawEvent {
        source: SOURCE.to_owned(),
        event_type: "clipboard.copy".to_owned(),
        app: App {
            name: "Unknown".to_owned(),
            bundle_id: None,
            pid: None,
        },
        window: None,
        element: None,
        data,
        capture_context: Default::default(),
    }
}

pub(super) fn emit(
    sender: &SyncSender<RawEvent>,
    event: Option<RawEvent>,
    dropped_events: &AtomicU64,
) -> EmitResult {
    let Some(event) = event else {
        return EmitResult::Filtered;
    };
    try_send_counted(sender, event, dropped_events)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EmitResult {
    Sent,
    Filtered,
    Dropped,
    Disconnected,
}

impl EmitResult {
    pub(super) const fn continues(self) -> bool {
        !matches!(self, Self::Disconnected)
    }

    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Filtered => "filtered",
            Self::Dropped => "dropped",
            Self::Disconnected => "disconnected",
        }
    }
}

pub(super) fn try_send_counted<T>(
    sender: &SyncSender<T>,
    value: T,
    dropped_events: &AtomicU64,
) -> EmitResult {
    match sender.try_send(value) {
        Ok(()) => EmitResult::Sent,
        Err(TrySendError::Full(_)) => {
            dropped_events.fetch_add(1, Ordering::Relaxed);
            EmitResult::Dropped
        }
        Err(TrySendError::Disconnected(_)) => {
            dropped_events.fetch_add(1, Ordering::Relaxed);
            EmitResult::Disconnected
        }
    }
}

pub(super) fn resolve_input_authorization(
    emit_result: EmitResult,
    authorization: Option<&InputAuthorization>,
) {
    let Some(authorization) = authorization else {
        return;
    };
    if emit_result == EmitResult::Sent {
        authorization.confirm();
    } else {
        authorization.reject();
    }
    trace::trace!(
        "component=eventtap event=key_authorization authorization={} reason=output_result",
        emit_result.name()
    );
}
