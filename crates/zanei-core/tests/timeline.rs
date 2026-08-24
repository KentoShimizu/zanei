use time::{Duration, OffsetDateTime};
use zanei_core::normalize::normalize;
use zanei_core::schema::{
    App, BrowserMode, BrowserNavigateData, EmptyData, Event, EventData, RawEvent, Window,
};
use zanei_core::store::EventMetadata;
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

    let coarse = build(&events, &[], &options(Granularity::Coarse, 4_000))
        .expect("coarse timeline should build");
    let fine = build(&events, &[], &options(Granularity::Fine, 4_000))
        .expect("fine timeline should build");

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
        build(&events, &[], &options(Granularity::Coarse, 4_000)).expect("timeline should build");

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
        build(&events, &[], &options(Granularity::Coarse, 4_000)).expect("timeline should build");

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
        build(&events, &[], &options(Granularity::Fine, 300)).expect("timeline should degrade");

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
        let timeline = build(&[], &[], &options).expect("empty timeline should build");

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
    let timeline = build(&events, &[], &options).expect("timeline should build");
    let encoded = serialize(&timeline, TimelineFormat::Json).expect("timeline should serialize");
    let json: serde_json::Value = serde_json::from_str(&encoded).expect("output should be JSON");

    assert!(json["sessions"].is_array());
    assert_eq!(json["sessions"][0]["app"], "Safari");
    assert!(json["sessions"][0]["event_ids"].is_array());
    assert_eq!(json["sessions"][0]["event_ids_truncated"], false);
    assert!(json["range"]["since"].is_string());
    assert_eq!(json["sessions"][0]["content_snapshots"], 0);
}

#[test]
fn snapshot_counts_use_the_last_session_start_at_or_before_the_timestamp() {
    let events = vec![
        activate("Safari", "com.apple.Safari", 0),
        activate("Slack", "com.tinyspeck.slackmacgap", 60),
    ];
    let metadata = vec![
        snapshot_for_app(50, 1, "Safari", Some("com.apple.Safari")),
        snapshot_for_app(60, 2, "Slack", Some("com.tinyspeck.slackmacgap")),
    ];

    let timeline = build(&events, &metadata, &options(Granularity::Fine, usize::MAX))
        .expect("timeline should build");

    assert_eq!(timeline.sessions.len(), 2);
    assert_eq!(timeline.sessions[0].content_snapshots, 1);
    assert_eq!(timeline.sessions[1].content_snapshots, 1);
}

#[test]
fn snapshot_counts_prefer_the_latest_same_app_session_and_preserve_unmatched_apps() {
    let events = vec![
        activate("Safari", "com.apple.Safari", 0),
        activate("Slack", "com.tinyspeck.slackmacgap", 60),
    ];
    let metadata = vec![
        snapshot_for_app(61, 1, "Safari", Some("com.apple.Safari")),
        snapshot_for_app(62, 2, "Slack", Some("com.tinyspeck.slackmacgap")),
        snapshot_for_app(63, 3, "Unknown", Some("dev.example.unknown")),
    ];

    let timeline = build(&events, &metadata, &options(Granularity::Fine, usize::MAX))
        .expect("timeline should build");

    assert_eq!(timeline.sessions.len(), 3);
    assert_eq!(timeline.sessions[0].content_snapshots, 1);
    assert_eq!(timeline.sessions[1].content_snapshots, 1);
    assert_eq!(timeline.sessions[2].app, "Unknown");
    assert_eq!(timeline.sessions[2].content_snapshots, 1);
}

#[test]
fn snapshot_only_store_builds_eventless_session_from_metadata() {
    let metadata = vec![
        snapshot_for_app(1, 1, "Notes", None),
        snapshot_for_app(3, 2, "Notes", None),
    ];
    let expected_start = metadata[0].ts.clone();
    let expected_end = metadata[1].ts.clone();
    let timeline = build(&[], &metadata, &options(Granularity::Fine, usize::MAX))
        .expect("snapshot-only timeline should build");

    assert_eq!(timeline.sessions.len(), 1);
    let session = &timeline.sessions[0];
    assert_eq!(session.start, expected_start);
    assert_eq!(session.end, expected_end);
    assert_eq!(session.app, "Notes");
    assert!(session.title_summary.is_none());
    assert!(session.activities.is_empty());
    assert_eq!(session.content_snapshots, 2);
    assert_eq!(session.event_ids.as_deref(), Some([].as_slice()));
    assert_eq!(session.interactions.as_deref(), Some([].as_slice()));

    let encoded = serialize(&timeline, TimelineFormat::Json).expect("timeline should serialize");
    let json: serde_json::Value = serde_json::from_str(&encoded).expect("output should be JSON");
    assert_eq!(json["sessions"][0]["activities"], serde_json::json!([]));
    assert_eq!(json["sessions"][0]["event_ids"], serde_json::json!([]));
    assert_eq!(json["sessions"][0]["interactions"], serde_json::json!([]));

    let mut markdown_options = options(Granularity::Coarse, usize::MAX);
    markdown_options.format = TimelineFormat::Markdown;
    let markdown = build(&[], &metadata, &markdown_options)
        .and_then(|timeline| serialize(&timeline, TimelineFormat::Markdown).map_err(Into::into))
        .expect("snapshot-only markdown should serialize");
    assert!(markdown.contains(&format!(
        "## {expected_start} — {expected_end} · Notes\nContent snapshots: 2"
    )));
    assert!(!markdown.contains("\nTitle:"));
    assert!(!markdown.contains("\n- "));
}

#[test]
fn snapshot_before_first_event_stays_in_its_own_session() {
    let events = vec![activate("Safari", "com.apple.Safari", 10)];
    let metadata = vec![snapshot_for_app(1, 1, "Safari", Some("com.apple.Safari"))];

    let timeline = build(
        &events,
        &metadata,
        &options(Granularity::Coarse, usize::MAX),
    )
    .expect("timeline should build");

    assert_eq!(timeline.sessions.len(), 2);
    assert_eq!(timeline.sessions[0].app, "Safari");
    assert_eq!(timeline.sessions[0].content_snapshots, 1);
    assert!(timeline.sessions[0].activities.is_empty());
    assert_eq!(timeline.sessions[1].content_snapshots, 0);
}

#[test]
fn ordinary_different_app_closes_snapshot_only_session_without_coarse_absorption() {
    let events = vec![activate("Slack", "com.tinyspeck.slackmacgap", 10)];
    let metadata = vec![
        snapshot_for_app(1, 1, "Safari", Some("com.apple.Safari")),
        snapshot_for_app(5, 2, "Safari", Some("com.apple.Safari")),
        snapshot_for_app(20, 3, "Safari", Some("com.apple.Safari")),
    ];

    let timeline = build(
        &events,
        &metadata,
        &options(Granularity::Coarse, usize::MAX),
    )
    .expect("timeline should build");

    assert_eq!(timeline.sessions.len(), 3);
    assert_eq!(timeline.sessions[0].app, "Safari");
    assert_eq!(timeline.sessions[0].content_snapshots, 2);
    assert_eq!(timeline.sessions[1].app, "Slack");
    assert_eq!(timeline.sessions[1].content_snapshots, 0);
    assert_eq!(timeline.sessions[2].app, "Safari");
    assert_eq!(timeline.sessions[2].content_snapshots, 1);
}

#[test]
fn markdown_omits_zero_snapshot_counts_and_includes_nonzero_counts() {
    let events = vec![activate("Safari", "com.apple.Safari", 0)];
    let mut markdown_options = options(Granularity::Coarse, usize::MAX);
    markdown_options.format = TimelineFormat::Markdown;

    let zero = build(&events, &[], &markdown_options).expect("zero-count timeline");
    let zero = serialize(&zero, TimelineFormat::Markdown).expect("zero-count markdown");
    assert!(!zero.contains("Content snapshots:"));

    let counted =
        build(&events, &[safari_snapshot(1, 1)], &markdown_options).expect("counted timeline");
    let counted = serialize(&counted, TimelineFormat::Markdown).expect("counted markdown");
    assert!(counted.contains("Content snapshots: 1"));
}

#[test]
fn snapshot_counts_survive_bounce_absorption_and_adjacent_merge() {
    let bounce_events = vec![
        activate("Safari", "com.apple.Safari", 0),
        activate("Slack", "com.tinyspeck.slackmacgap", 10),
        activate("Safari", "com.apple.Safari", 20),
    ];
    let bounced = build(
        &bounce_events,
        &[safari_snapshot(15, 1)],
        &options(Granularity::Coarse, usize::MAX),
    )
    .expect("bounce timeline");
    assert_eq!(bounced.sessions.len(), 1);
    assert_eq!(bounced.sessions[0].content_snapshots, 1);

    let adjacent_events = vec![
        activate("Safari", "com.apple.Safari", 0),
        navigate(
            "Safari",
            "com.apple.Safari",
            400,
            "https://example.com/long-page",
        ),
    ];
    let metadata = vec![safari_snapshot(1, 2), safari_snapshot(401, 3)];
    let unlimited = build(
        &adjacent_events,
        &metadata,
        &options(Granularity::Coarse, usize::MAX),
    )
    .expect("unlimited adjacent timeline");
    assert_eq!(unlimited.sessions.len(), 2);
    let merged = (MIN_TIMELINE_TOKEN_BUDGET_TOKENS..unlimited.token_estimate)
        .find_map(|budget| {
            let timeline = build(
                &adjacent_events,
                &metadata,
                &options(Granularity::Coarse, budget),
            )
            .ok()?;
            (timeline.sessions.len() == 1 && !timeline.truncated).then_some(timeline)
        })
        .expect("a budget should exercise adjacent same-app merge");
    assert_eq!(merged.sessions[0].content_snapshots, 2);
}

#[test]
fn snapshot_count_participates_in_token_estimation_without_changing_the_ladder() {
    let events = vec![activate("Safari", "com.apple.Safari", 0)];
    let zero = build(&events, &[], &options(Granularity::Coarse, usize::MAX))
        .expect("zero-count timeline");
    let counted = build(
        &events,
        &(0..100)
            .map(|index| safari_snapshot(1, index))
            .collect::<Vec<_>>(),
        &options(Granularity::Coarse, usize::MAX),
    )
    .expect("counted timeline");

    assert!(counted.token_estimate > zero.token_estimate);
    assert_eq!(counted.sessions[0].content_snapshots, 100);
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

    let timeline = build(&events, &[], &options(Granularity::Coarse, usize::MAX))
        .expect("timeline should build");
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
    let unlimited = build(&events, &[], &options(Granularity::Coarse, usize::MAX))
        .expect("unlimited timeline should build");
    let mut without_ids = unlimited.clone();
    for session in &mut without_ids.sessions {
        session.event_ids = None;
        session.event_ids_truncated = false;
    }
    update_estimate(&mut without_ids);

    let timeline = build(
        &events,
        &[],
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
            observed_at: None,
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

fn safari_snapshot(seconds: i64, id: u64) -> EventMetadata {
    snapshot_for_app(seconds, id, "Safari", Some("com.apple.Safari"))
}

fn snapshot_for_app(
    seconds: i64,
    id: u64,
    app_name: &str,
    bundle_id: Option<&str>,
) -> EventMetadata {
    EventMetadata {
        id: format!("snapshot-{id}"),
        ts: zanei_core::normalize::format_timestamp(
            OffsetDateTime::UNIX_EPOCH + Duration::seconds(seconds),
        ),
        bundle_id: bundle_id.map(str::to_owned),
        app_name: app_name.to_owned(),
        window_id: Some(1),
    }
}
