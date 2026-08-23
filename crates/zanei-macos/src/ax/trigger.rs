//! AX observation to content snapshot trigger projection.

use std::time::Instant;

use crate::{
    content_snapshot::{SnapshotTrigger, SnapshotTriggerKind, SnapshotTriggerPublisher},
    ffi::ax::NativeAxEvent,
};

use super::AxEventBuilder;

pub(super) fn publish_snapshot_trigger(
    publisher: Option<&SnapshotTriggerPublisher>,
    builder: &AxEventBuilder,
    observation: &NativeAxEvent,
) {
    let Some(publisher) = publisher else {
        return;
    };
    let (pid, window, kind) = match observation {
        NativeAxEvent::WindowFocused { pid, window } => {
            (*pid, window.clone(), SnapshotTriggerKind::Focus)
        }
        NativeAxEvent::WindowTitleChanged { pid, window } => {
            (*pid, window.clone(), SnapshotTriggerKind::Title)
        }
        NativeAxEvent::UiFocused { .. } | NativeAxEvent::UiValueChanged { .. } => return,
    };
    let Some(app) = builder.app(pid) else {
        return;
    };
    publisher.publish(SnapshotTrigger {
        app,
        window,
        kind,
        observed_at: Instant::now(),
    });
}

#[cfg(test)]
mod tests {
    use crate::{
        content_snapshot::{SnapshotTriggerKind, snapshot_trigger_channel},
        ffi::ax::NativeWindow,
        workspace::{ApplicationActivationPolicy, ApplicationInfo},
    };

    use super::*;
    use crate::ax::tests::text_policy;

    fn app() -> ApplicationInfo {
        ApplicationInfo {
            name: "Example".to_owned(),
            bundle_id: Some("dev.example.App".to_owned()),
            pid: 7,
            activation_policy: ApplicationActivationPolicy::Regular,
        }
    }

    #[test]
    fn focus_and_title_observations_publish_without_window_state() {
        let (publisher, receiver) = snapshot_trigger_channel();
        let mut builder = AxEventBuilder::new(text_policy());
        builder.add_app(app());
        let window = NativeWindow {
            title: Some("Window".to_owned()),
            id: Some(11),
        };

        publish_snapshot_trigger(
            Some(&publisher),
            &builder,
            &NativeAxEvent::WindowFocused {
                pid: 7,
                window: window.clone(),
            },
        );
        publish_snapshot_trigger(
            Some(&publisher),
            &builder,
            &NativeAxEvent::WindowTitleChanged { pid: 7, window },
        );

        assert_eq!(
            receiver.try_recv().expect("focus trigger").kind,
            SnapshotTriggerKind::Focus
        );
        assert_eq!(
            receiver.try_recv().expect("title trigger").kind,
            SnapshotTriggerKind::Title
        );
    }
}
