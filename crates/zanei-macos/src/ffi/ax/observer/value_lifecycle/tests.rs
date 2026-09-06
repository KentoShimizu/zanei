use super::*;
use crate::ffi::ax::{
    element::{capture_value_snapshot_with, value_snapshot_with},
    tests::{fake_field_snapshot, ide_app, ide_policy, titled_window},
};
use crate::text_capture::{ValueCapture, input_authorization_channel};
use std::{cell::Cell, time::Duration};
use zanei_core::schema::FieldKind;

#[test]
fn surface_race_preserves_registration_and_next_same_focus_input_recovers() {
    let mut observer = AppObserver::fake_with_unknown_focused_target();
    observer.capture_text_content = true;
    observer.capture_policy = ide_policy("block");
    observer.app = ide_app();
    let element = observer.fake_focused_element();
    let (publisher, mut authorizations) = input_authorization_channel();
    let class = FieldClass::KnownText(FieldKind::Text);
    let context = &mut observer.focused_target.current_mut().unwrap().context;
    context.window = titled_window(Some("main.rs"));
    context.field_class = class;
    context.capture = ValueCapture::new(true, Some("old".to_owned()), class);
    let generation = context.generation;
    assert!(
        observer
            .reconcile_current_value_notification_with(class, || Ok(()), || Ok(()))
            .is_ok()
    );
    let now = Instant::now();
    for (step, body) in ["old secret", "fresh", "fresh!"].into_iter().enumerate() {
        let at = now + Duration::from_secs(step as u64);
        if step != 1 {
            publisher.prepare(7, generation, at).unwrap().confirm();
        }
        let read = Cell::new(false);
        let mut events = observer
            .value_changed_events_with(
                at,
                OffsetDateTime::UNIX_EPOCH,
                false,
                &mut authorizations,
                |_, window, policy, app, enabled, _, changed| {
                    capture_value_snapshot_with(
                        window,
                        policy,
                        app,
                        enabled,
                        changed,
                        || {
                            Ok(titled_window(Some(if step == 0 && !read.get() {
                                "main.rs"
                            } else {
                                "next.rs"
                            })))
                        },
                        |allowed| {
                            value_snapshot_with(
                                fake_field_snapshot(Some("AXTextArea")),
                                allowed,
                                || {
                                    read.set(true);
                                    Ok(Some(body.to_owned()))
                                },
                                || Ok(Some(body.len() as i64)),
                            )
                        },
                    )
                },
                |observer, class| {
                    observer
                        .reconcile_current_value_notification_with(class, || Ok(()), || Ok(()))
                        .is_ok()
                },
            )
            .unwrap();
        assert!(
            read.get(),
            "registered same-focus delivery must reach the value boundary"
        );
        assert!(observer.is_current_target(crate::ffi::ax::TargetKind::Value, element));
        events.extend(observer.flush_pending(false, &mut authorizations));
        let text: Vec<_> = events
            .into_iter()
            .filter_map(|event| match event {
                NativeAxEvent::UiValueChanged(event) => event.text,
                _ => None,
            })
            .collect();
        assert_eq!(
            text,
            if step == 2 {
                vec!["!".to_owned()]
            } else {
                vec![]
            }
        );
    }
    // Disabled capture still reclassifies a genuinely unknown field and unregisters it.
    observer.capture_text_content = false;
    observer
        .value_changed_events_with(
            now + Duration::from_secs(3),
            OffsetDateTime::UNIX_EPOCH,
            false,
            &mut authorizations,
            |_, window, policy, app, enabled, _, changed| {
                capture_value_snapshot_with(
                    window,
                    policy,
                    app,
                    enabled,
                    changed,
                    || panic!("disabled capture does not read window"),
                    |allowed| {
                        value_snapshot_with(
                            fake_field_snapshot(None),
                            allowed,
                            || panic!("unknown field does not read body"),
                            || panic!("unknown count"),
                        )
                    },
                )
            },
            |observer, class| {
                observer
                    .reconcile_current_value_notification_with(class, || Ok(()), || Ok(()))
                    .is_ok()
            },
        )
        .unwrap();
    assert!(!observer.is_current_target(crate::ffi::ax::TargetKind::Value, element));
}
