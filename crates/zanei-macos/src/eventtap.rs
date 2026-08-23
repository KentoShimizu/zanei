//! Input, scrolling, clipboard, and click-trigger collection through CGEventTap.
mod clipboard;
pub(crate) mod logic;
mod mode;
mod output;
mod state;
mod support;
mod worker;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
};

use zanei_collector::{Collector, CollectorError, Permission, RawEvent};

pub use mode::EventTapMode;

pub use crate::input_source::InputSourceObserver;

use crate::{
    ax::ClickObservation,
    focus_context::FocusContext,
    focused_field::FocusedFieldTracker,
    input_source::ImeState,
    secure_input::SecureInputProbe,
    text_capture::{InputAuthorizationPublisher, TextContentPolicy},
};

static REQUIRED_PERMISSIONS: [Permission; 1] = [Permission::InputMonitoring];

pub struct EventTapCollector {
    mode: EventTapMode,
    click_sender: Option<SyncSender<ClickObservation>>,
    focused_fields: Option<FocusedFieldTracker>,
    input_authorizations: Option<InputAuthorizationPublisher>,
    secure_input_probe: Option<SecureInputProbe>,
    text_policy: TextContentPolicy,
    focus_context: FocusContext,
    ime_state: ImeState,
    input_source_prepared: bool,
    stop_sender: Option<SyncSender<()>>,
    worker: Option<Worker>,
    dropped_events: Arc<AtomicU64>,
    degraded_operations: Arc<AtomicU64>,
    current_degraded: Arc<AtomicBool>,
    secure_input_enabled: Arc<AtomicBool>,
}

impl EventTapCollector {
    #[must_use]
    pub fn new(
        mode: EventTapMode,
        click_sender: Option<SyncSender<ClickObservation>>,
        focused_fields: Option<FocusedFieldTracker>,
        input_authorizations: Option<InputAuthorizationPublisher>,
        secure_input_probe: Option<SecureInputProbe>,
        text_policy: TextContentPolicy,
        focus_context: FocusContext,
    ) -> Self {
        assert_eq!(
            mode.captures_clicks(),
            click_sender.is_some(),
            "EventTap click capture requires exactly one click sender"
        );
        Self {
            mode,
            click_sender,
            focused_fields,
            input_authorizations,
            secure_input_probe,
            text_policy,
            focus_context,
            ime_state: ImeState::new(),
            input_source_prepared: !mode.captures_text_content(),
            stop_sender: None,
            worker: None,
            dropped_events: Arc::new(AtomicU64::new(0)),
            degraded_operations: Arc::new(AtomicU64::new(0)),
            current_degraded: Arc::new(AtomicBool::new(false)),
            secure_input_enabled: Arc::new(AtomicBool::new(false)),
        }
    }

    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn degraded_operations(&self) -> u64 {
        self.degraded_operations.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn is_degraded(&self) -> bool {
        self.current_degraded.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn secure_input_enabled(&self) -> bool {
        self.mode.captures_input() && self.secure_input_enabled.load(Ordering::Relaxed)
    }

    pub fn prepare_main_thread(&mut self) -> Result<Option<InputSourceObserver>, CollectorError> {
        if !self.mode.captures_text_content() {
            return Ok(None);
        }
        if self.input_source_prepared || self.worker.is_some() {
            return Err(CollectorError::AlreadyRunning {
                collector: self.name().to_owned(),
            });
        }
        let observer =
            InputSourceObserver::new(&self.ime_state).ok_or_else(|| CollectorError::Start {
                collector: self.name().to_owned(),
                message: "failed to monitor the current keyboard input source".to_owned(),
            })?;
        self.input_source_prepared = true;
        Ok(Some(observer))
    }
}

impl Collector for EventTapCollector {
    fn name(&self) -> &str {
        "eventtap"
    }

    fn required_permissions(&self) -> &[Permission] {
        &REQUIRED_PERMISSIONS
    }

    fn start(&mut self, sender: SyncSender<RawEvent>) -> Result<(), CollectorError> {
        if self.worker.is_some() {
            return Err(CollectorError::AlreadyRunning {
                collector: self.name().to_owned(),
            });
        }
        if !self.input_source_prepared {
            return Err(CollectorError::Start {
                collector: self.name().to_owned(),
                message: "keyboard input source monitor was not prepared on the main thread"
                    .to_owned(),
            });
        }
        let mode = self.mode;
        let click_sender = self.click_sender.clone();
        let mut focused_fields = self.focused_fields.take();
        let dropped_events = Arc::clone(&self.dropped_events);
        let degraded_operations = Arc::clone(&self.degraded_operations);
        let current_degraded = Arc::clone(&self.current_degraded);
        let secure_input_enabled = Arc::clone(&self.secure_input_enabled);
        let input_authorizations = self.input_authorizations.clone();
        let secure_input_probe = self.secure_input_probe.clone();
        let ime_state = self.ime_state.clone();
        let text_policy = self.text_policy.clone();
        let focus_context = self.focus_context.clone();
        let (stop_sender, stop_receiver) = sync_channel(1);
        let (ready_sender, ready_receiver) = sync_channel(1);
        let handle = thread::Builder::new()
            .name("zanei-eventtap".to_owned())
            .spawn(move || {
                worker::run(
                    sender,
                    mode,
                    click_sender,
                    &mut focused_fields,
                    stop_receiver,
                    dropped_events,
                    degraded_operations,
                    current_degraded,
                    secure_input_enabled,
                    input_authorizations,
                    secure_input_probe,
                    ime_state,
                    text_policy,
                    focus_context,
                    ready_sender,
                );
                focused_fields
            })
            .map_err(|error| CollectorError::Start {
                collector: self.name().to_owned(),
                message: error.to_string(),
            })?;
        if ready_receiver.recv().is_err() {
            if let Ok(focused_fields) = handle.join() {
                self.focused_fields = focused_fields;
            }
            return Err(CollectorError::Start {
                collector: self.name().to_owned(),
                message: "EventTap worker stopped before initialization".to_owned(),
            });
        }
        self.stop_sender = Some(stop_sender);
        self.worker = Some(Worker { handle });
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(sender) = self.stop_sender.take() {
            let _ = sender.try_send(());
        }
        if let Some(worker) = self.worker.take() {
            if let Ok(focused_fields) = worker.handle.join() {
                self.focused_fields = focused_fields;
            }
        }
        self.current_degraded.store(false, Ordering::Relaxed);
        self.secure_input_enabled.store(false, Ordering::Relaxed);
    }
}

struct Worker {
    handle: JoinHandle<Option<FocusedFieldTracker>>,
}

impl Drop for EventTapCollector {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
