use time::{Duration, OffsetDateTime};
use zanei_core::normalize::normalize;
use zanei_core::schema::{
    App, BrowserMode, BrowserNavigateData, EmptyData, Event, EventData, RawEvent, Window,
};
use zanei_core::timeline::{
    Granularity, MIN_TIMELINE_TOKEN_BUDGET_TOKENS, TimeRange, TimelineError, TimelineFormat,
    TimelineOptions, build, estimate_tokens, serialize,
};

#[test]
fn coarse_sessions_absorb_short_app_bounces_but_fine_sessions_keep_them() {
    let events = vec![
        activate("Safari", "com.apple.Safari", 0),
        navigate("Safari", "com.apple.Safari", 10, "https://example.com/a"),
        activate("Slack", "com.tinyspeck.slackmacgap", 60),
        activate("Safari", "com.apple.Safari", 80),
    ];

    let coarse =
        build(&events, &options(Granularity::Coarse, 4_000)).expect("coarse timeline should build");
    let fine =
        build(&events, &options(Granularity::Fine, 4_000)).expect("fine timeline should build");

    assert_eq!(coarse.sessions.len(), 1);
    assert_eq!(coarse.sessions[0].app, "Safari");
    assert_eq!(fine.sessions.len(), 3);
}

#[test]
fn coarse_sessions_absorb_repeated_short_bounces() {
    let events = vec![
        activate("Safari", "com.apple.Safari", 0),
        activate("Slack", "com.tinyspeck.slackmacgap", 10),
        activate("Safari", "com.apple.Safari", 20),
        activate("Slack", "com.tinyspeck.slackmacgap", 30),
        activate("Safari", "com.apple.Safari", 40),
    ];

    let timeline =
        build(&events, &options(Granularity::Coarse, 4_000)).expect("timeline should build");

    assert_eq!(timeline.sessions.len(), 1);
    assert_eq!(timeline.sessions[0].app, "Safari");
}

#[test]
fn idle_gaps_over_five_minutes_split_sessions() {
    let events = vec![
        activate("Safari", "com.apple.Safari", 0),
        navigate("Safari", "com.apple.Safari", 1, "https://example.com/a"),
        navigate("Safari", "com.apple.Safari", 302, "https://example.com/b"),
    ];

    let timeline =
        build(&events, &options(Granularity::Coarse, 4_000)).expect("timeline should build");

    assert_eq!(timeline.sessions.len(), 2);
}

#[test]
fn token_budget_degrades_fine_output_and_truncates_old_sessions() {
    let mut events = Vec::new();
    for index in 0..20 {
        let seconds = i64::from(index) * 400;
        let app = format!("App{index}");
        let bundle = format!("com.example.app{index}");
        events.push(activate(&app, &bundle, seconds));
        events.push(navigate(
            &app,
            &bundle,
            seconds + 1,
            &format!("https://example{index}.com/page"),
        ));
    }

    let timeline =
        build(&events, &options(Granularity::Fine, 300)).expect("timeline should degrade");

    assert!(timeline.truncated);
    assert!(timeline.sessions.len() < 20);
    assert!(
        timeline
            .sessions
            .iter()
            .all(|session| session.interactions.is_none())
    );
    assert!(timeline.token_estimate <= 300);
}

#[test]
fn minimum_token_budget_covers_every_empty_timeline_envelope() {
    for format in [TimelineFormat::Markdown, TimelineFormat::Json] {
        let mut options = options(Granularity::Coarse, usize::MAX);
        options.format = format;
        let timeline = build(&[], &options).expect("empty timeline should build");

        assert!(
            MIN_TIMELINE_TOKEN_BUDGET_TOKENS >= timeline.token_estimate,
            "{format:?} empty envelope requires {} tokens",
            timeline.token_estimate
        );
    }
}

#[test]
fn token_budget_below_empty_envelope_minimum_is_rejected() {
    let error = build(
        &[],
        &options(Granularity::Coarse, MIN_TIMELINE_TOKEN_BUDGET_TOKENS - 1),
    )
    .expect_err("budget below the minimum should fail");

    assert!(matches!(
        error,
        TimelineError::TokenBudgetBelowMinimum {
            minimum: MIN_TIMELINE_TOKEN_BUDGET_TOKENS
        }
    ));
}

#[test]
fn json_output_uses_the_public_sessions_array_shape() {
    let events = vec![activate("Safari", "com.apple.Safari", 0)];
    let options = options(Granularity::Coarse, 4_000);
    let timeline = build(&events, &options).expect("timeline should build");
    let encoded = serialize(&timeline, TimelineFormat::Json).expect("timeline should serialize");
    let json: serde_json::Value = serde_json::from_str(&encoded).expect("output should be JSON");

    assert!(json["sessions"].is_array());
    assert_eq!(json["sessions"][0]["app"], "Safari");
    assert!(json["sessions"][0]["event_ids"].is_array());
    assert_eq!(json["sessions"][0]["event_ids_truncated"], false);
    assert!(json["range"]["since"].is_string());
}

#[test]
fn session_event_ids_are_capped_at_one_hundred() {
    let events: Vec<_> = (0..101)
        .map(|seconds| {
            navigate(
                "Safari",
                "com.apple.Safari",
                seconds,
                &format!("https://example.com/{seconds}"),
            )
        })
        .collect();

    let timeline =
        build(&events, &options(Granularity::Coarse, usize::MAX)).expect("timeline should build");
    let session = timeline.sessions.first().expect("one timeline session");

    assert_eq!(session.event_ids.as_ref().map(Vec::len), Some(100));
    assert!(session.event_ids_truncated);
}

#[test]
fn token_budget_removes_all_event_ids_before_dropping_sessions() {
    let events: Vec<_> = (0..101)
        .map(|seconds| {
            navigate(
                "Safari",
                "com.apple.Safari",
                seconds,
                &format!("https://example.com/{seconds}"),
            )
        })
        .collect();
    let unlimited = build(&events, &options(Granularity::Coarse, usize::MAX))
        .expect("unlimited timeline should build");
    let mut without_ids = unlimited.clone();
    for session in &mut without_ids.sessions {
        session.event_ids = None;
        session.event_ids_truncated = false;
    }
    update_estimate(&mut without_ids);

    let timeline = build(
        &events,
        &options(Granularity::Coarse, without_ids.token_estimate),
    )
    .expect("budgeted timeline should build");

    assert_eq!(timeline.sessions.len(), 1);
    assert!(timeline.sessions[0].event_ids.is_none());
    assert!(!timeline.sessions[0].event_ids_truncated);
    assert!(timeline.token_estimate <= without_ids.token_estimate);
}

fn update_estimate(timeline: &mut zanei_core::timeline::Timeline) {
    for _ in 0..3 {
        let encoded = serialize(timeline, TimelineFormat::Json).expect("timeline should serialize");
        timeline.token_estimate = estimate_tokens(&encoded);
    }
}

fn options(granularity: Granularity, token_budget: usize) -> TimelineOptions {
    TimelineOptions {
        range: TimeRange {
            since: "1970-01-01T00:00:00.000Z".to_owned(),
            until: "1970-01-02T00:00:00.000Z".to_owned(),
        },
        token_budget,
        granularity,
        format: TimelineFormat::Json,
    }
}

fn activate(app: &str, bundle_id: &str, seconds: i64) -> Event {
    event(
        app,
        bundle_id,
        seconds,
        "app.activate",
        EventData::AppActivate(EmptyData {}),
    )
}

fn navigate(app: &str, bundle_id: &str, seconds: i64, url: &str) -> Event {
    event(
        app,
        bundle_id,
        seconds,
        "browser.navigate",
        EventData::BrowserNavigate(BrowserNavigateData {
            url: url.to_owned().into(),
            tab_title: Some(format!("Page at {url}")),
            mode: BrowserMode::Normal,
            transition: None,
        }),
    )
}

fn event(app: &str, bundle_id: &str, seconds: i64, event_type: &str, data: EventData) -> Event {
    normalize(
        RawEvent {
            source: "macos.workspace".to_owned(),
            event_type: event_type.to_owned(),
            app: App {
                name: app.to_owned(),
                bundle_id: Some(bundle_id.to_owned()),
                pid: Some(1),
            },
            window: Some(Window {
                title: Some("Window".to_owned()),
                id: Some(1),
            }),
            element: None,
            data,
            capture_context: Default::default(),
        },
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(seconds),
        u64::try_from(seconds).expect("fixture seconds are non-negative") * 1_000_000_000,
    )
    .expect("fixture wall time is representable")
    .event
}
