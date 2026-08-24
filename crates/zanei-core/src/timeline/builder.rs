use std::collections::BTreeMap;

use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

use crate::schema::{Event, EventData};
use crate::store::EventMetadata;

use super::activity::{describe, summarize};
use super::model::{Granularity, Interaction, Session, Timeline, TimelineFormat, TimelineOptions};
use super::render::{estimate_tokens, serialize};

const IDLE_GAP: Duration = Duration::minutes(5);
const BOUNCE: Duration = Duration::seconds(30);
const MAX_EVENT_IDS: usize = 100;
pub const MIN_TIMELINE_TOKEN_BUDGET_TOKENS: usize = 34;

#[derive(Debug, thiserror::Error)]
pub enum TimelineError {
    #[error("event {event_id} has an invalid RFC3339 timestamp: {timestamp}")]
    InvalidTimestamp { event_id: String, timestamp: String },
    #[error("timeline token budget must be at least {minimum} tokens")]
    TokenBudgetBelowMinimum { minimum: usize },
    #[error("failed to serialize timeline: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Clone)]
struct RawSession {
    events: Vec<TimedEvent>,
    content_snapshots: u64,
    metadata_span: Option<MetadataSpan>,
}

#[derive(Clone)]
struct MetadataSpan {
    app_name: String,
    bundle_id: Option<String>,
    start: String,
    start_at: OffsetDateTime,
    end: String,
    end_at: OffsetDateTime,
}

#[derive(Clone)]
struct TimedEvent {
    event: Event,
    at: OffsetDateTime,
}

pub fn build(
    events: &[Event],
    snapshot_metadata: &[EventMetadata],
    options: &TimelineOptions,
) -> Result<Timeline, TimelineError> {
    if options.token_budget < MIN_TIMELINE_TOKEN_BUDGET_TOKENS {
        return Err(TimelineError::TokenBudgetBelowMinimum {
            minimum: MIN_TIMELINE_TOKEN_BUDGET_TOKENS,
        });
    }
    let mut timed = parse_events(events)?;
    timed.sort_by_key(|item| (item.at, item.event.mono_ns));
    let mut base_sessions = split_sessions(timed);
    assign_content_snapshots(&mut base_sessions, snapshot_metadata)?;
    let mut granularity = options.granularity;
    let mut raw_sessions = sessions_for_granularity(&base_sessions, granularity);
    let mut timeline = assemble(&raw_sessions, options, granularity);
    update_estimate(&mut timeline, options.format)?;

    if timeline.token_estimate > options.token_budget && granularity == Granularity::Fine {
        granularity = Granularity::Coarse;
        raw_sessions = sessions_for_granularity(&base_sessions, granularity);
        timeline = assemble(&raw_sessions, options, granularity);
        update_estimate(&mut timeline, options.format)?;
    }
    if timeline.token_estimate > options.token_budget && options.format == TimelineFormat::Json {
        for session in &mut timeline.sessions {
            session.event_ids = None;
            session.event_ids_truncated = false;
        }
        update_estimate(&mut timeline, options.format)?;
    }
    if timeline.token_estimate > options.token_budget {
        raw_sessions = merge_adjacent_same_app(raw_sessions);
        let ids_removed = options.format == TimelineFormat::Json
            && timeline
                .sessions
                .iter()
                .all(|session| session.event_ids.is_none());
        timeline = assemble(&raw_sessions, options, granularity);
        if ids_removed {
            for session in &mut timeline.sessions {
                session.event_ids = None;
                session.event_ids_truncated = false;
            }
        }
        update_estimate(&mut timeline, options.format)?;
    }
    while timeline.token_estimate > options.token_budget && !timeline.sessions.is_empty() {
        timeline.sessions.remove(0);
        timeline.truncated = true;
        update_estimate(&mut timeline, options.format)?;
    }
    Ok(timeline)
}

fn parse_events(events: &[Event]) -> Result<Vec<TimedEvent>, TimelineError> {
    events
        .iter()
        .map(|event| {
            let at = OffsetDateTime::parse(&event.ts, &Rfc3339).map_err(|_| {
                TimelineError::InvalidTimestamp {
                    event_id: event.id.clone(),
                    timestamp: event.ts.clone(),
                }
            })?;
            Ok(TimedEvent {
                event: event.clone(),
                at,
            })
        })
        .collect()
}

fn split_sessions(events: Vec<TimedEvent>) -> Vec<RawSession> {
    let mut sessions = Vec::new();
    let mut current = RawSession {
        events: Vec::new(),
        content_snapshots: 0,
        metadata_span: None,
    };
    for item in events {
        let split_for_idle = current
            .events
            .last()
            .is_some_and(|previous| item.at - previous.at > IDLE_GAP);
        let split_for_activation = item.event.event_type == "app.activate"
            && current
                .events
                .last()
                .is_some_and(|previous| app_key(&previous.event) != app_key(&item.event));
        if (split_for_idle || split_for_activation) && !current.events.is_empty() {
            sessions.push(current);
            current = RawSession {
                events: Vec::new(),
                content_snapshots: 0,
                metadata_span: None,
            };
        }
        current.events.push(item);
    }
    if !current.events.is_empty() {
        sessions.push(current);
    }
    sessions
}

fn assign_content_snapshots(
    sessions: &mut Vec<RawSession>,
    metadata: &[EventMetadata],
) -> Result<(), TimelineError> {
    let mut timed = metadata
        .iter()
        .map(|item| {
            let at = OffsetDateTime::parse(&item.ts, &Rfc3339).map_err(|_| {
                TimelineError::InvalidTimestamp {
                    event_id: item.id.clone(),
                    timestamp: item.ts.clone(),
                }
            })?;
            Ok::<_, TimelineError>((at, item))
        })
        .collect::<Result<Vec<_>, TimelineError>>()?;
    timed.sort_unstable_by_key(|(at, _)| *at);
    for (at, item) in timed {
        let matching = sessions.iter().enumerate().rposition(|(index, session)| {
            session
                .start_at()
                .is_some_and(|session_start| session_start <= at)
                && snapshot_matches_session(item, session)
                && snapshot_session_is_open(sessions, index, item, at)
        });
        if let Some(session) = matching.and_then(|index| sessions.get_mut(index)) {
            session.content_snapshots = session.content_snapshots.saturating_add(1);
            if let Some(span) = &mut session.metadata_span {
                span.end.clone_from(&item.ts);
                span.end_at = at;
            }
            continue;
        }

        let insertion = sessions
            .iter()
            .position(|session| session.start_at().is_some_and(|start| start > at))
            .unwrap_or(sessions.len());
        sessions.insert(insertion, RawSession::from_snapshot(item, at));
    }
    Ok(())
}

fn snapshot_matches_session(metadata: &EventMetadata, session: &RawSession) -> bool {
    match metadata.bundle_id.as_deref() {
        Some(bundle_id) => session
            .bundle_id()
            .is_some_and(|session_bundle| session_bundle.eq_ignore_ascii_case(bundle_id)),
        None => session
            .app_name()
            .is_some_and(|app_name| app_name.eq_ignore_ascii_case(&metadata.app_name)),
    }
}

fn snapshot_session_is_open(
    sessions: &[RawSession],
    index: usize,
    metadata: &EventMetadata,
    snapshot_at: OffsetDateTime,
) -> bool {
    let session = &sessions[index];
    if !session.events.is_empty() {
        return true;
    }
    let Some(start_at) = session.start_at() else {
        return false;
    };
    !sessions.iter().any(|candidate| {
        !candidate.events.is_empty()
            && candidate
                .start_at()
                .is_some_and(|at| at > start_at && at <= snapshot_at)
            && !snapshot_matches_session(metadata, candidate)
    })
}

fn sessions_for_granularity(sessions: &[RawSession], granularity: Granularity) -> Vec<RawSession> {
    if granularity == Granularity::Fine {
        sessions.to_vec()
    } else {
        absorb_bounces(sessions)
    }
}

fn absorb_bounces(sessions: &[RawSession]) -> Vec<RawSession> {
    let mut output = Vec::new();
    for session in sessions {
        output.push(session.clone());
        while output.len() >= 3 {
            let last = output.len() - 1;
            if output[last - 2].events.is_empty()
                || output[last - 1].events.is_empty()
                || output[last].events.is_empty()
                || primary_app_key(&output[last - 2]) != primary_app_key(&output[last])
                || session_residence(&output[last - 1], &output[last]) >= BOUNCE
            {
                break;
            }
            let mut trio = output.split_off(output.len() - 3);
            let mut leading = trio.remove(0);
            let bounce = trio.remove(0);
            let trailing = trio.remove(0);
            leading.events.extend(bounce.events);
            leading.events.extend(trailing.events);
            leading.content_snapshots = leading
                .content_snapshots
                .saturating_add(bounce.content_snapshots)
                .saturating_add(trailing.content_snapshots);
            let events = leading.events;
            output.push(RawSession {
                events,
                content_snapshots: leading.content_snapshots,
                metadata_span: None,
            });
        }
    }
    output
}

fn session_residence(session: &RawSession, next: &RawSession) -> Duration {
    let start = session.start_at();
    let next_start = next.start_at();
    match (start, next_start) {
        (Some(start), Some(end)) => end - start,
        _ => Duration::ZERO,
    }
}

fn assemble(
    raw_sessions: &[RawSession],
    options: &TimelineOptions,
    granularity: Granularity,
) -> Timeline {
    Timeline {
        range: options.range.clone(),
        token_estimate: 0,
        truncated: false,
        sessions: raw_sessions
            .iter()
            .map(|session| build_session(session, granularity, options.format))
            .collect(),
    }
}

fn build_session(raw: &RawSession, granularity: Granularity, format: TimelineFormat) -> Session {
    let events: Vec<_> = raw.events.iter().map(|item| item.event.clone()).collect();
    let app = dominant_app(raw);
    let mut event_ids: Vec<_> = events.iter().map(|event| event.id.clone()).collect();
    let event_ids_truncated = event_ids.len() > MAX_EVENT_IDS;
    event_ids.truncate(MAX_EVENT_IDS);
    Session {
        start: raw
            .start_timestamp()
            .map_or_else(String::new, str::to_owned),
        end: raw.end_timestamp().map_or_else(String::new, str::to_owned),
        app: app.clone(),
        title_summary: longest_title(raw),
        activities: if events.is_empty() {
            Vec::new()
        } else {
            summarize(&events, &app)
        },
        content_snapshots: raw.content_snapshots,
        event_ids: (format == TimelineFormat::Json).then_some(event_ids),
        event_ids_truncated,
        interactions: (granularity == Granularity::Fine).then(|| {
            events
                .iter()
                .map(|event| Interaction {
                    ts: event.ts.clone(),
                    activity: describe(event),
                })
                .collect()
        }),
    }
}

fn dominant_app(session: &RawSession) -> String {
    if session.events.is_empty() {
        return session.app_name().unwrap_or_default().to_owned();
    }
    let mut dwell = BTreeMap::<String, i128>::new();
    for (index, item) in session.events.iter().enumerate() {
        let next = session
            .events
            .get(index + 1)
            .map_or(item.at, |value| value.at);
        let nanos = (next - item.at).whole_nanoseconds().max(1);
        *dwell.entry(item.event.app.name.clone()).or_default() += nanos;
    }
    dwell
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
        .map_or_else(String::new, |(app, _)| app)
}

fn longest_title(session: &RawSession) -> Option<String> {
    let mut dwell = BTreeMap::<String, i128>::new();
    let mut current: Option<(String, OffsetDateTime)> = None;
    for item in &session.events {
        let Some(title) = title_at(&item.event) else {
            continue;
        };
        if current.as_ref().is_some_and(|(active, _)| *active == title) {
            continue;
        }
        if let Some((active, since)) = current.replace((title, item.at)) {
            *dwell.entry(active).or_default() += (item.at - since).whole_nanoseconds().max(1);
        }
    }
    if let Some((active, since)) = current {
        let end = session.events.last().map_or(since, |item| item.at);
        *dwell.entry(active).or_default() += (end - since).whole_nanoseconds().max(1);
    }
    dwell
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
        .map(|(title, _)| title)
}

fn title_at(event: &Event) -> Option<String> {
    match &event.data {
        EventData::WindowTitle(_) => event.window.as_ref()?.title.clone(),
        EventData::BrowserNavigate(data) => data.tab_title.clone(),
        _ => None,
    }
}

fn merge_adjacent_same_app(sessions: Vec<RawSession>) -> Vec<RawSession> {
    let mut output: Vec<RawSession> = Vec::new();
    for session in sessions {
        if let Some(previous) = output.last_mut()
            && primary_app_key(previous) == primary_app_key(&session)
        {
            previous.events.extend(session.events);
            previous.content_snapshots = previous
                .content_snapshots
                .saturating_add(session.content_snapshots);
            previous.metadata_span =
                merge_metadata_spans(previous.metadata_span.take(), session.metadata_span);
        } else {
            output.push(session);
        }
    }
    output
}

fn primary_app_key(session: &RawSession) -> String {
    session.events.first().map_or_else(
        || {
            session
                .metadata_span
                .as_ref()
                .map_or_else(String::new, |span| {
                    span.bundle_id
                        .as_deref()
                        .unwrap_or(&span.app_name)
                        .to_ascii_lowercase()
                })
        },
        |item| app_key(&item.event),
    )
}

fn app_key(event: &Event) -> String {
    event
        .app
        .bundle_id
        .as_deref()
        .unwrap_or(&event.app.name)
        .to_ascii_lowercase()
}

impl RawSession {
    fn from_snapshot(metadata: &EventMetadata, at: OffsetDateTime) -> Self {
        Self {
            events: Vec::new(),
            content_snapshots: 1,
            metadata_span: Some(MetadataSpan {
                app_name: metadata.app_name.clone(),
                bundle_id: metadata.bundle_id.clone(),
                start: metadata.ts.clone(),
                start_at: at,
                end: metadata.ts.clone(),
                end_at: at,
            }),
        }
    }

    fn start_at(&self) -> Option<OffsetDateTime> {
        match (self.events.first(), &self.metadata_span) {
            (Some(event), Some(span)) => Some(event.at.min(span.start_at)),
            (Some(event), None) => Some(event.at),
            (None, Some(span)) => Some(span.start_at),
            (None, None) => None,
        }
    }

    fn start_timestamp(&self) -> Option<&str> {
        match (self.events.first(), &self.metadata_span) {
            (Some(event), Some(span)) if event.at <= span.start_at => Some(&event.event.ts),
            (Some(_), Some(span)) => Some(&span.start),
            (Some(event), None) => Some(&event.event.ts),
            (None, Some(span)) => Some(&span.start),
            (None, None) => None,
        }
    }

    fn end_timestamp(&self) -> Option<&str> {
        match (self.events.last(), &self.metadata_span) {
            (Some(event), Some(span)) if event.at >= span.end_at => Some(&event.event.ts),
            (Some(_), Some(span)) => Some(&span.end),
            (Some(event), None) => Some(&event.event.ts),
            (None, Some(span)) => Some(&span.end),
            (None, None) => None,
        }
    }

    fn app_name(&self) -> Option<&str> {
        self.events.first().map_or_else(
            || {
                self.metadata_span
                    .as_ref()
                    .map(|span| span.app_name.as_str())
            },
            |event| Some(event.event.app.name.as_str()),
        )
    }

    fn bundle_id(&self) -> Option<&str> {
        self.events.first().map_or_else(
            || {
                self.metadata_span
                    .as_ref()
                    .and_then(|span| span.bundle_id.as_deref())
            },
            |event| event.event.app.bundle_id.as_deref(),
        )
    }
}

fn merge_metadata_spans(
    left: Option<MetadataSpan>,
    right: Option<MetadataSpan>,
) -> Option<MetadataSpan> {
    match (left, right) {
        (Some(mut left), Some(right)) => {
            if right.start_at < left.start_at {
                left.start = right.start;
                left.start_at = right.start_at;
            }
            if right.end_at > left.end_at {
                left.end = right.end;
                left.end_at = right.end_at;
            }
            Some(left)
        }
        (Some(span), None) | (None, Some(span)) => Some(span),
        (None, None) => None,
    }
}

fn update_estimate(
    timeline: &mut Timeline,
    format: TimelineFormat,
) -> Result<(), serde_json::Error> {
    for _ in 0..3 {
        timeline.token_estimate = estimate_tokens(&serialize(timeline, format)?);
    }
    Ok(())
}
