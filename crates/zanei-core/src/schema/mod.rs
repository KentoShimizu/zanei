mod capture_context;
mod content_snapshot;
mod model;
mod payload;

pub use capture_context::{CaptureContext, CaptureSurface};
pub use content_snapshot::{ContentSnapshotCutoff, ContentSnapshotData, ContentSnapshotTrigger};
pub(crate) use model::event_schema_version_for_data;
pub use model::{
    App, Element, Event, KNOWN_EVENT_TYPES, RawEvent, Redaction, Window, event_schema_version,
    is_known_event_type,
};
pub use payload::{
    BrowserMode, BrowserNavigateData, BrowserTransition, BrowserUrl, ClickButton,
    ClipboardCopyData, ClipboardOrigin, ClipboardPasteData, ContentKind, EmptyData, EventData,
    FieldKind, InputKeyData, InputKeyKind, InputScrollData, Modifier, ScrollDirection, UiClickData,
    UiFocusData, UiValueData, WindowTitleData,
};
