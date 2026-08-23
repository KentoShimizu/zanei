//! FocusContext transition to content-snapshot trigger projection.

use std::time::Instant;

use crate::{
    content_snapshot::{SnapshotTrigger, SnapshotTriggerKind, SnapshotTriggerPublisher},
    focus_context::FocusTransition,
};

pub(super) fn publish_focus_transition(
    publisher: Option<&SnapshotTriggerPublisher>,
    transition: Option<FocusTransition>,
) {
    let (Some(publisher), Some(transition)) = (publisher, transition) else {
        return;
    };
    let observed_at = Instant::now();
    let same_window = matches!(
        (&transition.previous, &transition.current),
        (Some(previous), Some(current))
            if previous.app.pid == current.app.pid
                && previous.window.as_ref().and_then(|window| window.id)
                    == current.window.as_ref().and_then(|window| window.id)
    );
    if !same_window
        && let Some(previous) = transition.previous
        && let Some(window) = previous.window
    {
        publisher.publish(SnapshotTrigger {
            app: previous.app,
            window,
            kind: SnapshotTriggerKind::FocusOut,
            observed_at,
        });
    }
    if let Some(current) = transition.current
        && let Some(window) = current.window
    {
        publisher.publish(SnapshotTrigger {
            app: current.app,
            window,
            kind: if same_window && !transition.resynced {
                SnapshotTriggerKind::Title
            } else {
                SnapshotTriggerKind::Focus
            },
            observed_at,
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        content_snapshot::{SnapshotTriggerKind, snapshot_trigger_channel},
        ffi::ax::NativeWindow,
        focus_context::FocusContext,
        workspace::{ApplicationActivationPolicy, ApplicationInfo},
    };

    use super::*;

    fn app(pid: i64) -> ApplicationInfo {
        ApplicationInfo {
            name: "Example".to_owned(),
            bundle_id: Some("dev.example.App".to_owned()),
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
    fn activation_projects_a_focus_trigger() {
        let context = FocusContext::new();
        let (publisher, receiver) = snapshot_trigger_channel();

        publish_focus_transition(
            Some(&publisher),
            context.activate(app(7), Some(window(11, "First"))),
        );

        assert_eq!(
            receiver.try_recv().expect("focus trigger").kind,
            SnapshotTriggerKind::Focus
        );
    }

    #[test]
    fn title_only_transition_projects_a_title_trigger() {
        let context = FocusContext::new();
        context.activate(app(7), Some(window(11, "First")));
        let (publisher, receiver) = snapshot_trigger_channel();

        publish_focus_transition(
            Some(&publisher),
            context.observe_window(7, window(11, "Renamed")),
        );

        assert_eq!(
            receiver.try_recv().expect("title trigger").kind,
            SnapshotTriggerKind::Title
        );
    }

    #[test]
    fn resync_projects_a_focus_trigger_even_when_the_title_changed() {
        let context = FocusContext::new();
        context.activate(app(7), Some(window(11, "Before sleep")));
        let (publisher, receiver) = snapshot_trigger_channel();

        publish_focus_transition(
            Some(&publisher),
            Some(context.resync(app(7), Some(window(11, "After wake")))),
        );

        assert_eq!(
            receiver.try_recv().expect("wake focus trigger").kind,
            SnapshotTriggerKind::Focus
        );
    }

    #[test]
    fn s24_termination_projects_the_previous_focus_out() {
        let context = FocusContext::new();
        context.activate(app(7), Some(window(11, "Before exit")));
        let (publisher, receiver) = snapshot_trigger_channel();

        publish_focus_transition(Some(&publisher), context.terminate(7));

        assert_eq!(
            receiver.try_recv().expect("focus-out trigger").kind,
            SnapshotTriggerKind::FocusOut
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn s25_windowless_current_still_projects_the_previous_focus_out() {
        let context = FocusContext::new();
        context.activate(app(7), Some(window(11, "Previous")));
        let (publisher, receiver) = snapshot_trigger_channel();

        publish_focus_transition(Some(&publisher), context.activate(app(8), None));

        let focus_out = receiver.try_recv().expect("focus-out trigger");
        assert_eq!(focus_out.kind, SnapshotTriggerKind::FocusOut);
        assert_eq!(focus_out.app.pid, 7);
        assert_eq!(focus_out.window.id, Some(11));
        assert!(receiver.try_recv().is_err());
    }
}
