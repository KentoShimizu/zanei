//! Ephemeral binding between an observed event and the surface it came from.

use std::sync::Arc;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CaptureContext {
    /// Complete URL shared across input events; website policy derives its host from this.
    pub url: Option<Arc<str>>,
    /// Browser/window identity used to keep delayed observations attached to their source.
    pub surface: Option<Box<CaptureSurface>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureSurface {
    /// The CoreGraphics window identity used by AX and event attribution.
    pub cg_window_id: Option<i64>,
    /// The browser window identity returned by AppleScript, when available.
    pub applescript_window_id: Option<String>,
    /// A stable browser tab identity. Safari and other unknown surfaces leave this absent.
    pub tab_id: Option<String>,
}
