use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::SyncSender,
    },
    time::{Duration, Instant},
};

use zanei_collector::RawEvent;
use zanei_core::{privacy::PrivacyScope, schema::EventData};

use super::{
    EventTapMode,
    clipboard::{ClipboardObservationTime, ClipboardTracker, paste_data},
    logic::{
        MouseObservation, click_data, is_copy_shortcut, is_paste_shortcut, key_data, scroll_data,
    },
    output::{
        EmitResult, emit, emit_clipboard, emit_or_quarantine, emit_released, raw_event,
        resolve_input_authorization, try_send_counted,
    },
    state::{Driver, EventTapApi, MonotonicTime, TapState, WATCHDOG_INTERVAL},
    support::{
        click_button, disable_reason, elapsed, record_degraded_entries, refresh_secure_input,
    },
};
use crate::{
    ax::ClickObservation,
    capture_policy::CapturePolicy,
    chrome::ChromeObserver,
    ffi::eventtap::{
        self as native, EventTap, EventTapConfig, NativeApp, NativeContext, NativeEvent,
        Pasteboard, WakeObserver,
    },
    focus_context::{FocusContext, FocusSnapshot},
    input_source::ImeState,
    secure_input::SecureInputProbe,
    text_capture::{InputAuthorizationPublisher, TextQuarantine},
    trace,
};

const EVENT_QUEUE_CAPACITY: usize = 4_096;
const WAKE_QUEUE_CAPACITY: usize = 8;
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(100);

struct NativeApi {
    tap: Option<EventTap>,
    mode: EventTapMode,
    dropped_events: Arc<AtomicU64>,
    degraded_operations: Arc<AtomicU64>,
    input_authorizations: Option<InputAuthorizationPublisher>,
    ime_state: ImeState,
    secure_input_probe: Option<SecureInputProbe>,
    focus_context: FocusContext,
}

impl NativeApi {
    #[allow(clippy::too_many_arguments)]
    fn new(
        mode: EventTapMode,
        dropped_events: Arc<AtomicU64>,
        degraded_operations: Arc<AtomicU64>,
        input_authorizations: Option<InputAuthorizationPublisher>,
        ime_state: ImeState,
        secure_input_probe: Option<SecureInputProbe>,
        focus_context: FocusContext,
    ) -> Self {
        Self {
            tap: None,
            mode,
            dropped_events,
            degraded_operations,
            input_authorizations,
            ime_state,
            secure_input_probe,
            focus_context,
        }
    }

    fn run_once(&self) {
        if let Some(tap) = &self.tap {
            tap.run_once(WORKER_POLL_INTERVAL);
        } else {
            native::run_loop_once(WORKER_POLL_INTERVAL);
        }
    }

    fn try_next_event(&self, capture_policy: &CapturePolicy) -> Option<NativeEvent> {
        self.tap.as_ref().and_then(|tap| {
            tap.try_next_event(|target| {
                early_text_read_allowed(target, capture_policy, &self.focus_context)
            })
        })
    }
}

impl EventTapApi for NativeApi {
    fn enable(&mut self) {
        if let Some(tap) = &self.tap {
            tap.enable();
        }
    }

    fn is_enabled(&self) -> bool {
        self.tap.as_ref().is_some_and(EventTap::is_enabled)
    }

    fn recreate(&mut self) -> bool {
        self.tap = None;
        self.tap = EventTap::create(EventTapConfig {
            queue_capacity: EVENT_QUEUE_CAPACITY,
            mode: self.mode,
            dropped_events: Arc::clone(&self.dropped_events),
            degraded_operations: Arc::clone(&self.degraded_operations),
            input_authorizations: self.input_authorizations.clone(),
            ime_state: self.ime_state.clone(),
            secure_input_probe: self.secure_input_probe.clone(),
            focus_context: self.focus_context.clone(),
        })
        .ok();
        self.tap.is_some()
    }

    fn secure_input_enabled(&self) -> bool {
        self.mode.captures_text_content()
            && self
                .secure_input_probe
                .as_ref()
                .is_none_or(|probe| probe.enabled().unwrap_or(true))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    event_sender: SyncSender<RawEvent>,
    mode: EventTapMode,
    click_sender: Option<SyncSender<ClickObservation>>,
    stop_receiver: std::sync::mpsc::Receiver<()>,
    dropped_events: Arc<AtomicU64>,
    degraded_operations: Arc<AtomicU64>,
    current_degraded: Arc<AtomicBool>,
    secure_input_enabled: Arc<AtomicBool>,
    input_authorizations: Option<InputAuthorizationPublisher>,
    secure_input_probe: Option<SecureInputProbe>,
    ime_state: ImeState,
    capture_policy: CapturePolicy,
    chrome_observer: ChromeObserver,
    focus_context: FocusContext,
    ready_sender: SyncSender<()>,
) {
    let capture_text_content = mode.captures_text_content();
    let started_at = Instant::now();
    let mut driver = Driver::start(
        NativeApi::new(
            mode,
            Arc::clone(&dropped_events),
            Arc::clone(&degraded_operations),
            input_authorizations,
            ime_state.clone(),
            secure_input_probe,
            focus_context.clone(),
        ),
        elapsed(started_at),
    );
    driver.watchdog(elapsed(started_at));
    refresh_secure_input(&mut driver, &secure_input_enabled);
    let mut observed_degraded_entries = 0;
    record_degraded_entries(
        &driver,
        &mut observed_degraded_entries,
        &degraded_operations,
    );
    let wake_observer = WakeObserver::new(WAKE_QUEUE_CAPACITY).ok();
    let wake_recovery_degraded = wake_observer.is_none();
    if wake_recovery_degraded {
        degraded_operations.fetch_add(1, Ordering::Relaxed);
    }
    publish_current_health(&driver, wake_recovery_degraded, &current_degraded);
    let pasteboard = Pasteboard::new();
    let mut clipboard = mode
        .captures_input()
        .then(|| ClipboardTracker::new(pasteboard.change_count()));
    let mut quarantine = TextQuarantine::new(chrome_observer);
    let mut last_watchdog = Instant::now();
    let _ = ready_sender.try_send(());

    while stop_receiver.try_recv().is_err() {
        if !emit_released(
            &event_sender,
            quarantine.release(Instant::now(), &capture_policy),
            &dropped_events,
        ) {
            return;
        }
        let secure_input_before = refresh_secure_input(&mut driver, &secure_input_enabled);
        if let Some(clipboard) = clipboard.as_mut() {
            let change_before_events = pasteboard.change_count();
            if clipboard.has_changed(change_before_events) {
                let context_before_events = focus_context.current().map(native_context);
                let copy = clipboard.copy_event(
                    change_before_events,
                    context_before_events.as_ref(),
                    ClipboardObservationTime {
                        monotonic: Instant::now(),
                        wall: time::OffsetDateTime::now_utc(),
                    },
                    |include_content| pasteboard.content(include_content),
                    secure_input_before,
                    &capture_policy,
                );
                if !emit_clipboard(&event_sender, copy, &mut quarantine, &dropped_events)
                    .continues()
                {
                    return;
                }
            }
        }
        driver.api_mut().run_once();
        let secure_input_now = refresh_secure_input(&mut driver, &secure_input_enabled);
        let now = elapsed(started_at);
        let mut events = Vec::new();
        while let Some(event) = driver.api_mut().try_next_event(&capture_policy) {
            events.push(event);
        }
        let pasteboard_change_count = clipboard.as_ref().map(|_| pasteboard.change_count());
        let clipboard_changed = clipboard
            .as_ref()
            .zip(pasteboard_change_count)
            .is_some_and(|(clipboard, count)| clipboard.has_changed(count));
        let needs_context = clipboard_changed
            || events.iter().any(|event| {
                matches!(
                    event,
                    NativeEvent::Key { .. }
                        | NativeEvent::Scroll { .. }
                        | NativeEvent::MouseDown { .. }
                )
            });
        let context = needs_context
            .then(|| focus_context.current().map(native_context))
            .flatten();

        for event in events {
            if !handle_native_event(
                event,
                &mut driver,
                &event_sender,
                click_sender.as_ref(),
                &pasteboard,
                context.as_ref(),
                capture_text_content,
                &dropped_events,
                now,
                ime_state.active(),
                secure_input_now,
                &mut clipboard,
                &capture_policy,
                &focus_context,
                &mut quarantine,
            ) {
                return;
            }
        }

        if wake_observer.as_ref().is_some_and(WakeObserver::take_wake) {
            driver.wake(now);
        }
        if let (Some(clipboard), Some(change_count)) = (clipboard.as_mut(), pasteboard_change_count)
            && clipboard_changed
        {
            let copy = clipboard.copy_event(
                change_count,
                context.as_ref(),
                ClipboardObservationTime {
                    monotonic: Instant::now(),
                    wall: time::OffsetDateTime::now_utc(),
                },
                |include_content| pasteboard.content(include_content),
                secure_input_now,
                &capture_policy,
            );
            if !emit_clipboard(&event_sender, copy, &mut quarantine, &dropped_events).continues() {
                return;
            }
        }
        if last_watchdog.elapsed() >= WATCHDOG_INTERVAL {
            driver.watchdog(now);
            last_watchdog = Instant::now();
        }
        if matches!(driver.state(), TapState::Degraded { .. }) {
            driver.retry(now);
        }
        record_degraded_entries(
            &driver,
            &mut observed_degraded_entries,
            &degraded_operations,
        );
        publish_current_health(&driver, wake_recovery_degraded, &current_degraded);
    }
    let _ = emit_released(&event_sender, quarantine.flush(), &dropped_events);
}

pub(super) fn early_text_read_allowed(
    target: Option<&crate::ffi::eventtap::NativeInputTarget>,
    capture_policy: &CapturePolicy,
    focus_context: &FocusContext,
) -> bool {
    let Some(target) = target else {
        return false;
    };
    if target.focus_generation != focus_context.generation()
        || target.field_generation != focus_context.field_generation()
    {
        return false;
    }
    let window_id = target.context.window.as_ref().and_then(|window| window.id);
    let app = zanei_core::schema::App {
        name: target.context.app.name.clone(),
        bundle_id: target.context.app.bundle_id.clone(),
        pid: Some(target.context.app.pid),
    };
    capture_policy
        .input_decision(&app, window_id, target.focused_field)
        .is_allowed()
}

fn native_context(focus: FocusSnapshot) -> NativeContext {
    NativeContext {
        app: NativeApp {
            name: focus.app.name,
            bundle_id: focus.app.bundle_id,
            pid: focus.app.pid,
        },
        window: focus.window,
    }
}

fn publish_current_health<A: EventTapApi>(
    driver: &Driver<A>,
    wake_recovery_degraded: bool,
    current_degraded: &AtomicBool,
) {
    current_degraded.store(
        wake_recovery_degraded || driver.is_degraded(),
        Ordering::Relaxed,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_native_event<A: EventTapApi>(
    event: NativeEvent,
    driver: &mut Driver<A>,
    sender: &SyncSender<RawEvent>,
    click_sender: Option<&SyncSender<ClickObservation>>,
    pasteboard: &Pasteboard,
    context: Option<&NativeContext>,
    capture_text_content: bool,
    dropped_events: &AtomicU64,
    now: MonotonicTime,
    ime_active: bool,
    secure_input_enabled: bool,
    clipboard: &mut Option<ClipboardTracker>,
    capture_policy: &CapturePolicy,
    focus_context: &FocusContext,
    quarantine: &mut TextQuarantine,
) -> bool {
    match event {
        NativeEvent::Disabled(reason) => {
            driver.disabled(disable_reason(reason), now);
            true
        }
        NativeEvent::Key {
            observation,
            target,
            authorization,
            secure_input: secure_input_at_input,
            ime_active: ime_active_at_input,
            observed_at,
        } if !secure_input_enabled && !secure_input_at_input => {
            let clipboard = clipboard
                .as_mut()
                .expect("input events require EventTap input capture");
            let Some(target) = target.as_ref() else {
                trace::trace!(
                    "component=eventtap event=key_worker pid=none authorization=rejected reason=context_missing"
                );
                resolve_input_authorization(EmitResult::Filtered, authorization.as_ref());
                return true;
            };
            let context = &target.context;
            let focused_field = target.focused_field;
            let field_kind = focused_field.and_then(|field| field.field_kind());
            let window_id = context.window.as_ref().and_then(|window| window.id);
            let app = zanei_core::schema::App {
                name: context.app.name.clone(),
                bundle_id: context.app.bundle_id.clone(),
                pid: Some(context.app.pid),
            };
            let generation_matches = target.focus_generation == focus_context.generation()
                && target.field_generation == focus_context.field_generation();
            let window_decision =
                capture_policy.decision(PrivacyScope::TextContent, &app, window_id);
            let window_text_allowed = generation_matches && window_decision.is_allowed();
            let input_decision = capture_policy.input_decision(&app, window_id, focused_field);
            let input_text_allowed =
                capture_text_content && generation_matches && input_decision.is_allowed();
            let key_event = raw_event(
                "input.key",
                context,
                EventData::InputKey(key_data(
                    &observation,
                    input_text_allowed,
                    field_kind,
                    ime_active || ime_active_at_input,
                )),
                capture_policy,
                observed_at,
            );
            let key_result = emit_or_quarantine(
                sender,
                key_event,
                Some(&input_decision),
                quarantine,
                dropped_events,
            );
            trace::trace!(
                "component=eventtap event=key_worker pid={} field_class={} authorization={} reason={}",
                context.app.pid,
                focused_field.map_or("none", |field| trace::field_class_name(field.class)),
                if input_text_allowed && key_result == EmitResult::Sent {
                    "confirmed"
                } else {
                    "rejected"
                },
                if input_text_allowed {
                    key_result.name()
                } else {
                    "text_policy_denied"
                }
            );
            resolve_input_authorization(
                if input_text_allowed {
                    key_result
                } else {
                    EmitResult::Filtered
                },
                authorization.as_ref(),
            );
            if key_result == EmitResult::Disconnected {
                return false;
            }
            if is_copy_shortcut(&observation) {
                clipboard.observe_copy(
                    context,
                    ClipboardObservationTime {
                        monotonic: Instant::now(),
                        wall: observed_at,
                    },
                    capture_text_content && window_text_allowed,
                    window_decision,
                );
            }
            if is_paste_shortcut(&observation) {
                return emit_or_quarantine(
                    sender,
                    raw_event(
                        "clipboard.paste",
                        context,
                        EventData::ClipboardPaste(paste_data(
                            |include_content| pasteboard.content(include_content),
                            input_text_allowed,
                            field_kind,
                        )),
                        capture_policy,
                        observed_at,
                    ),
                    Some(&input_decision),
                    quarantine,
                    dropped_events,
                )
                .continues();
            }
            true
        }
        NativeEvent::Key { authorization, .. } => {
            trace::trace!(
                "component=eventtap event=key_worker authorization=rejected reason=secure_input"
            );
            resolve_input_authorization(EmitResult::Filtered, authorization.as_ref());
            true
        }
        NativeEvent::Scroll {
            vertical,
            horizontal,
            observed_at,
        } => {
            let (Some(context), Some(data)) = (context, scroll_data(vertical, horizontal)) else {
                return true;
            };
            emit(
                sender,
                raw_event(
                    "input.scroll",
                    context,
                    EventData::InputScroll(data),
                    capture_policy,
                    observed_at,
                ),
                dropped_events,
            )
            .continues()
        }
        NativeEvent::MouseDown {
            x,
            y,
            button,
            click_count,
            observed_at,
        } => {
            let (Some(sender), Some(context)) = (click_sender, context) else {
                return true;
            };
            let Some(click) = click_data(
                context.app.pid,
                MouseObservation {
                    x,
                    y,
                    button,
                    click_count,
                },
            ) else {
                return true;
            };
            let observation = ClickObservation {
                pid: click.pid,
                x: click.x,
                y: click.y,
                button: click_button(click.button),
                click_count: click.click_count,
                observed_at,
            };
            let _ = try_send_counted(sender, observation, dropped_events);
            true
        }
    }
}
