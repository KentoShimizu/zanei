use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    time::{Duration, Instant},
};

use zanei_collector::RawEvent;
use zanei_core::{
    config::FilterConfig,
    schema::{App, ClickButton, EmptyData, EventData, FieldKind},
};

use super::{
    EventTapCollector, EventTapMode,
    clipboard::ClipboardTracker,
    logic::{
        KeyModifiers, KeyObservation, PasteboardContent, PasteboardKind, clipboard_paste, key_data,
    },
    output::{EmitResult, emit, resolve_input_authorization, try_send_counted},
    state::{Driver, EventTapApi, MonotonicTime},
    support::target_pid_matches_context,
    worker::handle_native_event,
};
use crate::{
    ax::{ClickObservation, click_channel},
    chrome::chrome_eligibility_channel,
    ffi::eventtap::{
        NativeApp, NativeContext, NativeEvent, NativeInputTarget, NativeWindow, Pasteboard,
    },
    focused_field::{FieldClass, FocusedField, FocusedFieldTracker, focused_field_channel},
    text_capture::{InputAuthorization, TextContentPolicy, input_authorization_channel},
};

fn text_policy() -> TextContentPolicy {
    let (_, tracker) = chrome_eligibility_channel(FilterConfig::default());
    TextContentPolicy::new(tracker)
}

fn raw() -> RawEvent {
    RawEvent {
        source: "macos.eventtap".to_owned(),
        event_type: "app.launch".to_owned(),
        app: App {
            name: "Test".to_owned(),
            bundle_id: Some("dev.zanei.test".to_owned()),
            pid: Some(501),
        },
        window: None,
        element: None,
        data: EventData::AppLaunch(EmptyData::default()),
        capture_context: Default::default(),
    }
}

#[derive(Default)]
struct FakeEventTapApi {
    enabled: bool,
}

impl EventTapApi for FakeEventTapApi {
    fn enable(&mut self) {
        self.enabled = true;
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn recreate(&mut self) -> bool {
        self.enabled = true;
        true
    }

    fn secure_input_enabled(&self) -> bool {
        false
    }
}

fn context(window: Option<NativeWindow>) -> NativeContext {
    NativeContext {
        app: NativeApp {
            name: "Test".to_owned(),
            bundle_id: Some("dev.zanei.test".to_owned()),
            pid: 501,
        },
        window,
    }
}

fn window() -> NativeWindow {
    NativeWindow {
        title: Some("Window".to_owned()),
        id: Some(11),
    }
}

fn key_event(
    target: Option<NativeInputTarget>,
    authorization: Option<InputAuthorization>,
) -> NativeEvent {
    NativeEvent::Key {
        observation: KeyObservation {
            key_code: 0,
            modifiers: KeyModifiers::default(),
            text: None,
        },
        target,
        authorization,
        secure_input: false,
        ime_active: false,
    }
}

fn handle_event(
    event: NativeEvent,
    sender: &SyncSender<RawEvent>,
    click_sender: Option<&SyncSender<ClickObservation>>,
    context: Option<&NativeContext>,
    dropped: &AtomicU64,
) -> bool {
    let mut driver = Driver::start(
        FakeEventTapApi::default(),
        MonotonicTime::from_duration(Duration::ZERO),
    );
    let pasteboard = Pasteboard::new();
    let mut clipboard = Some(ClipboardTracker::new(0));
    handle_native_event(
        event,
        &mut driver,
        sender,
        click_sender,
        &pasteboard,
        context,
        false,
        dropped,
        MonotonicTime::from_duration(Duration::ZERO),
        false,
        false,
        &mut clipboard,
        &text_policy(),
    )
}

#[test]
fn full_raw_event_channel_increments_drop_counter_through_worker() {
    let (sender, _receiver) = sync_channel(1);
    sender.try_send(raw()).expect("first event fits");
    let dropped = AtomicU64::new(0);
    let context = context(Some(window()));

    assert!(handle_event(
        key_event(None, None),
        &sender,
        None,
        Some(&context),
        &dropped,
    ));
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
}

#[test]
fn disconnected_click_channel_increments_drop_counter() {
    let (sender, receiver) = sync_channel(1);
    drop(receiver);
    let dropped = AtomicU64::new(0);
    let connected = try_send_counted(
        &sender,
        ClickObservation {
            pid: 501,
            x: 1.0,
            y: 2.0,
            button: ClickButton::Left,
            click_count: 1,
        },
        &dropped,
    );
    assert_eq!(connected, EmitResult::Disconnected);
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
}

#[test]
fn disconnected_raw_event_channel_requests_worker_stop() {
    let (sender, receiver) = sync_channel(1);
    drop(receiver);
    let dropped = AtomicU64::new(0);
    assert_eq!(
        try_send_counted(&sender, raw(), &dropped),
        EmitResult::Disconnected
    );
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
}

#[test]
fn click_only_mode_skips_input_source_and_secure_input_state() {
    let (click_sender, _click_receiver) = click_channel();
    let mut collector = EventTapCollector::new(
        EventTapMode::ClickOnly,
        Some(click_sender),
        None,
        None,
        None,
        text_policy(),
    );
    collector
        .secure_input_enabled
        .store(true, Ordering::Relaxed);

    assert!(
        collector
            .prepare_main_thread()
            .expect("click-only preparation")
            .is_none()
    );
    assert!(!collector.secure_input_enabled());
}

#[test]
fn missing_window_is_filtered_without_incrementing_drop_counter() {
    let (sender, _receiver) = sync_channel(1);
    let dropped = AtomicU64::new(0);
    assert_eq!(emit(&sender, None, &dropped), EmitResult::Filtered);
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
}

#[test]
fn windowless_key_is_filtered_without_incrementing_drop_counter() {
    let (sender, receiver) = sync_channel(1);
    let dropped = AtomicU64::new(0);
    let context = context(None);

    assert!(handle_event(
        key_event(None, None),
        &sender,
        None,
        Some(&context),
        &dropped,
    ));
    assert!(receiver.try_recv().is_err());
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
}

#[test]
fn windowless_click_does_not_increment_drop_counter() {
    let (sender, _receiver) = sync_channel(1);
    let (click_sender, click_receiver) = click_channel();
    let dropped = AtomicU64::new(0);
    let context = context(None);

    assert!(handle_event(
        NativeEvent::MouseDown {
            x: 1.0,
            y: 2.0,
            button: 0,
            click_count: 1,
        },
        &sender,
        Some(&click_sender),
        Some(&context),
        &dropped,
    ));
    assert_eq!(
        click_receiver
            .try_recv()
            .expect("windowless click should reach AX hit testing")
            .pid,
        501
    );
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
}

#[test]
fn contextless_click_is_filtered_without_incrementing_drop_counter() {
    let (sender, _receiver) = sync_channel(1);
    let (click_sender, click_receiver) = click_channel();
    let dropped = AtomicU64::new(0);

    assert!(handle_event(
        NativeEvent::MouseDown {
            x: 1.0,
            y: 2.0,
            button: 0,
            click_count: 1,
        },
        &sender,
        Some(&click_sender),
        None,
        &dropped,
    ));
    assert!(click_receiver.try_recv().is_err());
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
}

#[test]
fn mismatched_target_pid_is_filtered_without_incrementing_drop_counter() {
    let (sender, receiver) = sync_channel(1);
    let dropped = AtomicU64::new(0);
    let context = context(Some(window()));
    let (publisher, mut authorizations) = input_authorization_channel();
    let input_at = Instant::now();
    let authorization = publisher
        .prepare(502, 3, input_at)
        .expect("authorization channel should accept the reservation");

    assert!(handle_event(
        key_event(
            Some(NativeInputTarget {
                pid: 502,
                focused_field: None,
            }),
            Some(authorization),
        ),
        &sender,
        None,
        Some(&context),
        &dropped,
    ));
    assert!(receiver.try_recv().is_err());
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    assert!(!authorizations.matching_for_test(502, 3, input_at));
}

#[test]
fn focused_field_tracker_feeds_key_and_paste_payloads_and_clears_by_pid() {
    let (publisher, tracker) = focused_field_channel();
    let focused_fields = Some(tracker);
    publisher.update(
        501,
        Some(FocusedField {
            generation: 7,
            class: FieldClass::KnownText(FieldKind::Search),
        }),
    );
    publisher.update(
        502,
        Some(FocusedField {
            generation: 9,
            class: FieldClass::KnownText(FieldKind::Text),
        }),
    );

    let field_kind = focused_fields
        .as_ref()
        .and_then(|tracker| tracker.focused_field(501))
        .and_then(FocusedField::field_kind);
    let key = key_data(
        &KeyObservation {
            key_code: 0,
            modifiers: KeyModifiers::default(),
            text: None,
        },
        false,
        field_kind,
        false,
    );
    let paste = clipboard_paste(
        PasteboardContent {
            kind: PasteboardKind::Text,
            size_bytes: Some(3),
            text: None,
        },
        field_kind,
    );

    assert_eq!(key.field_kind, Some(FieldKind::Search));
    assert_eq!(paste.field_kind, Some(FieldKind::Search));
    assert_eq!(
        focused_fields
            .as_ref()
            .and_then(|tracker| tracker.focused_field(502))
            .and_then(FocusedField::field_kind),
        Some(FieldKind::Text),
    );

    publisher.update(501, None);
    assert_eq!(
        focused_fields
            .as_ref()
            .and_then(|tracker| tracker.focused_field(501)),
        None
    );
    assert_eq!(
        focused_fields
            .as_ref()
            .and_then(|tracker| tracker.focused_field(502))
            .and_then(FocusedField::field_kind),
        Some(FieldKind::Text),
    );
}

#[test]
fn missing_ax_tracker_keeps_field_kind_null() {
    let focused_fields: Option<FocusedFieldTracker> = None;
    assert_eq!(
        focused_fields.and_then(|tracker| tracker.focused_field(501)),
        None
    );
}

#[test]
fn annotated_target_pid_must_match_the_worker_context() {
    assert!(target_pid_matches_context(Some(501), 501));
    assert!(target_pid_matches_context(None, 501));
    assert!(!target_pid_matches_context(Some(502), 501));
}

#[test]
fn dropped_raw_event_rejects_input_authorization() {
    let (sender, _receiver) = sync_channel(1);
    sender.try_send(raw()).expect("first event fits");
    let dropped = AtomicU64::new(0);
    let emit_result = try_send_counted(&sender, raw(), &dropped);
    let (publisher, mut authorizations) = input_authorization_channel();
    let input_at = Instant::now();
    let authorization = publisher
        .prepare(501, 3, input_at)
        .expect("authorization channel should accept the reservation");

    resolve_input_authorization(emit_result, Some(&authorization));

    assert!(!authorizations.matching_for_test(501, 3, input_at));
}

#[test]
fn authorization_matching_remains_pid_scoped() {
    let (publisher, mut authorizations) = input_authorization_channel();
    let input_at = Instant::now();
    let authorization = publisher
        .prepare(501, 3, input_at)
        .expect("authorization channel should accept the reservation");
    resolve_input_authorization(EmitResult::Sent, Some(&authorization));

    assert!(!authorizations.matching_for_test(502, 3, input_at));
    assert!(authorizations.matching_for_test(501, 3, input_at));
    assert!(authorizations.matching_for_test(501, 3, input_at));
}

#[test]
fn disconnected_raw_event_rejects_input_authorization() {
    let (publisher, mut authorizations) = input_authorization_channel();
    let input_at = Instant::now();
    let authorization = publisher
        .prepare(501, 3, input_at)
        .expect("authorization channel should accept the reservation");

    resolve_input_authorization(EmitResult::Disconnected, Some(&authorization));

    assert!(!authorizations.matching_for_test(501, 3, input_at));
}

#[test]
fn callback_snapshot_preserves_generation_after_focus_moves() {
    let (publisher, tracker) = focused_field_channel();
    publisher.update(
        501,
        Some(FocusedField {
            generation: 3,
            class: FieldClass::KnownText(FieldKind::Text),
        }),
    );
    let callback_snapshot = tracker
        .focused_field(501)
        .expect("callback should see the current focused field");

    publisher.update(
        501,
        Some(FocusedField {
            generation: 4,
            class: FieldClass::KnownText(FieldKind::Search),
        }),
    );

    assert_eq!(callback_snapshot.generation, 3);
    assert_eq!(
        tracker.focused_field(501).map(|field| field.generation),
        Some(4)
    );
}
