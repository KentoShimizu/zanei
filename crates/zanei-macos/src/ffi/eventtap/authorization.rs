//! EventTap target snapshots and text-authorization reservation.

use std::{sync::atomic::Ordering, time::Instant};

use super::{
    CallbackContext, CfMutableRef, EVENT_TARGET_UNIX_PROCESS_ID_FIELD, NativeApp, NativeContext,
    NativeInputTarget,
};
use crate::{focus_context::FocusContext, text_capture::InputAuthorization, trace};

pub(super) fn input_target(
    event: CfMutableRef,
    focus_context: &FocusContext,
) -> Option<NativeInputTarget> {
    let pid = i32::try_from(unsafe {
        super::CGEventGetIntegerValueField(event, EVENT_TARGET_UNIX_PROCESS_ID_FIELD)
    })
    .ok()
    .filter(|pid| *pid > 0)?;
    let focus = focus_context
        .current()
        .filter(|focus| focus.app.pid == i64::from(pid))?;
    Some(NativeInputTarget {
        context: NativeContext {
            app: NativeApp {
                name: focus.app.name,
                bundle_id: focus.app.bundle_id,
                pid: focus.app.pid,
            },
            window: focus.window,
        },
        focused_field: focus.focused_field,
        focus_generation: focus.generation,
        field_generation: focus.field_generation,
    })
}

pub(super) fn prepare_input_authorization(
    context: &CallbackContext,
    target: Option<&NativeInputTarget>,
    input_at: Instant,
    secure_input: bool,
) -> Option<InputAuthorization> {
    let publisher = context.input_authorizations.as_ref()?;
    let valid_target = (!secure_input)
        .then_some(target)
        .flatten()
        .and_then(|target| {
            let pid = i32::try_from(target.context.app.pid).ok()?;
            target
                .focused_field
                .filter(|field| field.class.is_known_text())
                .map(|field| (pid, field.generation))
        });
    let result = match valid_target {
        Some((pid, generation)) => publisher.prepare(pid, generation, input_at).map(Some),
        None => {
            let reason = if secure_input {
                "secure_input"
            } else if target.is_none() {
                "target_missing"
            } else if target.and_then(|target| target.focused_field).is_none() {
                "focused_field_missing"
            } else {
                "field_not_text"
            };
            let rejected_pid = (!secure_input)
                .then(|| target.and_then(|target| i32::try_from(target.context.app.pid).ok()))
                .flatten();
            trace::trace!(
                "component=eventtap event=key_authorization pid={} target_generation={} authorization=rejected reason={}",
                target.map_or_else(
                    || "none".to_owned(),
                    |target| target.context.app.pid.to_string()
                ),
                target
                    .and_then(|target| target.focused_field)
                    .map_or_else(|| "none".to_owned(), |field| field.generation.to_string()),
                reason
            );
            publisher
                .reject_attempt(rejected_pid, input_at)
                .map(|()| None)
        }
    };
    match result {
        Ok(authorization) => authorization,
        Err(_) => {
            trace::trace!(
                "component=eventtap event=key_authorization pid={} target_generation={} authorization=rejected reason=authorization_queue_full",
                target.map_or_else(
                    || "none".to_owned(),
                    |target| target.context.app.pid.to_string()
                ),
                target
                    .and_then(|target| target.focused_field)
                    .map_or_else(|| "none".to_owned(), |field| field.generation.to_string())
            );
            context.degraded_operations.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

pub(super) fn trace_target(
    target: Option<&NativeInputTarget>,
    secure_input: bool,
    ime_active: bool,
) {
    trace::trace!(
        "component=eventtap event=key_callback pid={} field_class={} target_generation={} secure_input={} ime_active={}",
        target.map_or_else(
            || "none".to_owned(),
            |target| target.context.app.pid.to_string()
        ),
        target
            .and_then(|target| target.focused_field)
            .map_or("none", |field| trace::field_class_name(field.class)),
        target
            .and_then(|target| target.focused_field)
            .map_or_else(|| "none".to_owned(), |field| field.generation.to_string()),
        secure_input,
        ime_active
    );
}
