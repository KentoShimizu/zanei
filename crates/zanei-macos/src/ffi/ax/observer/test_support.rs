use std::{
    sync::{Arc, atomic::AtomicU64, mpsc::sync_channel},
    time::Instant,
};

use zanei_core::{config::FilterConfig, schema::App};

use super::{
    super::{
        NativeElement, ObserverContext,
        cf::{CfRef, OwnedCf, cf_string},
        element::create_application,
        value_context::FocusedValueContext,
    },
    AppObserver, RegisteredFocusedTarget,
};
use crate::{
    capture_policy::CapturePolicy, chrome::chrome_eligibility_channel, focused_field::FieldClass,
};

impl AppObserver {
    pub(in crate::ffi::ax) fn fake_attached_with_unavailable_application(
        attached_at: Instant,
    ) -> Self {
        Self::fake_with_application(
            create_application(i32::MAX).expect("application AX element"),
            Some(attached_at),
        )
    }

    pub(in crate::ffi::ax) fn fake_with_unknown_focused_target() -> Self {
        Self::fake_with_application(
            cf_string("fake application").expect("application CFString"),
            None,
        )
    }

    fn fake_with_application(application: OwnedCf, attached_at: Option<Instant>) -> Self {
        let observer = cf_string("fake observer").expect("observer CFString");
        let source = application.as_ptr();
        let element = cf_string("fake focused element").expect("element CFString");
        let (sender, _receiver) = sync_channel(1);
        let context = Box::new(ObserverContext {
            pid: 7,
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
        });
        let filter = FilterConfig::default();
        let (_, chrome) = chrome_eligibility_channel(filter.clone());
        let degraded = Arc::new(AtomicU64::new(0));
        let app = App {
            name: "Fake".to_owned(),
            bundle_id: Some("dev.zanei.fake".to_owned()),
            pid: Some(7),
        };
        let capture_policy = CapturePolicy::new(chrome, filter, None);
        let mut observer = match attached_at {
            Some(attached_at) => Self::new_attached(
                application,
                observer,
                source,
                context,
                degraded,
                false,
                app,
                capture_policy,
                false,
                attached_at,
            ),
            None => Self::new(
                application,
                observer,
                source,
                context,
                degraded,
                false,
                app,
                capture_policy,
                false,
            ),
        };
        observer.skip_native_cleanup = true;
        let generation = observer.focused_target.next_generation();
        let installed =
            observer
                .focused_target
                .transition::<()>(Ok(Some(RegisteredFocusedTarget {
                    element,
                    context: FocusedValueContext::new(
                        None,
                        NativeElement {
                            role: None,
                            subrole: None,
                            title: None,
                            value: None,
                            value_len: None,
                        },
                        false,
                        None,
                        generation,
                        FieldClass::Unknown,
                    ),
                })));
        assert!(installed.is_ok(), "install fake focused target");
        observer
    }

    pub(in crate::ffi::ax) fn fake_focused_element(&self) -> CfRef {
        self.focused_target
            .current()
            .expect("fake focused target")
            .element
            .as_ptr()
    }

    pub(in crate::ffi::ax) fn fake_focused_field_class(&self) -> FieldClass {
        self.focused_target
            .current()
            .expect("fake focused target")
            .context
            .field_class
    }

    pub(in crate::ffi::ax) fn fake_degraded_operations(&self) -> u64 {
        self.degraded.load(std::sync::atomic::Ordering::Relaxed)
    }
}
