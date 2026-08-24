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
    privacy::{CHROME_BUNDLE_ID, PrivacyScope},
    schema::{
        App, ClickButton, EmptyData, EventData, FieldKind, InputKeyData, InputKeyKind, Window,
    },
};

use super::{
    EventTapCollector, EventTapMode,
    clipboard::{ClipboardObservationTime, ClipboardTracker},
    logic::{
        KeyModifiers, KeyObservation, PasteboardContent, PasteboardKind, clipboard_paste, key_data,
    },
    output::{
        EmitResult, emit, emit_clipboard, emit_or_quarantine, raw_event,
        resolve_input_authorization, try_send_counted,
    },
    state::{Driver, EventTapApi, MonotonicTime},
    worker::{early_text_read_allowed, handle_native_event},
};
use crate::{
    CapturePolicy,
    ax::{ClickObservation, click_channel},
    chrome::{ChromeObserver, chrome_eligibility_channel},
    ffi::eventtap::{
        NativeApp, NativeContext, NativeEvent, NativeInputTarget, NativeWindow, Pasteboard,
    },
    focus_context::FocusContext,
    focused_field::{FieldClass, FocusedField},
    text_capture::{InputAuthorization, TextQuarantine, input_authorization_channel},
    workspace::{ApplicationActivationPolicy, ApplicationInfo},
};

fn capture_policy() -> CapturePolicy {
    let filter = FilterConfig::default();
    let (_, tracker) = chrome_eligibility_channel(filter.clone());
    CapturePolicy::new(tracker, filter, None)
}

fn raw() -> RawEvent {
    RawEvent {
        observed_at: None,
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

#[test]
fn tap_time_focus_generation_mismatch_denies_text() {
    let focus_context = FocusContext::new();
    focus_context.activate(
        ApplicationInfo {
            name: "Test".to_owned(),
            bundle_id: Some("dev.zanei.test".to_owned()),
            pid: 501,
            activation_policy: ApplicationActivationPolicy::Regular,
        },
        Some(window()),
    );
    let target = NativeInputTarget {
        context: context(Some(window())),
        focused_field: Some(FocusedField {
            generation: 1,
            class: FieldClass::KnownText(FieldKind::Text),
        }),
        focus_generation: 1,
        field_generation: 1,
    };
    assert!(early_text_read_allowed(
        Some(&target),
        &capture_policy(),
        &focus_context,
    ));

    focus_context.activate(
        ApplicationInfo {
            name: "Other".to_owned(),
            bundle_id: Some("dev.zanei.other".to_owned()),
            pid: 502,
            activation_policy: ApplicationActivationPolicy::Regular,
        },
        Some(NativeWindow {
            title: Some("Other".to_owned()),
            id: Some(12),
        }),
    );

    assert!(!early_text_read_allowed(
        Some(&target),
        &capture_policy(),
        &focus_context,
    ));
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
        observed_at: time::OffsetDateTime::UNIX_EPOCH,
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
    let focus_context = FocusContext::new();
    if let Some(context) = context {
        focus_context.activate(
            ApplicationInfo {
                name: context.app.name.clone(),
                bundle_id: context.app.bundle_id.clone(),
                pid: context.app.pid,
                activation_policy: ApplicationActivationPolicy::Regular,
            },
            context.window.clone(),
        );
    }
    let policy = capture_policy();
    let mut quarantine = crate::text_capture::TextQuarantine::new(ChromeObserver::new());
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
        &policy,
        &focus_context,
        &mut quarantine,
    )
}

#[test]
fn full_raw_event_channel_increments_drop_counter_through_worker() {
    let (sender, _receiver) = sync_channel(1);
    sender.try_send(raw()).expect("first event fits");
    let dropped = AtomicU64::new(0);
    let context = context(Some(window()));

    assert!(handle_event(
        key_event(
            Some(NativeInputTarget {
                context: context.clone(),
                focused_field: None,
                focus_generation: 1,
                field_generation: 1,
            }),
            None,
        ),
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
            observed_at: time::OffsetDateTime::UNIX_EPOCH,
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
        capture_policy(),
        ChromeObserver::new(),
        FocusContext::new(),
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
fn eventtap_chrome_body_without_version_is_suppressed() {
    let policy = capture_policy();
    let app = App {
        name: "Google Chrome".to_owned(),
        bundle_id: Some(CHROME_BUNDLE_ID.to_owned()),
        pid: Some(501),
    };
    let decision = policy.decision(PrivacyScope::TextContent, &app, Some(11));
    assert!(!decision.is_allowed());
    assert_eq!(decision.chrome_version(), None);
    let event = RawEvent {
        observed_at: Some(time::OffsetDateTime::UNIX_EPOCH),
        source: "macos.eventtap".to_owned(),
        event_type: "input.key".to_owned(),
        app,
        window: Some(Window {
            title: Some("Window".to_owned()),
            id: Some(11),
        }),
        element: None,
        data: EventData::InputKey(InputKeyData {
            kind: InputKeyKind::Text,
            modifiers: Vec::new(),
            combo: None,
            text: Some("private".to_owned()),
            field_kind: Some(FieldKind::Text),
            count: 1,
        }),
        capture_context: Default::default(),
    };
    let (sender, receiver) = sync_channel(1);
    let dropped = AtomicU64::new(0);
    let mut quarantine = TextQuarantine::new(ChromeObserver::new());

    assert_eq!(
        emit_or_quarantine(
            &sender,
            Some(event),
            &policy,
            Some(&decision),
            &mut quarantine,
            &dropped,
        ),
        EmitResult::Sent
    );

    let event = receiver.try_recv().expect("suppressed metadata event");
    let EventData::InputKey(data) = event.data else {
        panic!("input.key");
    };
    assert_eq!(data.text, None);
}

#[test]
fn v3_2_send_time_decision_overrides_stale_allow() {
    let context = context(Some(window()));
    let app = App {
        name: context.app.name.clone(),
        bundle_id: context.app.bundle_id.clone(),
        pid: Some(context.app.pid),
    };
    let policy = capture_policy();
    let earlier = policy.decision(PrivacyScope::TextContent, &app, Some(11));
    let event = raw_event(
        "input.key",
        &context,
        EventData::InputKey(InputKeyData {
            kind: InputKeyKind::Text,
            modifiers: Vec::new(),
            combo: None,
            text: Some("private key".to_owned()),
            field_kind: Some(FieldKind::Text),
            count: 1,
        }),
        &policy,
        time::OffsetDateTime::UNIX_EPOCH,
    )
    .expect("event built while allowed");
    assert!(earlier.is_allowed());
    deny_test_app_text(&policy);
    let (sender, receiver) = sync_channel(2);
    let dropped = AtomicU64::new(0);
    let mut quarantine = TextQuarantine::new(ChromeObserver::new());

    assert_eq!(
        emit_or_quarantine(
            &sender,
            Some(event),
            &policy,
            Some(&earlier),
            &mut quarantine,
            &dropped,
        ),
        EmitResult::Sent
    );
    let event = receiver.try_recv().expect("denied key metadata event");
    let EventData::InputKey(data) = event.data else {
        panic!("input.key");
    };
    assert_eq!(data.text, None);

    let clipboard_policy = capture_policy();
    let clipboard_decision = clipboard_policy.decision(PrivacyScope::TextContent, &app, Some(11));
    let mut clipboard = ClipboardTracker::new(1);
    let observed_at = ClipboardObservationTime {
        monotonic: Instant::now(),
        wall: time::OffsetDateTime::UNIX_EPOCH,
    };
    clipboard.observe_copy(&context, observed_at, true, clipboard_decision.clone());
    assert!(clipboard_decision.is_allowed());
    deny_test_app_text(&clipboard_policy);
    let output = clipboard.copy_event(
        2,
        Some(&context),
        observed_at,
        |include_content| PasteboardContent {
            kind: PasteboardKind::Text,
            size_bytes: include_content.then_some(7),
            text: include_content.then(|| "private".to_owned()),
        },
        false,
        &clipboard_policy,
    );
    let EventData::ClipboardCopy(data) = &output.as_ref().expect("copy output").event.data else {
        panic!("clipboard.copy");
    };
    assert_eq!(data.text.as_deref(), Some("private"));

    assert_eq!(
        emit_clipboard(
            &sender,
            output,
            &clipboard_policy,
            &mut quarantine,
            &dropped,
        ),
        EmitResult::Sent
    );
    let event = receiver.try_recv().expect("denied copy metadata event");
    let EventData::ClipboardCopy(data) = event.data else {
        panic!("clipboard.copy");
    };
    assert_eq!(data.text, None);
    assert_eq!(data.size_bytes, None);

    let deny_then_allow = capture_policy();
    deny_test_app_text(&deny_then_allow);
    let earlier_deny = deny_then_allow.decision(PrivacyScope::TextContent, &app, Some(11));
    deny_then_allow.replace_filter(FilterConfig::default());
    assert!(
        !deny_then_allow
            .decision_at_send(
                PrivacyScope::TextContent,
                &app,
                Some(11),
                Some(&earlier_deny),
            )
            .is_allowed()
    );
}

fn deny_test_app_text(policy: &CapturePolicy) {
    let mut filter = FilterConfig::default();
    filter
        .text_content
        .exclude_apps
        .push("dev.zanei.test".to_owned());
    policy.replace_filter(filter);
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
            observed_at: time::OffsetDateTime::UNIX_EPOCH,
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
            observed_at: time::OffsetDateTime::UNIX_EPOCH,
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
fn tap_time_target_wins_when_worker_focus_has_moved() {
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
                context: NativeContext {
                    app: NativeApp {
                        name: "Other".to_owned(),
                        bundle_id: Some("dev.zanei.other".to_owned()),
                        pid: 502,
                    },
                    window: Some(window()),
                },
                focused_field: None,
                focus_generation: 1,
                field_generation: 1,
            }),
            Some(authorization),
        ),
        &sender,
        None,
        Some(&context),
        &dropped,
    ));
    let event = receiver
        .try_recv()
        .expect("tap-time target should remain attributable");
    assert_eq!(event.app.pid, Some(502));
    assert_eq!(event.app.bundle_id.as_deref(), Some("dev.zanei.other"));
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    assert!(!authorizations.matching_for_test(502, 3, input_at));
}

#[test]
fn focus_context_feeds_key_and_paste_payloads_and_clears_on_app_transition() {
    let focus_context = FocusContext::new();
    focus_context.activate(
        ApplicationInfo {
            name: "Test".to_owned(),
            bundle_id: Some("dev.zanei.test".to_owned()),
            pid: 501,
            activation_policy: ApplicationActivationPolicy::Regular,
        },
        Some(window()),
    );
    focus_context.update_focused_field(
        501,
        Some(FocusedField {
            generation: 7,
            class: FieldClass::KnownText(FieldKind::Search),
        }),
    );
    let field_kind = focus_context
        .current()
        .and_then(|focus| focus.focused_field)
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
    focus_context.activate(
        ApplicationInfo {
            name: "Other".to_owned(),
            bundle_id: Some("dev.zanei.other".to_owned()),
            pid: 502,
            activation_policy: ApplicationActivationPolicy::Regular,
        },
        Some(window()),
    );
    assert_eq!(
        focus_context
            .current()
            .and_then(|focus| focus.focused_field),
        None
    );
}

#[test]
fn missing_ax_focus_observation_keeps_field_kind_null() {
    assert_eq!(
        FocusContext::new()
            .current()
            .and_then(|focus| focus.focused_field),
        None,
    );
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
    let focus_context = FocusContext::new();
    focus_context.activate(
        ApplicationInfo {
            name: "Test".to_owned(),
            bundle_id: Some("dev.zanei.test".to_owned()),
            pid: 501,
            activation_policy: ApplicationActivationPolicy::Regular,
        },
        Some(window()),
    );
    focus_context.update_focused_field(
        501,
        Some(FocusedField {
            generation: 3,
            class: FieldClass::KnownText(FieldKind::Text),
        }),
    );
    let callback_snapshot = focus_context
        .current()
        .and_then(|focus| focus.focused_field)
        .expect("callback should see the current focused field");

    focus_context.update_focused_field(
        501,
        Some(FocusedField {
            generation: 4,
            class: FieldClass::KnownText(FieldKind::Search),
        }),
    );

    assert_eq!(callback_snapshot.generation, 3);
    assert_eq!(
        focus_context
            .current()
            .and_then(|focus| focus.focused_field)
            .map(|field| field.generation),
        Some(4)
    );
}
