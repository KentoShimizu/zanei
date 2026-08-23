//! Raw event normalization and coalescing.

mod limits;

use std::time::Instant;

use time::OffsetDateTime;

use crate::schema::{
    CaptureContext, Event, EventData, FieldKind, InputKeyKind, RawEvent, Redaction, ScrollDirection,
};

pub(crate) use limits::enforce_size_limits;
pub use limits::{TEXT_FIELD_MAX_BYTES, URL_TITLE_FIELD_MAX_BYTES};

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
        self.push_at(raw, OffsetDateTime::now_utc(), mono_ns)
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
        source,
        event_type,
        app,
        window,
        element,
        data,
        capture_context,
    } = raw;
    let mut event = Event {
        version: crate::schema::EVENT_SCHEMA_VERSION,
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
    enforce_size_limits(&mut event);
    serde_json::to_value(&event)?;
    Ok(NormalizedEvent {
        event,
        capture_context,
    })
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
        website_host: Option<String>,
        field_kind: Option<FieldKind>,
        kind: InputKeyKind,
    },
    Scroll {
        app: String,
        window: WindowKey,
        direction: ScrollDirection,
    },
    WindowTitle {
        app: String,
        window: WindowKey,
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
                website_host: normalized.capture_context.website_host.clone(),
                field_kind: data.field_kind,
                kind: data.kind,
            })
        }
        EventData::InputScroll(data) => Some(PendingKind::Scroll {
            app,
            window,
            direction: data.direction,
        }),
        EventData::WindowTitle(_) if event.window.is_some() => {
            Some(PendingKind::WindowTitle { app, window })
        }
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
