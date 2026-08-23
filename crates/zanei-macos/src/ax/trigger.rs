//! FocusContext transition to content-snapshot trigger projection.

use crate::{content_snapshot::SnapshotTriggerPublisher, focus_context::FocusTransition};

pub(super) fn publish_focus_transition(
    publisher: Option<&SnapshotTriggerPublisher>,
    transition: Option<FocusTransition>,
) {
    let (Some(publisher), Some(transition)) = (publisher, transition) else {
        return;
    };
    publisher.publish_focus_transition(transition);
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use zanei_core::schema::ContentSnapshotTrigger;

    use crate::{
        content_snapshot::{
            SnapshotScheduler, SnapshotTriggerMessage, SnapshotTriggerReceiver,
            snapshot_trigger_channel, snapshot_trigger_channel_with_capacity,
        },
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

    fn projected(receiver: &SnapshotTriggerReceiver) -> SnapshotScheduler {
        let mut scheduler = SnapshotScheduler::default();
        scheduler.observe_message(receiver.try_recv().expect("focus transition"));
        scheduler
    }

    #[test]
    fn activation_projects_a_focus_trigger() {
        let context = FocusContext::new();
        let (publisher, receiver) = snapshot_trigger_channel();

        publish_focus_transition(
            Some(&publisher),
            context.activate(app(7), Some(window(11, "First"))),
        );

        let mut scheduler = projected(&receiver);
        assert_eq!(
            scheduler
                .take_due(Instant::now() + Duration::from_secs(3))
                .expect("settle")
                .trigger,
            ContentSnapshotTrigger::Settle
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

        let mut scheduler = projected(&receiver);
        assert_eq!(
            scheduler
                .take_due(Instant::now() + Duration::from_secs(3))
                .expect("settle")
                .trigger,
            ContentSnapshotTrigger::Settle
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

        let mut scheduler = projected(&receiver);
        assert_eq!(
            scheduler
                .take_due(Instant::now() + Duration::from_secs(3))
                .expect("settle")
                .trigger,
            ContentSnapshotTrigger::Settle
        );
    }

    #[test]
    fn v2_3_focus_change_is_one_queue_message() {
        let context = FocusContext::new();
        context.activate(app(7), Some(window(11, "Previous")));
        let (publisher, receiver) = snapshot_trigger_channel_with_capacity(1);

        publish_focus_transition(
            Some(&publisher),
            context.activate(app(8), Some(window(12, "Current"))),
        );

        assert_eq!(publisher.dropped(), 0);
        let mut scheduler = projected(&receiver);
        assert!(receiver.try_recv().is_err());
        let focus_out = scheduler
            .take_due(Instant::now())
            .expect("previous focus-out");
        assert_eq!(focus_out.trigger, ContentSnapshotTrigger::FocusOut);
        assert_eq!(focus_out.target.app.pid, 7);
        let current = scheduler
            .take_due(Instant::now() + Duration::from_secs(3))
            .expect("current settle");
        assert_eq!(current.trigger, ContentSnapshotTrigger::Settle);
        assert_eq!(current.target.app.pid, 8);
    }

    #[test]
    fn s24_termination_projects_the_previous_focus_out() {
        let context = FocusContext::new();
        context.activate(app(7), Some(window(11, "Before exit")));
        let (publisher, receiver) = snapshot_trigger_channel();

        publish_focus_transition(Some(&publisher), context.terminate(7));

        let SnapshotTriggerMessage::FocusTransition { transition, .. } =
            receiver.try_recv().expect("focus transition")
        else {
            panic!("FocusTransition message");
        };
        assert_eq!(transition.previous.map(|focus| focus.app.pid), Some(7));
        assert!(transition.current.is_none());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn s25_windowless_current_still_projects_the_previous_focus_out() {
        let context = FocusContext::new();
        context.activate(app(7), Some(window(11, "Previous")));
        let (publisher, receiver) = snapshot_trigger_channel();

        publish_focus_transition(Some(&publisher), context.activate(app(8), None));

        let SnapshotTriggerMessage::FocusTransition { transition, .. } =
            receiver.try_recv().expect("focus transition")
        else {
            panic!("FocusTransition message");
        };
        let previous = transition.previous.expect("previous focus");
        assert_eq!(previous.app.pid, 7);
        assert_eq!(previous.window.and_then(|window| window.id), Some(11));
        assert!(
            transition
                .current
                .is_some_and(|current| current.window.is_none())
        );
        assert!(receiver.try_recv().is_err());
    }
}
