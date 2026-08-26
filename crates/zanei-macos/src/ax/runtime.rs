//! AX runtime adapter kept separate from collector orchestration.

use std::time::Duration;

use zanei_core::schema::App;

use crate::{
    CapturePolicy, InputAuthorizations, SecureInputProbe,
    ffi::{
        ax::{
            ManualAccessibilityPolicy, NativeAx, NativeAxError, NativeAxEvent, NativeAxObservation,
            NativeHitTest, NativeWindow,
        },
        workspace::{frontmost_application, running_applications},
    },
    workspace::ApplicationInfo,
};

use super::{
    ClickObservation,
    health::{AxFailure, AxFailureKind, AxFailurePhase, AxFailurePublisher},
};

pub(super) trait AxApi {
    fn running_applications(&self) -> Vec<ApplicationInfo>;
    fn frontmost_application(&self) -> Option<ApplicationInfo>;
    fn attach(
        &mut self,
        pid: i32,
        app: App,
        manual_accessibility: bool,
    ) -> Result<Vec<NativeAxEvent>, AxFailure>;
    fn focused_window(&mut self, pid: i32) -> Result<Option<NativeWindow>, NativeAxError>;
    fn reconcile_manual_accessibility(&mut self, policy: &ManualAccessibilityPolicy);
    fn detach(&mut self, pid: i32) -> Vec<NativeAxEvent>;
    fn poll(&mut self, timeout: Duration) -> Vec<NativeAxObservation>;
    fn flush_pending(&mut self) -> Vec<NativeAxEvent>;
    fn hit_test(&self, click: ClickObservation) -> Option<NativeHitTest>;
    fn take_dropped_events(&self) -> u64;
    fn take_degraded_operations(&self) -> u64;
}

pub(super) struct SystemAxApi {
    native: NativeAx,
}

impl SystemAxApi {
    pub(super) fn new(
        capture_text_content: bool,
        authorizations: InputAuthorizations,
        secure_input_probe: Option<SecureInputProbe>,
        capture_policy: CapturePolicy,
        observe_chrome_loads: bool,
        failure_publisher: AxFailurePublisher,
    ) -> Self {
        Self {
            native: NativeAx::new(
                capture_text_content,
                authorizations,
                secure_input_probe,
                capture_policy,
                observe_chrome_loads,
                failure_publisher,
            ),
        }
    }

    pub(super) fn into_authorizations(self) -> InputAuthorizations {
        self.native.into_authorizations()
    }
}

impl AxApi for SystemAxApi {
    fn running_applications(&self) -> Vec<ApplicationInfo> {
        running_applications()
            .into_iter()
            .map(ApplicationInfo::from)
            .collect()
    }

    fn frontmost_application(&self) -> Option<ApplicationInfo> {
        frontmost_application().map(ApplicationInfo::from)
    }

    fn attach(
        &mut self,
        pid: i32,
        app: App,
        manual_accessibility: bool,
    ) -> Result<Vec<NativeAxEvent>, AxFailure> {
        self.native
            .attach(pid, app, manual_accessibility)
            .map_err(|error| {
                AxFailure::new(
                    Some(i64::from(pid)),
                    AxFailurePhase::Attach,
                    AxFailureKind::NativeAx {
                        operation: error.operation(),
                        code: error.code(),
                    },
                )
            })
    }

    fn focused_window(&mut self, pid: i32) -> Result<Option<NativeWindow>, NativeAxError> {
        self.native.focused_window(pid)
    }

    fn reconcile_manual_accessibility(&mut self, policy: &ManualAccessibilityPolicy) {
        self.native.reconcile_manual_accessibility(policy);
    }

    fn detach(&mut self, pid: i32) -> Vec<NativeAxEvent> {
        self.native.detach(pid)
    }

    fn poll(&mut self, timeout: Duration) -> Vec<NativeAxObservation> {
        self.native.poll(timeout)
    }

    fn flush_pending(&mut self) -> Vec<NativeAxEvent> {
        self.native.flush_pending()
    }

    fn hit_test(&self, click: ClickObservation) -> Option<NativeHitTest> {
        self.native.hit_test(click.pid, click.x, click.y)
    }

    fn take_dropped_events(&self) -> u64 {
        self.native.take_dropped_events()
    }

    fn take_degraded_operations(&self) -> u64 {
        self.native.take_degraded_operations()
    }
}
