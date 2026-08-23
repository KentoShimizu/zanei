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
    let Some(current) = transition.current else {
        return;
    };
    let Some(window) = current.window else {
        return;
    };
    let title_only = transition.previous.as_ref().is_some_and(|previous| {
        previous.app.pid == current.app.pid
            && previous.window.as_ref().and_then(|window| window.id) == window.id
    });
    publisher.publish(SnapshotTrigger {
        app: current.app,
        window,
        kind: if title_only {
            SnapshotTriggerKind::Title
        } else {
            SnapshotTriggerKind::Focus
        },
        observed_at: Instant::now(),
    });
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
}
