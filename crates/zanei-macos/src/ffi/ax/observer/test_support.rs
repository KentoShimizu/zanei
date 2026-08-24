use std::sync::{Arc, atomic::AtomicU64, mpsc::sync_channel};

use zanei_core::{config::FilterConfig, schema::App};

use super::{
    super::{
        NativeElement, ObserverContext,
        cf::{CfRef, cf_string},
        value_context::FocusedValueContext,
    },
    AppObserver, RegisteredFocusedTarget, ValueNotificationRegistration,
};
use crate::{
    capture_policy::CapturePolicy, chrome::chrome_eligibility_channel, focused_field::FieldClass,
    text_capture::FocusedTarget,
};

impl AppObserver {
    pub(in crate::ffi::ax) fn fake_with_unknown_focused_target() -> Self {
        let application = cf_string("fake application").expect("application CFString");
        let observer = cf_string("fake observer").expect("observer CFString");
        let source = application.as_ptr();
        let element = cf_string("fake focused element").expect("element CFString");
        let (sender, _receiver) = sync_channel(1);
        let context = Box::new(ObserverContext {
            pid: 7,
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
        });
        let mut focused_target = FocusedTarget::new();
        let generation = focused_target.next_generation();
        let installed = focused_target.transition::<()>(Ok(Some(RegisteredFocusedTarget {
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
            value_notification: ValueNotificationRegistration::default(),
        })));
        assert!(installed.is_ok(), "install fake focused target");
        let filter = FilterConfig::default();
        let (_, chrome) = chrome_eligibility_channel(filter.clone());

        Self {
            application,
            observer,
            source,
            context,
            window_target: None,
            focused_target,
            retired_contexts: Vec::new(),
            stale_targets: Vec::new(),
            degraded: Arc::new(AtomicU64::new(0)),
            capture_text_content: false,
            app: App {
                name: "Fake".to_owned(),
                bundle_id: Some("dev.zanei.fake".to_owned()),
                pid: Some(7),
            },
            capture_policy: CapturePolicy::new(chrome, filter, None),
            manual_accessibility: false,
            accessibility_activation: Default::default(),
            skip_native_cleanup: true,
        }
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
}
