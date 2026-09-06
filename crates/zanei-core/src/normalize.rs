//! Raw event normalization and coalescing.

mod limits;

use std::time::Instant;

use time::OffsetDateTime;

use crate::schema::{
    App, CaptureContext, Event, EventData, FieldKind, InputKeyKind, RawEvent, Redaction,
    ScrollDirection,
};

pub(crate) use limits::enforce_size_limits;
pub use limits::{
    CONTENT_SNAPSHOT_SAFETY_MAX_BYTES, TEXT_FIELD_MAX_BYTES, URL_TITLE_FIELD_MAX_BYTES,
};

// Flush coalescing candidates at the same scale as persistence batches. A single
// incoming event may cross the byte threshold; no second event is retained above it.
const MAX_PENDING_EVENTS: usize = 512;
const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;

const NANOS_PER_MILLISECOND: u32 = 1_000_000;
const KEY_GAP_NS: u64 = 2_000_000_000;
const SCROLL_GAP_NS: u64 = 1_000_000_000;
const WINDOW_TITLE_DEBOUNCE_NS: u64 = 500_000_000;
const NANOS_PER_SECOND: i128 = 1_000_000_000;
const MILLIS_PER_SECOND: i128 = 1_000;
const MAX_ULID_TIMESTAMP_MS: u64 = (1_u64 << 48) - 1;

#[derive(Debug, thiserror::Error)]
pub enum NormalizeError {
    #[error("monotonic timestamp moved backwards from {previous} to {current}")]
    MonotonicRegression { previous: u64, current: u64 },
    #[error("coalesced event count overflowed")]
    CountOverflow,
    #[error("coalesced scroll amount became non-finite")]
    ScrollAmountOverflow,
    #[error("monotonic clock exceeded the supported nanosecond range")]
    MonotonicClockOverflow,
    #[error("wall clock is outside the ULID timestamp range")]
    WallClockOutOfRange,
    #[error("raw event violates the event contract: {0}")]
    EventContract(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq)]
pub struct NormalizedEvent {
    pub event: Event,
    pub capture_context: CaptureContext,
    pub(crate) selectors: PolicySelectors,
}

impl NormalizedEvent {
    /// Binds original policy selectors before applying storage field-size limits.
    #[must_use]
    pub fn new(mut event: Event, capture_context: CaptureContext) -> Self {
        let url = match &event.data {
            EventData::BrowserNavigate(data) => data.url.as_deref().map(std::sync::Arc::from),
            _ => capture_context.url.clone(),
        };
        let selectors = PolicySelectors {
            app: App {
                name: event.app.name.clone(),
                bundle_id: event.app.bundle_id.clone(),
                pid: None,
            },
            title: event
                .window
                .as_ref()
                .and_then(|window| window.title.clone()),
            url,
        };
        enforce_size_limits(&mut event);
        Self {
            event,
            capture_context,
            selectors,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PolicySelectors {
    pub(crate) app: App,
    pub(crate) title: Option<String>,
    pub(crate) url: Option<std::sync::Arc<str>>,
}

impl PolicySelectors {
    pub(crate) fn retained_bytes(&self) -> usize {
        self.app.name.len()
            + self.app.bundle_id.as_ref().map_or(0, String::len)
            + self.title.as_ref().map_or(0, String::len)
            + self.url.as_ref().map_or(0, |url| url.len())
    }
}

impl std::ops::Deref for NormalizedEvent {
    type Target = Event;

    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

impl std::ops::DerefMut for NormalizedEvent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.event
    }
}

pub struct Normalizer {
    monotonic_origin: Instant,
    last_seen_mono_ns: Option<u64>,
    pending: Vec<Pending>,
}

impl Default for Normalizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Normalizer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            monotonic_origin: Instant::now(),
            last_seen_mono_ns: None,
            pending: Vec::new(),
        }
    }

    pub fn push(&mut self, raw: RawEvent) -> Result<Vec<NormalizedEvent>, NormalizeError> {
        let elapsed = self.monotonic_origin.elapsed().as_nanos();
        let mono_ns = u64::try_from(elapsed).map_err(|_| NormalizeError::MonotonicClockOverflow)?;
        let wall_time = raw.observed_at.unwrap_or_else(OffsetDateTime::now_utc);
        self.push_at(raw, wall_time, mono_ns)
    }

    pub fn push_at(
        &mut self,
        raw: RawEvent,
        wall_time: OffsetDateTime,
        mono_ns: u64,
    ) -> Result<Vec<NormalizedEvent>, NormalizeError> {
        if let Some(previous) = self.last_seen_mono_ns
            && mono_ns < previous
        {
            return Err(NormalizeError::MonotonicRegression {
                previous,
                current: mono_ns,
            });
        }
        self.last_seen_mono_ns = Some(mono_ns);

        let event = normalize(raw, wall_time, mono_ns)?;
        let mut emitted = self.flush_expired(mono_ns);
        let Some(kind) = pending_kind(&event) else {
            emitted.extend(self.drain_pending());
            emitted.push(event);
            emitted.sort_by_key(|item| item.event.mono_ns);
            return Ok(emitted);
        };

        if let Some(pending) = self.pending.iter_mut().find(|item| item.kind == kind) {
            merge(pending, event)?;
        } else {
            self.pending.push(Pending {
                event,
                last_mono_ns: mono_ns,
                kind,
            });
        }
        let pending_bytes = self.pending.iter().try_fold(0usize, |total, pending| {
            let event = &pending.event;
            let context = &event.capture_context;
            let surface_bytes = context.surface.as_ref().map_or(0, |surface| {
                surface
                    .applescript_window_id
                    .as_ref()
                    .map_or(0, String::len)
                    + surface.tab_id.as_ref().map_or(0, String::len)
            });
            // Count shared strings conservatively, including the coalescing key's copy.
            let selectors = context.url.as_ref().map_or(0, |url| url.len())
                + surface_bytes
                + event.app.name.len()
                + event.app.bundle_id.as_ref().map_or(0, String::len)
                + event
                    .window
                    .as_ref()
                    .and_then(|window| window.title.as_ref())
                    .map_or(0, String::len);
            serde_json::to_vec(&event.event).map(|bytes| {
                total.saturating_add(
                    bytes.len() + 2 * selectors + 2 * event.selectors.retained_bytes(),
                )
            })
        })?;
        if self.pending.len() >= MAX_PENDING_EVENTS || pending_bytes >= MAX_PENDING_BYTES {
            emitted.extend(self.drain_pending());
        }
        emitted.sort_by_key(|item| item.event.mono_ns);
        Ok(emitted)
    }

    #[must_use]
    pub fn flush(&mut self) -> Vec<NormalizedEvent> {
        self.drain_pending()
    }

    fn flush_expired(&mut self, now_mono_ns: u64) -> Vec<NormalizedEvent> {
        let mut emitted = Vec::new();
        let mut retained = Vec::with_capacity(self.pending.len());
        for pending in self.pending.drain(..) {
            if now_mono_ns - pending.last_mono_ns > pending.kind.window_ns() {
                emitted.push(pending.event);
            } else {
                retained.push(pending);
            }
        }
        self.pending = retained;
        emitted.sort_by_key(|item| item.event.mono_ns);
        emitted
    }

    fn drain_pending(&mut self) -> Vec<NormalizedEvent> {
        let mut events: Vec<_> = self.pending.drain(..).map(|item| item.event).collect();
        events.sort_by_key(|event| event.event.mono_ns);
        events
    }
}

pub fn normalize(
    raw: RawEvent,
    wall_time: OffsetDateTime,
    mono_ns: u64,
) -> Result<NormalizedEvent, NormalizeError> {
    let timestamp_ms = wall_time
        .unix_timestamp_nanos()
        .div_euclid(NANOS_PER_SECOND / MILLIS_PER_SECOND);
    let timestamp_ms = u64::try_from(timestamp_ms)
        .ok()
        .filter(|value| *value <= MAX_ULID_TIMESTAMP_MS)
        .ok_or(NormalizeError::WallClockOutOfRange)?;
    let random = ulid::Ulid::new().random();
    let id = ulid::Ulid::from_parts(timestamp_ms, random);
    let RawEvent {
        observed_at: _,
        source,
        event_type,
        app,
        window,
        element,
        data,
        capture_context,
    } = raw;
    let version = crate::schema::event_schema_version(&event_type).ok_or_else(|| {
        NormalizeError::EventContract(<serde_json::Error as serde::de::Error>::custom(format!(
            "unknown event type: {event_type}"
        )))
    })?;
    let event = Event {
        version,
        id: format!("evt_{id}"),
        ts: format_timestamp(wall_time),
        mono_ns,
        source,
        event_type,
        app,
        window,
        element,
        data,
        redaction: Redaction {
            applied: false,
            rules: Vec::new(),
        },
    };
    let normalized = NormalizedEvent::new(event, capture_context);
    serde_json::to_value(&normalized.event)?;
    Ok(normalized)
}

#[must_use]
pub fn format_timestamp(value: OffsetDateTime) -> String {
    let utc = value.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
        utc.nanosecond() / NANOS_PER_MILLISECOND,
    )
}

struct Pending {
    event: NormalizedEvent,
    last_mono_ns: u64,
    kind: PendingKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PendingKind {
    Key {
        app: String,
        window: WindowKey,
        capture_context: CaptureContext,
        selectors: PolicySelectors,
        field_kind: Option<FieldKind>,
        kind: InputKeyKind,
    },
    Scroll {
        app: String,
        window: WindowKey,
        capture_context: CaptureContext,
        selectors: PolicySelectors,
        direction: ScrollDirection,
    },
    WindowTitle {
        app: String,
        window: WindowKey,
        capture_context: CaptureContext,
    },
}

impl PendingKind {
    const fn window_ns(&self) -> u64 {
        match self {
            Self::Key { .. } => KEY_GAP_NS,
            Self::Scroll { .. } => SCROLL_GAP_NS,
            Self::WindowTitle { .. } => WINDOW_TITLE_DEBOUNCE_NS,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WindowKey {
    Id(i64),
    Title(Option<String>),
}

fn pending_kind(normalized: &NormalizedEvent) -> Option<PendingKind> {
    let event = &normalized.event;
    if event.is_truncated() {
        return None;
    }
    let app = event
        .app
        .bundle_id
        .as_deref()
        .unwrap_or(&event.app.name)
        .to_lowercase();
    let window = window_key(event);
    match &event.data {
        EventData::InputKey(data)
            if matches!(
                data.kind,
                InputKeyKind::Text | InputKeyKind::Navigation | InputKeyKind::Delete
            ) =>
        {
            Some(PendingKind::Key {
                app,
                window,
                capture_context: normalized.capture_context.clone(),
                selectors: normalized.selectors.clone(),
                field_kind: data.field_kind,
                kind: data.kind,
            })
        }
        EventData::InputScroll(data) => Some(PendingKind::Scroll {
            app,
            window,
            capture_context: normalized.capture_context.clone(),
            selectors: normalized.selectors.clone(),
            direction: data.direction,
        }),
        EventData::WindowTitle(_) if event.window.is_some() => Some(PendingKind::WindowTitle {
            app,
            window,
            capture_context: normalized.capture_context.clone(),
        }),
        _ => None,
    }
}

fn window_key(event: &Event) -> WindowKey {
    event
        .window
        .as_ref()
        .map_or(WindowKey::Title(None), |window| {
            window
                .id
                .map_or_else(|| WindowKey::Title(window.title.clone()), WindowKey::Id)
        })
}

fn merge(pending: &mut Pending, incoming: NormalizedEvent) -> Result<(), NormalizeError> {
    let already_truncated = pending.event.event.is_truncated();
    let incoming_mono_ns = incoming.event.mono_ns;
    match (&mut pending.event.event.data, &incoming.event.data) {
        (EventData::InputKey(current), EventData::InputKey(next)) => {
            current.count = current
                .count
                .checked_add(next.count)
                .ok_or(NormalizeError::CountOverflow)?;
            if !already_truncated {
                if let Some(next_text) = &next.text {
                    match &mut current.text {
                        Some(current_text) => current_text.push_str(next_text),
                        None => current.text = Some(next_text.clone()),
                    }
                }
            }
        }
        (EventData::InputScroll(current), EventData::InputScroll(next)) => {
            current.count = current
                .count
                .checked_add(next.count)
                .ok_or(NormalizeError::CountOverflow)?;
            current.amount += next.amount;
            if !current.amount.is_finite() {
                return Err(NormalizeError::ScrollAmountOverflow);
            }
        }
        (EventData::WindowTitle(_), EventData::WindowTitle(_)) => pending.event = incoming,
        _ => unreachable!("pending kind guarantees matching payload variants"),
    }
    enforce_size_limits(&mut pending.event.event);
    pending.last_mono_ns = incoming_mono_ns;
    Ok(())
}
