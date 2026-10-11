//! EventTap target snapshots and text-authorization reservation.

use std::{sync::atomic::Ordering, time::Instant};

use super::{
    CallbackContext, CfMutableRef, EVENT_TARGET_UNIX_PROCESS_ID_FIELD, NativeApp, NativeContext,
    NativeInputTarget,
};
use crate::{
    eventtap::logic::is_submit_key,
    focus_context::FocusContext,
    text_capture::{InputActivity, InputActivityKind, InputAuthorization},
    trace,
};

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

/// Activity is reported only for the application that owns the current focus, and never while
/// Secure Input is active.
fn input_activity(
    target: Option<&NativeInputTarget>,
    secure_input: bool,
    key_code: u16,
    at: Instant,
) -> Option<InputActivity> {
    let pid = i32::try_from(target?.context.app.pid).ok()?;
    (!secure_input).then_some(InputActivity {
        pid,
        kind: if is_submit_key(key_code) {
            InputActivityKind::Submit
        } else {
            InputActivityKind::Edit
        },
        at,
    })
}

pub(super) fn prepare_input_authorization(
    context: &CallbackContext,
    target: Option<&NativeInputTarget>,
    key_code: u16,
    input_at: Instant,
    secure_input: bool,
) -> Option<InputAuthorization> {
    let publisher = context.input_authorizations.as_ref()?;
    if let Some(activity) = input_activity(target, secure_input, key_code, input_at)
        && publisher.report_activity(activity).is_err()
    {
        trace::trace!(
            "component=eventtap event=input_activity pid={} result=dropped reason=authorization_queue_full",
            activity.pid
        );
        context.degraded_operations.fetch_add(1, Ordering::Relaxed);
    }
    if !context.capture_text_content {
        return None;
    }
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

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, atomic::AtomicU64, mpsc::sync_channel},
        time::Instant,
    };

    use zanei_core::schema::FieldKind;

    use super::{super::CallbackContext, NativeApp, NativeContext, NativeInputTarget};
    use crate::{
        focus_context::FocusContext,
        focused_field::{FieldClass, FocusedField},
        input_source::ImeState,
        text_capture::{InputActivityKind, input_authorization_channel},
    };

    const KNOWN_TEXT: Option<FocusedField> = Some(FocusedField {
        generation: 3,
        class: FieldClass::KnownText(FieldKind::Text),
    });

    /// Runs one key-down through the callback helper and returns the reported activity kinds and
    /// whether a text authorization was reserved.
    fn key_down(
        capture_text_content: bool,
        target: Option<Option<FocusedField>>,
        key_code: u16,
        secure_input: bool,
    ) -> (Vec<InputActivityKind>, bool) {
        let (publisher, mut authorizations) = input_authorization_channel();
        let context = CallbackContext {
            sender: sync_channel(1).0,
            dropped_events: Arc::new(AtomicU64::new(0)),
            degraded_operations: Arc::new(AtomicU64::new(0)),
            input_authorizations: Some(publisher),
            ime_state: ImeState::new(),
            secure_input_probe: None,
            focus_context: FocusContext::new(),
            capture_text_content,
        };
        let target = target.map(|focused_field| NativeInputTarget {
            context: NativeContext {
                app: NativeApp {
                    name: "Test".to_owned(),
                    bundle_id: None,
                    pid: 501,
                },
                window: None,
            },
            focused_field,
            focus_generation: 1,
            field_generation: 1,
        });
        let at = Instant::now();
        let reserved = super::prepare_input_authorization(
            &context,
            target.as_ref(),
            key_code,
            at,
            secure_input,
        )
        .is_some();
        let kinds = authorizations.take_activity();
        assert!(kinds.iter().all(|activity| activity.pid == 501));
        (
            kinds.into_iter().map(|activity| activity.kind).collect(),
            reserved,
        )
    }

    // Bug: metadata mode reserves authorizations; keys for a field that is not yet known never
    // reach AX; Return or keypad Enter is not reported as a submit.
    #[test]
    fn key_downs_for_the_focused_app_report_activity_in_both_modes() {
        use InputActivityKind::{Edit, Submit};
        assert_eq!(
            key_down(false, Some(KNOWN_TEXT), 0x00, false),
            (vec![Edit], false)
        );
        assert_eq!(
            key_down(false, Some(None), 0x24, false),
            (vec![Submit], false)
        );
        assert_eq!(
            key_down(false, Some(None), 0x4c, false),
            (vec![Submit], false)
        );
        assert_eq!(key_down(true, Some(None), 0x00, false), (vec![Edit], false));
        assert_eq!(
            key_down(true, Some(KNOWN_TEXT), 0x00, false),
            (vec![Edit], true)
        );
    }

    // Bug: a pull is armed for an application that does not own the focus, or during Secure Input.
    #[test]
    fn unmatched_targets_and_secure_input_report_no_activity() {
        for capture_text_content in [false, true] {
            assert!(
                key_down(capture_text_content, None, 0x00, false)
                    .0
                    .is_empty()
            );
            assert!(
                key_down(capture_text_content, Some(KNOWN_TEXT), 0x00, true)
                    .0
                    .is_empty()
            );
        }
    }
}
