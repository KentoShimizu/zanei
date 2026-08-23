//! Clipboard copy attribution from keyboard intent and pasteboard changes.

use std::time::{Duration, Instant};

use zanei_collector::RawEvent;
use zanei_core::schema::{ClipboardOrigin, ClipboardPasteData, EventData, FieldKind};

use super::{
    logic::{PasteboardContent, clipboard_copy, clipboard_paste},
    output::{raw_event, unknown_clipboard_event},
};
use crate::{ffi::eventtap::NativeContext, text_capture::TextContentPolicy};

const COPY_CORRELATION_WINDOW: Duration = Duration::from_millis(500);

#[derive(Clone)]
struct CopyIntent {
    context: NativeContext,
    observed_at: Instant,
    text_allowed: bool,
}

enum ClipboardChange {
    Matched(CopyIntent),
    Unknown,
}

pub(super) struct ClipboardTracker {
    last_change_count: i64,
    pending: Option<CopyIntent>,
}

impl ClipboardTracker {
    pub(super) const fn new(initial_change_count: i64) -> Self {
        Self {
            last_change_count: initial_change_count,
            pending: None,
        }
    }

    pub(super) fn has_changed(&self, current_change_count: i64) -> bool {
        current_change_count != self.last_change_count
    }

    pub(super) fn observe_copy(
        &mut self,
        context: &NativeContext,
        observed_at: Instant,
        text_allowed: bool,
    ) {
        self.pending = Some(CopyIntent {
            context: context.clone(),
            observed_at,
            text_allowed,
        });
    }

    fn take_change(
        &mut self,
        current_change_count: i64,
        current_context: Option<&NativeContext>,
        now: Instant,
    ) -> Option<ClipboardChange> {
        if !self.has_changed(current_change_count) {
            return None;
        }
        self.last_change_count = current_change_count;
        let Some(intent) = self.pending.take() else {
            return Some(ClipboardChange::Unknown);
        };
        let same_pid =
            current_context.is_some_and(|context| context.app.pid == intent.context.app.pid);
        let timely = now.saturating_duration_since(intent.observed_at) <= COPY_CORRELATION_WINDOW;
        Some(if same_pid && timely {
            ClipboardChange::Matched(intent)
        } else {
            ClipboardChange::Unknown
        })
    }

    pub(super) fn copy_event<F>(
        &mut self,
        current_change_count: i64,
        current_context: Option<&NativeContext>,
        now: Instant,
        read_content: F,
        secure_input: bool,
        text_policy: &TextContentPolicy,
    ) -> Option<RawEvent>
    where
        F: FnOnce(bool) -> PasteboardContent,
    {
        match self.take_change(current_change_count, current_context, now)? {
            ClipboardChange::Matched(intent) => {
                let include_content = copy_text_allowed(secure_input, &intent, text_policy);
                raw_event(
                    "clipboard.copy",
                    &intent.context,
                    EventData::ClipboardCopy(clipboard_copy(
                        read_content(include_content),
                        ClipboardOrigin::CopyShortcut,
                    )),
                    text_policy,
                )
            }
            ClipboardChange::Unknown => Some(unknown_clipboard_event(EventData::ClipboardCopy(
                clipboard_copy(read_content(false), ClipboardOrigin::Unknown),
            ))),
        }
    }
}

pub(super) fn paste_data<F>(
    read_content: F,
    text_allowed: bool,
    field_kind: Option<FieldKind>,
) -> ClipboardPasteData
where
    F: FnOnce(bool) -> PasteboardContent,
{
    clipboard_paste(read_content(text_allowed), field_kind)
}

fn copy_text_allowed(
    secure_input: bool,
    intent: &CopyIntent,
    text_policy: &TextContentPolicy,
) -> bool {
    let window_id = intent.context.window.as_ref().and_then(|window| window.id);
    !secure_input
        && intent.text_allowed
        && text_policy.allows_window(
            intent.context.app.bundle_id.as_deref(),
            intent.context.app.pid,
            window_id,
        )
}

#[cfg(test)]
mod tests {
    use zanei_core::config::FilterConfig;
    use zanei_core::schema::{ClipboardOrigin, ContentKind};

    use super::*;
    use crate::{
        chrome::chrome_eligibility_channel,
        ffi::eventtap::{NativeApp, NativeWindow},
    };

    fn context(pid: i64) -> NativeContext {
        NativeContext {
            app: NativeApp {
                name: "Example".to_owned(),
                bundle_id: Some("dev.example.App".to_owned()),
                pid,
            },
            window: Some(NativeWindow {
                title: Some("Window".to_owned()),
                id: Some(11),
            }),
        }
    }

    fn text_content(include_content: bool) -> PasteboardContent {
        PasteboardContent {
            kind: super::super::logic::PasteboardKind::Text,
            size_bytes: include_content.then_some(7),
            text: include_content.then(|| "private".to_owned()),
        }
    }

    #[test]
    fn pasteboard_change_without_copy_intent_has_unknown_origin_and_no_body() {
        let mut tracker = ClipboardTracker::new(1);
        let (_, chrome) = chrome_eligibility_channel(FilterConfig::default());
        let event = tracker
            .copy_event(
                2,
                Some(&context(7)),
                Instant::now(),
                text_content,
                false,
                &TextContentPolicy::new(chrome),
            )
            .expect("unknown copy event remains");

        assert_eq!(event.app.name, "Unknown");
        assert_eq!(event.app.pid, None);
        assert_eq!(event.window, None);
        let EventData::ClipboardCopy(data) = event.data else {
            panic!("expected clipboard.copy");
        };
        assert_eq!(data.origin, ClipboardOrigin::Unknown);
        assert_eq!(data.content_kind, ContentKind::Text);
        assert_eq!(data.size_bytes, None);
        assert_eq!(data.text, None);
    }

    #[test]
    fn copy_intent_requires_same_pid_and_short_window() {
        let now = Instant::now();
        let mut tracker = ClipboardTracker::new(1);
        tracker.observe_copy(&context(7), now, true);
        assert!(matches!(
            tracker.take_change(2, Some(&context(8)), now),
            Some(ClipboardChange::Unknown)
        ));

        tracker.observe_copy(&context(7), now, true);
        assert!(matches!(
            tracker.take_change(
                3,
                Some(&context(7)),
                now + COPY_CORRELATION_WINDOW + Duration::from_millis(1),
            ),
            Some(ClipboardChange::Unknown)
        ));

        tracker.observe_copy(&context(7), now, true);
        assert!(matches!(
            tracker.take_change(4, Some(&context(7)), now + COPY_CORRELATION_WINDOW),
            Some(ClipboardChange::Matched(_))
        ));
    }

    #[test]
    fn non_chrome_policy_is_available_to_copy_path() {
        let (_, chrome) = chrome_eligibility_channel(FilterConfig::default());
        let policy = TextContentPolicy::new(chrome);
        let app = context(7);
        assert!(policy.allows_window(
            app.app.bundle_id.as_deref(),
            app.app.pid,
            app.window.and_then(|window| window.id),
        ));
    }

    #[test]
    fn secure_input_suppresses_matched_copy_body() {
        let (_, chrome) = chrome_eligibility_channel(FilterConfig::default());
        let policy = TextContentPolicy::new(chrome);
        let now = Instant::now();
        let app = context(7);
        let mut tracker = ClipboardTracker::new(1);
        tracker.observe_copy(&app, now, true);
        let event = tracker
            .copy_event(2, Some(&app), now, text_content, true, &policy)
            .expect("copy event");

        let EventData::ClipboardCopy(data) = event.data else {
            panic!("expected clipboard.copy");
        };
        assert_eq!(data.origin, ClipboardOrigin::CopyShortcut);
        assert_eq!(data.size_bytes, None);
        assert_eq!(data.text, None);
    }

    #[test]
    fn missing_ax_tracking_suppresses_paste_body() {
        let (_, chrome) = chrome_eligibility_channel(FilterConfig::default());
        let policy = TextContentPolicy::new(chrome);
        let text_allowed = policy.allows_input(Some("dev.example.App"), 7, Some(11), None);

        let data = paste_data(text_content, text_allowed, None);

        assert_eq!(data.content_kind, ContentKind::Text);
        assert_eq!(data.size_bytes, None);
        assert_eq!(data.text, None);
        assert_eq!(data.field_kind, None);
    }
}
