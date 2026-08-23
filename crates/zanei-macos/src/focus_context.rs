//! Single process-wide authority for the frontmost application and window.

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
    mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
};

use crate::{ffi::window_list::NativeWindow, workspace::ApplicationInfo};

const TRANSITION_CHANNEL_CAPACITY: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FocusSnapshot {
    pub app: ApplicationInfo,
    pub window: Option<NativeWindow>,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FocusTransition {
    pub previous: Option<FocusSnapshot>,
    pub current: Option<FocusSnapshot>,
}

#[derive(Clone, Default)]
pub struct FocusContext {
    state: Arc<RwLock<FocusState>>,
    dropped: Arc<AtomicU64>,
}

#[derive(Default)]
struct FocusState {
    current: Option<FocusSnapshot>,
    generation: u64,
    subscribers: Vec<SyncSender<FocusTransition>>,
}

pub struct FocusTransitionReceiver {
    receiver: Receiver<FocusTransition>,
}

impl FocusContext {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn current(&self) -> Option<FocusSnapshot> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current
            .clone()
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation
    }

    #[must_use]
    pub fn subscribe(&self) -> FocusTransitionReceiver {
        let (sender, receiver) = sync_channel(TRANSITION_CHANNEL_CAPACITY);
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .subscribers
            .push(sender);
        FocusTransitionReceiver { receiver }
    }

    #[must_use]
    pub fn dropped_transitions(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub(crate) fn activate(
        &self,
        app: ApplicationInfo,
        window: Option<NativeWindow>,
    ) -> Option<FocusTransition> {
        self.transition_to(Some((app, window)))
    }

    pub(crate) fn observe_window(&self, pid: i32, window: NativeWindow) -> Option<FocusTransition> {
        let current = self.current()?;
        (current.app.pid == i64::from(pid))
            .then(|| self.transition_to(Some((current.app, Some(window)))))
            .flatten()
    }

    pub(crate) fn terminate(&self, pid: i64) -> Option<FocusTransition> {
        self.current()
            .filter(|current| current.app.pid == pid)
            .and_then(|_| self.transition_to(None))
    }

    fn transition_to(
        &self,
        candidate: Option<(ApplicationInfo, Option<NativeWindow>)>,
    ) -> Option<FocusTransition> {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if same_identity(state.current.as_ref(), candidate.as_ref()) {
            if let Some((app, window)) = candidate
                && let Some(current) = state.current.as_mut()
            {
                current.app = app;
                current.window = window;
            }
            return None;
        }
        state.generation = state.generation.saturating_add(1);
        let current = candidate.map(|(app, window)| FocusSnapshot {
            app,
            window,
            generation: state.generation,
        });
        let transition = FocusTransition {
            previous: std::mem::replace(&mut state.current, current.clone()),
            current,
        };
        state
            .subscribers
            .retain(|subscriber| match subscriber.try_send(transition.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    false
                }
            });
        Some(transition)
    }
}

impl FocusTransitionReceiver {
    pub fn try_recv(&self) -> Result<FocusTransition, TryRecvError> {
        self.receiver.try_recv()
    }
}

fn same_identity(
    current: Option<&FocusSnapshot>,
    candidate: Option<&(ApplicationInfo, Option<NativeWindow>)>,
) -> bool {
    match (current, candidate) {
        (None, None) => true,
        (Some(current), Some((app, window))) => {
            current.app.pid == app.pid
                && current.window.as_ref().and_then(|window| window.id)
                    == window.as_ref().and_then(|window| window.id)
                && current
                    .window
                    .as_ref()
                    .and_then(|window| window.title.as_ref())
                    == window.as_ref().and_then(|window| window.title.as_ref())
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::ApplicationActivationPolicy;

    fn app(pid: i64) -> ApplicationInfo {
        ApplicationInfo {
            name: format!("App {pid}"),
            bundle_id: Some(format!("dev.example.App{pid}")),
            pid,
            activation_policy: ApplicationActivationPolicy::Regular,
        }
    }

    fn window(id: i64, title: &str) -> NativeWindow {
        NativeWindow {
            title: Some(title.to_owned()),
            id: Some(id),
        }
    }

    #[test]
    fn initial_activation_is_a_transition() {
        let context = FocusContext::new();
        let transitions = context.subscribe();

        let transition = context
            .activate(app(7), Some(window(11, "First")))
            .expect("initial transition");

        assert_eq!(transition.previous, None);
        assert_eq!(
            transition.current.as_ref().map(|value| value.generation),
            Some(1)
        );
        assert_eq!(transitions.try_recv(), Ok(transition));
    }

    #[test]
    fn activation_does_not_require_an_ax_notification() {
        let context = FocusContext::new();
        context.activate(app(7), Some(window(11, "First")));

        let transition = context
            .activate(app(8), Some(window(12, "Second")))
            .expect("application transition");

        assert_eq!(
            transition.previous.as_ref().map(|value| value.app.pid),
            Some(7)
        );
        assert_eq!(
            transition.current.as_ref().map(|value| value.app.pid),
            Some(8)
        );
        assert_eq!(context.generation(), 2);
    }

    #[test]
    fn title_only_change_increments_generation() {
        let context = FocusContext::new();
        context.activate(app(7), Some(window(11, "First")));

        let transition = context
            .observe_window(7, window(11, "Renamed"))
            .expect("title transition");

        assert_eq!(
            transition
                .previous
                .as_ref()
                .and_then(|value| value.window.as_ref())
                .and_then(|value| value.title.as_deref()),
            Some("First")
        );
        assert_eq!(
            transition
                .current
                .as_ref()
                .and_then(|value| value.window.as_ref())
                .and_then(|value| value.title.as_deref()),
            Some("Renamed")
        );
        assert_eq!(context.generation(), 2);
    }

    #[test]
    fn non_frontmost_ax_notification_is_ignored() {
        let context = FocusContext::new();
        context.activate(app(7), Some(window(11, "First")));

        assert_eq!(context.observe_window(8, window(12, "Other")), None);
        assert_eq!(context.generation(), 1);
        assert_eq!(
            context
                .current()
                .and_then(|value| value.window)
                .and_then(|value| value.id),
            Some(11)
        );
    }

    #[test]
    fn terminating_frontmost_application_clears_context() {
        let context = FocusContext::new();
        context.activate(app(7), Some(window(11, "First")));

        let transition = context.terminate(7).expect("termination transition");

        assert!(transition.current.is_none());
        assert!(context.current().is_none());
        assert_eq!(context.generation(), 2);
    }
}
