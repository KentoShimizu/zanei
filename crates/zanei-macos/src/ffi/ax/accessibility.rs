use super::{cf::CfRef, element::set_boolean_attribute};

const MANUAL_ACCESSIBILITY: &str = "AXManualAccessibility";

pub(super) fn set_manual_accessibility(
    application: CfRef,
    pid: i32,
    capture_text_content: bool,
    enabled: bool,
) {
    let Some(enabled) = manual_accessibility_setting(capture_text_content, enabled) else {
        return;
    };
    // AXEnhancedUserInterface is intentionally avoided because it can cause window-resize jank.
    if let Err(error) = set_boolean_attribute(application, MANUAL_ACCESSIBILITY, enabled)
        && enabled
        && !error.is_attribute_unsupported()
    {
        crate::trace::trace!(
            "component=ax phase=attach action=manual_accessibility pid={} operation={} code={}",
            pid,
            error.operation(),
            error.code()
        );
    }
}

const fn manual_accessibility_setting(capture_text_content: bool, enabled: bool) -> Option<bool> {
    if capture_text_content {
        Some(enabled)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::manual_accessibility_setting;

    #[test]
    fn lifecycle_is_gated_by_text_capture() {
        assert_eq!(manual_accessibility_setting(false, true), None);
        assert_eq!(manual_accessibility_setting(false, false), None);
        assert_eq!(manual_accessibility_setting(true, true), Some(true));
        assert_eq!(manual_accessibility_setting(true, false), Some(false));
    }
}
