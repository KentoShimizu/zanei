//! Clipboard copy attribution from keyboard intent and pasteboard changes.

use std::time::{Duration, Instant};
use time::OffsetDateTime;

use zanei_collector::RawEvent;
use zanei_core::schema::{ClipboardOrigin, ClipboardPasteData, EventData, FieldKind};

use super::{
    logic::{PasteboardContent, clipboard_copy, clipboard_paste},
    output::{raw_event, unknown_clipboard_event},
};
use crate::{
    capture_policy::{CaptureDecision, CapturePolicy},
    ffi::eventtap::NativeContext,
};

const COPY_CORRELATION_WINDOW: Duration = Duration::from_millis(500);

#[derive(Clone)]
struct CopyIntent {
    context: NativeContext,
    observed_monotonic_at: Instant,
    observed_at: OffsetDateTime,
    text_allowed: bool,
    decision: CaptureDecision,
}

pub(super) struct ClipboardOutput {
    pub(super) event: RawEvent,
    pub(super) decision: Option<CaptureDecision>,
}

#[derive(Clone, Copy)]
pub(super) struct ClipboardObservationTime {
    pub(super) monotonic: Instant,
    pub(super) wall: OffsetDateTime,
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
        observed_at: ClipboardObservationTime,
        text_allowed: bool,
        decision: CaptureDecision,
    ) {
        self.pending = Some(CopyIntent {
            context: context.clone(),
            observed_monotonic_at: observed_at.monotonic,
            observed_at: observed_at.wall,
            text_allowed,
            decision,
        });
    }

    fn take_change(
        &mut self,
        current_change_count: i64,
        focus_at_change: Option<&NativeContext>,
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
            focus_at_change.is_some_and(|context| context.app.pid == intent.context.app.pid);
        let timely =
            now.saturating_duration_since(intent.observed_monotonic_at) <= COPY_CORRELATION_WINDOW;
        Some(if same_pid && timely {
            ClipboardChange::Matched(intent)
        } else {
            ClipboardChange::Unknown
        })
    }

    pub(super) fn copy_event<F>(
        &mut self,
        current_change_count: i64,
        focus_at_change: Option<&NativeContext>,
        observed_at: ClipboardObservationTime,
        read_content: F,
        secure_input: bool,
        capture_policy: &CapturePolicy,
    ) -> Option<ClipboardOutput>
    where
        F: FnOnce(bool) -> PasteboardContent,
    {
        match self.take_change(current_change_count, focus_at_change, observed_at.monotonic)? {
            ClipboardChange::Matched(intent) => {
                let include_content = !secure_input && intent.text_allowed;
                let event = raw_event(
                    "clipboard.copy",
                    &intent.context,
                    EventData::ClipboardCopy(clipboard_copy(
                        read_content(include_content),
                        ClipboardOrigin::CopyShortcut,
                    )),
                    capture_policy,
                    intent.observed_at,
                )?;
                Some(ClipboardOutput {
                    event,
                    decision: Some(intent.decision),
                })
            }
            ClipboardChange::Unknown => Some(ClipboardOutput {
                event: unknown_clipboard_event(
                    EventData::ClipboardCopy(clipboard_copy(
                        read_content(false),
                        ClipboardOrigin::Unknown,
                    )),
                    observed_at.wall,
                ),
                decision: None,
            }),
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

    fn observed(monotonic: Instant) -> ClipboardObservationTime {
        ClipboardObservationTime {
            monotonic,
            wall: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn decision(policy: &CapturePolicy, context: &NativeContext) -> CaptureDecision {
        let app = zanei_core::schema::App {
            name: context.app.name.clone(),
            bundle_id: context.app.bundle_id.clone(),
            pid: Some(context.app.pid),
        };
        policy.decision(
            zanei_core::privacy::PrivacyScope::TextContent,
            &app,
            context.window.as_ref().and_then(|window| window.id),
        )
    }

    #[test]
    fn pasteboard_change_without_copy_intent_has_unknown_origin_and_no_body() {
        let mut tracker = ClipboardTracker::new(1);
        let filter = FilterConfig::default();
        let (_, chrome) = chrome_eligibility_channel(filter.clone());
        let output = tracker
            .copy_event(
                2,
                Some(&context(7)),
                observed(Instant::now()),
                text_content,
                false,
                &CapturePolicy::new(chrome, filter, None),
            )
            .expect("unknown copy event remains");
        let event = output.event;

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
        let filter = FilterConfig::default();
        let (_, chrome) = chrome_eligibility_channel(filter.clone());
        let policy = CapturePolicy::new(chrome, filter, None);
        let source = context(7);
        tracker.observe_copy(&source, observed(now), true, decision(&policy, &source));
        assert!(matches!(
            tracker.take_change(2, Some(&context(8)), now),
            Some(ClipboardChange::Unknown)
        ));

        tracker.observe_copy(&source, observed(now), true, decision(&policy, &source));
        assert!(matches!(
            tracker.take_change(
                3,
                Some(&context(7)),
                now + COPY_CORRELATION_WINDOW + Duration::from_millis(1),
            ),
            Some(ClipboardChange::Unknown)
        ));

        tracker.observe_copy(&source, observed(now), true, decision(&policy, &source));
        assert!(matches!(
            tracker.take_change(4, Some(&context(7)), now + COPY_CORRELATION_WINDOW),
            Some(ClipboardChange::Matched(_))
        ));
    }

    #[test]
    fn non_chrome_policy_is_available_to_copy_path() {
        let filter = FilterConfig::default();
        let (_, chrome) = chrome_eligibility_channel(filter.clone());
        let policy = CapturePolicy::new(chrome, filter, None);
        let context = context(7);
        let app = zanei_core::schema::App {
            name: context.app.name,
            bundle_id: context.app.bundle_id,
            pid: Some(context.app.pid),
        };
        assert!(
            policy
                .decision(
                    zanei_core::privacy::PrivacyScope::TextContent,
                    &app,
                    context.window.and_then(|window| window.id),
                )
                .is_allowed()
        );
    }

    #[test]
    fn secure_input_suppresses_matched_copy_body() {
        let filter = FilterConfig::default();
        let (_, chrome) = chrome_eligibility_channel(filter.clone());
        let policy = CapturePolicy::new(chrome, filter, None);
        let now = Instant::now();
        let app = context(7);
        let mut tracker = ClipboardTracker::new(1);
        tracker.observe_copy(&app, observed(now), true, decision(&policy, &app));
        let event = tracker
            .copy_event(2, Some(&app), observed(now), text_content, true, &policy)
            .expect("copy event")
            .event;

        let EventData::ClipboardCopy(data) = event.data else {
            panic!("expected clipboard.copy");
        };
        assert_eq!(data.origin, ClipboardOrigin::CopyShortcut);
        assert_eq!(data.size_bytes, None);
        assert_eq!(data.text, None);
    }

    #[test]
    fn missing_ax_tracking_suppresses_paste_body() {
        let filter = FilterConfig::default();
        let (_, chrome) = chrome_eligibility_channel(filter.clone());
        let policy = CapturePolicy::new(chrome, filter, None);
        let text_allowed = policy
            .input_decision(
                &zanei_core::schema::App {
                    name: "Example".to_owned(),
                    bundle_id: Some("dev.example.App".to_owned()),
                    pid: Some(7),
                },
                Some(11),
                None,
            )
            .is_allowed();

        let data = paste_data(text_content, text_allowed, None);

        assert_eq!(data.content_kind, ContentKind::Text);
        assert_eq!(data.size_bytes, None);
        assert_eq!(data.text, None);
        assert_eq!(data.field_kind, None);
    }
}
