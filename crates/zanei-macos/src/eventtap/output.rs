//! EventTap output construction and bounded-channel delivery.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{SyncSender, TrySendError},
};

use time::OffsetDateTime;
use zanei_collector::RawEvent;
use zanei_core::{
    privacy::PrivacyScope,
    schema::{App, EventData, Window},
};

use crate::trace;
use crate::{
    capture_policy::{CaptureDecision, CapturePolicy},
    ffi::eventtap::NativeContext,
    text_capture::{
        InputAuthorization, ReleasedEvent, TextBodyRoute, TextQuarantine, route_text_body,
    },
};

use super::clipboard::ClipboardOutput;

const SOURCE: &str = "macos.eventtap";

pub(super) fn raw_event(
    event_type: &str,
    context: &NativeContext,
    data: EventData,
    capture_policy: &CapturePolicy,
    observed_at: OffsetDateTime,
) -> Option<RawEvent> {
    let Some(window) = context.window.as_ref() else {
        trace::trace!(
            "component=eventtap event=output pid={} event_type={} result=filtered reason=missing_window",
            context.app.pid,
            event_type
        );
        return None;
    };
    let app = App {
        name: context.app.name.clone(),
        bundle_id: context.app.bundle_id.clone(),
        pid: Some(context.app.pid),
    };
    let capture_context = capture_policy
        .decision(PrivacyScope::TextContent, &app, window.id)
        .capture_context();
    Some(RawEvent {
        observed_at: Some(observed_at),
        source: SOURCE.to_owned(),
        event_type: event_type.to_owned(),
        app,
        window: Some(Window {
            title: window.title.clone(),
            id: window.id,
        }),
        element: None,
        data,
        capture_context,
    })
}

pub(super) fn unknown_clipboard_event(data: EventData, observed_at: OffsetDateTime) -> RawEvent {
    RawEvent {
        observed_at: Some(observed_at),
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

pub(super) fn emit_clipboard(
    sender: &SyncSender<RawEvent>,
    output: Option<ClipboardOutput>,
    quarantine: &mut TextQuarantine,
    dropped_events: &AtomicU64,
) -> EmitResult {
    let Some(output) = output else {
        return EmitResult::Filtered;
    };
    emit_or_quarantine(
        sender,
        Some(output.event),
        output.decision.as_ref(),
        quarantine,
        dropped_events,
    )
}

pub(super) fn emit_or_quarantine(
    sender: &SyncSender<RawEvent>,
    event: Option<RawEvent>,
    decision: Option<&CaptureDecision>,
    quarantine: &mut TextQuarantine,
    dropped_events: &AtomicU64,
) -> EmitResult {
    let Some(event) = event else {
        return EmitResult::Filtered;
    };
    match route_text_body(event, decision) {
        TextBodyRoute::Send(event) => try_send_counted(sender, event, dropped_events),
        TextBodyRoute::Quarantine {
            event,
            key,
            version,
            observed_at,
        } => {
            quarantine.hold_text(event, key, version, observed_at);
            EmitResult::Sent
        }
    }
}

pub(super) fn emit_released(
    sender: &SyncSender<RawEvent>,
    events: Vec<ReleasedEvent>,
    dropped_events: &AtomicU64,
) -> bool {
    events.into_iter().all(|event| {
        let (event, _) = event.into_parts();
        try_send_counted(sender, event, dropped_events).continues()
    })
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
