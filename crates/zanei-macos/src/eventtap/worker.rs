use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::SyncSender,
    },
    time::{Duration, Instant},
};

use zanei_collector::RawEvent;
use zanei_core::schema::EventData;

use super::{
    EventTapMode,
    clipboard::{ClipboardTracker, paste_data},
    logic::{
        MouseObservation, click_data, is_copy_shortcut, is_paste_shortcut, key_data, scroll_data,
    },
    output::{EmitResult, emit, raw_event, resolve_input_authorization, try_send_counted},
    state::{Driver, EventTapApi, MonotonicTime, TapState, WATCHDOG_INTERVAL},
    support::{
        click_button, disable_reason, elapsed, record_degraded_entries, refresh_secure_input,
        target_pid_matches_context,
    },
};
use crate::{
    ax::ClickObservation,
    ffi::eventtap::{
        self as native, EventTap, EventTapConfig, NativeContext, NativeEvent, Pasteboard,
        WakeObserver,
    },
    focused_field::FocusedFieldTracker,
    input_source::ImeState,
    secure_input::SecureInputProbe,
    text_capture::{InputAuthorizationPublisher, TextContentPolicy},
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
    focused_fields: Option<FocusedFieldTracker>,
    input_authorizations: Option<InputAuthorizationPublisher>,
    ime_state: ImeState,
    secure_input_probe: Option<SecureInputProbe>,
}

impl NativeApi {
    #[allow(clippy::too_many_arguments)]
    fn new(
        mode: EventTapMode,
        dropped_events: Arc<AtomicU64>,
        degraded_operations: Arc<AtomicU64>,
        focused_fields: Option<FocusedFieldTracker>,
        input_authorizations: Option<InputAuthorizationPublisher>,
        ime_state: ImeState,
        secure_input_probe: Option<SecureInputProbe>,
    ) -> Self {
        Self {
            tap: None,
            mode,
            dropped_events,
            degraded_operations,
            focused_fields,
            input_authorizations,
            ime_state,
            secure_input_probe,
        }
    }

    fn run_once(&self) {
        if let Some(tap) = &self.tap {
            tap.run_once(WORKER_POLL_INTERVAL);
        } else {
            native::run_loop_once(WORKER_POLL_INTERVAL);
        }
    }

    fn try_next_event(&self, text_policy: &TextContentPolicy) -> Option<NativeEvent> {
        self.tap.as_ref().and_then(|tap| {
            tap.try_next_event(|target| early_text_read_allowed(target, text_policy))
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
            focused_fields: self.focused_fields.clone(),
            input_authorizations: self.input_authorizations.clone(),
            ime_state: self.ime_state.clone(),
            secure_input_probe: self.secure_input_probe.clone(),
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
    focused_fields: &mut Option<FocusedFieldTracker>,
    stop_receiver: std::sync::mpsc::Receiver<()>,
    dropped_events: Arc<AtomicU64>,
    degraded_operations: Arc<AtomicU64>,
    current_degraded: Arc<AtomicBool>,
    secure_input_enabled: Arc<AtomicBool>,
    input_authorizations: Option<InputAuthorizationPublisher>,
    secure_input_probe: Option<SecureInputProbe>,
    ime_state: ImeState,
    text_policy: TextContentPolicy,
    ready_sender: SyncSender<()>,
) {
    let capture_text_content = mode.captures_text_content();
    let started_at = Instant::now();
    let mut driver = Driver::start(
        NativeApi::new(
            mode,
            Arc::clone(&dropped_events),
            Arc::clone(&degraded_operations),
            focused_fields.clone(),
            input_authorizations,
            ime_state.clone(),
            secure_input_probe,
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
    let mut last_watchdog = Instant::now();
    let _ = ready_sender.try_send(());

    while stop_receiver.try_recv().is_err() {
        let secure_input_before = refresh_secure_input(&mut driver, &secure_input_enabled);
        if let Some(clipboard) = clipboard.as_mut() {
            let change_before_events = pasteboard.change_count();
            if clipboard.has_changed(change_before_events) {
                let context_before_events = native::current_context();
                let copy = clipboard.copy_event(
                    change_before_events,
                    context_before_events.as_ref(),
                    Instant::now(),
                    |include_content| pasteboard.content(include_content),
                    secure_input_before,
                    &text_policy,
                );
                if !emit(&event_sender, copy, &dropped_events).continues() {
                    return;
                }
            }
        }
        driver.api_mut().run_once();
        let secure_input_now = refresh_secure_input(&mut driver, &secure_input_enabled);
        let now = elapsed(started_at);
        let mut events = Vec::new();
        while let Some(event) = driver.api_mut().try_next_event(&text_policy) {
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
        let context = needs_context.then(native::current_context).flatten();

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
                &text_policy,
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
                Instant::now(),
                |include_content| pasteboard.content(include_content),
                secure_input_now,
                &text_policy,
            );
            if !emit(&event_sender, copy, &dropped_events).continues() {
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
}

fn early_text_read_allowed(
    target: Option<crate::ffi::eventtap::NativeInputTarget>,
    text_policy: &TextContentPolicy,
) -> bool {
    let (Some(target), Some(context)) = (target, native::current_context()) else {
        return false;
    };
    if i64::from(target.pid) != context.app.pid {
        return false;
    }
    let app = zanei_core::schema::App {
        name: context.app.name,
        bundle_id: context.app.bundle_id,
        pid: Some(context.app.pid),
    };
    let window_id = context.window.and_then(|window| window.id);
    text_policy
        .input_decision(&app, window_id, target.focused_field)
        .is_allowed()
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
    text_policy: &TextContentPolicy,
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
        } if !secure_input_enabled && !secure_input_at_input => {
            let clipboard = clipboard
                .as_mut()
                .expect("input events require EventTap input capture");
            let Some(context) = context else {
                trace::trace!(
                    "component=eventtap event=key_worker pid=none authorization=rejected reason=context_missing"
                );
                resolve_input_authorization(EmitResult::Filtered, authorization.as_ref());
                return true;
            };
            if !target_pid_matches_context(target.map(|target| target.pid), context.app.pid) {
                trace::trace!(
                    "component=eventtap event=key_worker pid={} authorization=rejected reason=target_pid_mismatch",
                    context.app.pid
                );
                resolve_input_authorization(EmitResult::Filtered, authorization.as_ref());
                return true;
            }
            let focused_field = target.and_then(|target| target.focused_field);
            let field_kind = focused_field.and_then(|field| field.field_kind());
            let window_id = context.window.as_ref().and_then(|window| window.id);
            let app = zanei_core::schema::App {
                name: context.app.name.clone(),
                bundle_id: context.app.bundle_id.clone(),
                pid: Some(context.app.pid),
            };
            let window_text_allowed = text_policy.decision(&app, window_id).is_allowed();
            let input_text_allowed = capture_text_content
                && text_policy
                    .input_decision(&app, window_id, focused_field)
                    .is_allowed();
            let key_event = raw_event(
                "input.key",
                context,
                EventData::InputKey(key_data(
                    &observation,
                    input_text_allowed,
                    field_kind,
                    ime_active || ime_active_at_input,
                )),
                text_policy,
            );
            let key_result = emit(sender, key_event, dropped_events);
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
                    Instant::now(),
                    capture_text_content && window_text_allowed,
                );
            }
            if is_paste_shortcut(&observation) {
                return emit(
                    sender,
                    raw_event(
                        "clipboard.paste",
                        context,
                        EventData::ClipboardPaste(paste_data(
                            |include_content| pasteboard.content(include_content),
                            input_text_allowed,
                            field_kind,
                        )),
                        text_policy,
                    ),
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
                    text_policy,
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
            };
            let _ = try_send_counted(sender, observation, dropped_events);
            true
        }
    }
}
