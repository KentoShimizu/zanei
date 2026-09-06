use std::collections::VecDeque;

use zanei_core::{
    config::capture_policy::{BrowserMode, BrowserPolicy, BrowserUrlRule, IdePolicy, PolicyAction},
    config::{CapturePolicyConfig, CaptureSource, FilterConfig, RedactorKind},
    schema::{
        App, CaptureContext, EmptyData, Event, EventData, InputKeyData, InputKeyKind, Window,
    },
};

use super::*;
use crate::daemon::pipeline::store::StorePersistence;

#[derive(Default)]
struct Writes {
    failures: VecDeque<bool>,
    persisted: Vec<Event>,
}

struct Writer(Arc<Mutex<Writes>>);
impl StorePersistence for Writer {
    fn persist(&mut self, events: &[Event], _: Option<&DaemonState>) -> Result<usize, String> {
        let mut writes = self.0.lock().unwrap();
        if writes.failures.pop_front().unwrap_or(false) {
            return Err("disk full".to_owned());
        }
        writes.persisted.extend_from_slice(events);
        Ok(events.len())
    }
}

fn worker(filter: FilterConfig) -> (Worker, SyncSender<RawEvent>, Arc<Mutex<Writes>>) {
    let (raw_sender, raw_receiver) = mpsc::sync_channel(2);
    let (_, control_receiver) = mpsc::channel();
    let writes = Arc::new(Mutex::new(Writes::default()));
    let health = Arc::new(Mutex::new(StoreHealth::Healthy));
    let destination = Destination::Store(Box::new(StoreDestination::with_writer(
        Box::new(Writer(Arc::clone(&writes))),
        health,
    )));
    let worker = Worker::new(
        raw_receiver,
        control_receiver,
        SourceGate::new(
            &[
                CaptureSource::App,
                CaptureSource::Window,
                CaptureSource::Input,
            ],
            true,
        ),
        PrivacyFilter::new(filter),
        destination,
        Duration::from_secs(60),
        Arc::new(AtomicU64::new(0)),
        Arc::new(Mutex::new(BTreeMap::new())),
    );
    (worker, raw_sender, writes)
}

fn raw(name: &str, title: &str, url: Option<&str>, text: Option<&str>) -> RawEvent {
    let browser = matches!(name, "Safari" | "Google Chrome");
    RawEvent {
        observed_at: None,
        source: "test.collector".to_owned(),
        event_type: if text.is_some() {
            "input.key"
        } else {
            "window.focus"
        }
        .to_owned(),
        app: App {
            name: name.to_owned(),
            bundle_id: browser.then(|| {
                if name == "Safari" {
                    "com.apple.Safari"
                } else {
                    "com.google.Chrome"
                }
                .to_owned()
            }),
            pid: Some(1),
        },
        window: Some(Window {
            title: Some(title.to_owned()),
            id: Some(1),
        }),
        element: None,
        data: text.map_or(EventData::WindowFocus(EmptyData {}), |text| {
            EventData::InputKey(InputKeyData {
                kind: InputKeyKind::Text,
                modifiers: vec![],
                count: 1,
                combo: None,
                text: Some(text.to_owned()),
                field_kind: None,
            })
        }),
        capture_context: CaptureContext {
            url: url.map(Arc::from),
            surface: None,
        },
    }
}

fn policy(names: &[&str]) -> FilterConfig {
    let mut config = FilterConfig::default();
    config.text_content.exclude_apps.clear();
    config.content_snapshot.exclude_apps.clear();
    config.capture_policy = Some(CapturePolicyConfig {
        allowed_apps: names.iter().map(|name| (*name).to_owned()).collect(),
        browser: BrowserPolicy {
            mode: BrowserMode::AllSites,
            default_policy: PolicyAction::Block,
            on_url_unavailable: PolicyAction::Allow,
            block_auth: false,
            block_payments: false,
            allow_list: vec![],
            block_list: vec![],
        },
        ide: IdePolicy {
            block_env_files: false,
            on_file_name_unavailable: PolicyAction::Allow,
        },
    });
    config
}

fn replace(worker: &mut Worker, filter: FilterConfig) {
    let (acknowledge, receiver) = mpsc::sync_channel(1);
    worker
        .handle_control(Control::ReplaceFilterAndFlush {
            filter: PrivacyFilter::new(filter),
            acknowledge,
        })
        .unwrap();
    receiver
        .try_recv()
        .expect("policy application acknowledged");
}

fn fail_flush(worker: &mut Worker, writes: &Arc<Mutex<Writes>>) -> Instant {
    writes.lock().unwrap().failures.push_back(true);
    worker.flush_all().unwrap();
    match worker.destination.store_health().unwrap() {
        StoreHealth::Backoff { retry_at, .. } => retry_at,
        state => panic!("expected backoff, got {state:?}"),
    }
}

#[test]
fn reload_uses_original_app_title_and_url_without_restoring_redacted_metadata() {
    let mut initial = policy(&["Mail alice@example.com", "Safari", "Cursor"]);
    initial.redactors = vec![RedactorKind::Email];
    let (mut worker, _, writes) = worker(initial.clone());
    for event in [
        raw(
            "Mail alice@example.com",
            "alice@example.com",
            None,
            Some("alice@example.com"),
        ),
        raw(
            "Safari",
            "browser",
            Some("https://example.com/alice@example.com"),
            Some("alice@example.com"),
        ),
        raw(
            "Cursor",
            ".env.alice@example.com",
            None,
            Some("alice@example.com"),
        ),
    ] {
        worker.process(event).unwrap();
    }
    let retry_at = fail_flush(&mut worker, &writes);
    let mut updated = initial.clone();
    let rules = updated.capture_policy.as_mut().unwrap();
    rules.browser.block_list.push(BrowserUrlRule {
        host: "example.com".to_owned(),
        path_prefix: "/alice@example.com".to_owned(),
        match_subdomains: false,
    });
    rules.ide.block_env_files = true;
    replace(&mut worker, updated);
    assert!(writes.lock().unwrap().persisted.is_empty());
    assert_eq!(
        worker.destination.batch_len(),
        1,
        "URL and IDE title rejected"
    );
    worker.destination.retry_if_due(retry_at);
    let writes = writes.lock().unwrap();
    assert_eq!(
        writes.persisted.len(),
        1,
        "original app selector remains allowed"
    );
    let stored = &writes.persisted[0];
    assert_eq!(stored.app.name, "Mail [REDACTED:email]");
    assert_eq!(
        stored.window.as_ref().unwrap().title.as_deref(),
        Some("[REDACTED:email]")
    );
    assert!(stored.redaction.applied);
    assert_eq!(stored.redaction.rules, ["email"]);
    assert!(
        !serde_json::to_string(stored)
            .unwrap()
            .contains("alice@example.com")
    );
}

#[test]
fn relaxation_does_not_restore_normalizer_pending_body_or_existing_redaction_history() {
    let mut initial = policy(&["Example"]);
    initial.text_content.exclude_apps.push("Example".to_owned());
    initial.redactors = vec![RedactorKind::Email];
    let (mut worker, _, writes) = worker(initial);
    worker
        .process(raw("Example", "alice@example.com", None, Some("secret")))
        .unwrap();
    assert_eq!(
        worker.destination.batch_len(),
        0,
        "input is still coalescing"
    );
    let mut relaxed = policy(&["Example"]);
    relaxed.redactors.clear();
    replace(&mut worker, relaxed);
    let writes = writes.lock().unwrap();
    assert_eq!(writes.persisted.len(), 1);
    let event = &writes.persisted[0];
    let EventData::InputKey(data) = &event.data else {
        panic!("input");
    };
    assert_eq!(data.text, None);
    assert_eq!(
        event.window.as_ref().unwrap().title.as_deref(),
        Some("[REDACTED:email]")
    );
    assert_eq!(event.redaction.rules, ["email"]);
}

#[test]
fn backoff_controls_keep_raw_queue_bounded_and_recovery_uses_current_policy() {
    let initial = policy(&["Allowed", "Denied"]);
    let (mut worker, sender, writes) = worker(initial);
    worker
        .process(raw("Allowed", "pending", None, None))
        .unwrap();
    let retry_at = fail_flush(&mut worker, &writes);
    sender.send(raw("Denied", "queued", None, None)).unwrap();
    sender.send(raw("Allowed", "queued", None, None)).unwrap();
    let (acknowledge, flushed) = mpsc::sync_channel(1);
    worker.handle_control(Control::Flush(acknowledge)).unwrap();
    replace(&mut worker, policy(&["Allowed"]));
    worker.handle_control(Control::Shutdown).unwrap();
    assert!(matches!(
        sender.try_send(raw("Allowed", "extra", None, None)),
        Err(mpsc::TrySendError::Full(_))
    ));
    assert!(matches!(flushed.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert!(writes.lock().unwrap().persisted.is_empty());
    worker.handle_control(Control::RetryAt(retry_at)).unwrap();
    worker.run().unwrap();
    flushed
        .try_recv()
        .expect("flush completes only after recovery");
    let writes = writes.lock().unwrap();
    assert_eq!(writes.persisted.len(), 2);
    assert!(
        writes
            .persisted
            .iter()
            .all(|event| event.app.name == "Allowed")
    );
}

#[test]
fn url_unavailable_obeys_optional_policy_but_known_sites_keep_scope_rules() {
    for name in ["Safari", "Google Chrome"] {
        for unavailable_allowed in [false, true] {
            let mut config = policy(&[name]);
            config
                .capture_policy
                .as_mut()
                .unwrap()
                .browser
                .on_url_unavailable = if unavailable_allowed {
                PolicyAction::Allow
            } else {
                PolicyAction::Block
            };
            config.exclude_websites.push("denied.example".to_owned());
            let (mut worker, _, writes) = worker(config);
            worker
                .process(raw(name, "unknown", None, Some("unknown body")))
                .unwrap();
            worker
                .process(raw(
                    name,
                    "denied",
                    Some("https://denied.example/path"),
                    Some("denied body"),
                ))
                .unwrap();
            worker.flush_all().unwrap();
            let writes = writes.lock().unwrap();
            let bodies: Vec<_> = writes
                .persisted
                .iter()
                .filter_map(|event| match &event.data {
                    EventData::InputKey(data) => data.text.as_deref(),
                    _ => None,
                })
                .collect();
            assert_eq!(
                bodies,
                if unavailable_allowed {
                    vec!["unknown body"]
                } else {
                    vec![]
                }
            );
        }
    }
}

#[test]
fn selector_bytes_trigger_flush_even_when_stored_event_is_small() {
    let config = policy(&["Safari"]);
    let (mut worker, _, writes) = worker(config);
    let url = format!(
        "https://example.com/{}",
        "x".repeat(super::super::store::MAX_BATCH_BYTES)
    );
    worker
        .process(raw("Safari", "window", Some(&url), None))
        .unwrap();
    assert_eq!(
        worker.destination.batch_len(),
        0,
        "selector bytes cause persistence"
    );
    let writes = writes.lock().unwrap();
    assert_eq!(writes.persisted.len(), 1);
    assert!(serde_json::to_vec(&writes.persisted[0]).unwrap().len() < 1024);
}

#[test]
fn original_selectors_survive_size_limits_and_policy_replacement() {
    let initial = policy(&["Cursor", "Safari"]);
    let title = format!(
        ".env - {}",
        "x".repeat(zanei_core::normalize::URL_TITLE_FIELD_MAX_BYTES)
    );
    let url = format!(
        "https://example.com/blocked/{}",
        "x".repeat(zanei_core::normalize::URL_TITLE_FIELD_MAX_BYTES)
    );
    let (mut worker, _, writes) = worker(initial.clone());
    worker.process(raw("Cursor", &title, None, None)).unwrap();
    let mut browser = raw("Safari", "browser", None, None);
    browser.event_type = "browser.navigate".to_owned();
    browser.data = EventData::BrowserNavigate(zanei_core::schema::BrowserNavigateData {
        url: url.into(),
        tab_title: None,
        mode: zanei_core::schema::BrowserMode::Unknown,
        transition: None,
    });
    // Browser events use their own URL, not CaptureContext. The gate must include them.
    worker.gate = SourceGate::new(&[CaptureSource::Window, CaptureSource::Browser], true);
    worker.process(browser).unwrap();
    let retry_at = fail_flush(&mut worker, &writes);
    assert_eq!(worker.destination.batch_len(), 2);
    let mut updated = initial;
    let policy = updated.capture_policy.as_mut().unwrap();
    policy.ide.block_env_files = true;
    policy.browser.block_list.push(BrowserUrlRule {
        host: "example.com".to_owned(),
        path_prefix: "/blocked/".to_owned(),
        match_subdomains: false,
    });
    replace(&mut worker, updated);
    worker.destination.retry_if_due(retry_at);
    assert!(
        writes.lock().unwrap().persisted.is_empty(),
        "size limiting cannot turn denied selectors into unavailable Allow"
    );
}

#[test]
fn coalescing_does_not_bind_a_denied_title_body_to_an_allowed_title() {
    let mut config = policy(&["Cursor"]);
    config.capture_policy.as_mut().unwrap().ide.block_env_files = true;
    let (mut worker, _, writes) = worker(config);
    worker
        .process(raw("Cursor", "main.rs", None, Some("public")))
        .unwrap();
    worker
        .process(raw("Cursor", ".env", None, Some("private")))
        .unwrap();
    worker.flush_all().unwrap();
    let writes = writes.lock().unwrap();
    assert_eq!(writes.persisted.len(), 1);
    let EventData::InputKey(data) = &writes.persisted[0].data else {
        panic!("input");
    };
    assert_eq!(data.text.as_deref(), Some("public"));
}

#[test]
fn reloading_redactors_adds_history_without_restoring_removed_rules() {
    let mut initial = policy(&["Example"]);
    initial.redactors = vec![RedactorKind::Email];
    let (mut worker, _, writes) = worker(initial.clone());
    worker
        .process(raw(
            "Example",
            "alice@example.com",
            None,
            Some("token=abcd1234"),
        ))
        .unwrap();
    let retry_at = fail_flush(&mut worker, &writes);
    let mut updated = initial.clone();
    updated.redactors = vec![RedactorKind::Token];
    replace(&mut worker, updated);
    initial.redactors.clear();
    replace(&mut worker, initial);
    worker.destination.retry_if_due(retry_at);
    let writes = writes.lock().unwrap();
    assert_eq!(writes.persisted.len(), 1);
    let event = &writes.persisted[0];
    assert_eq!(event.redaction.rules, ["email", "token"]);
    let EventData::InputKey(data) = &event.data else {
        panic!("input");
    };
    assert_eq!(data.text.as_deref(), Some("token=[REDACTED:token]"));
    assert!(
        !serde_json::to_string(event)
            .unwrap()
            .contains("alice@example.com")
    );
}

#[test]
fn coalescing_flushes_selector_bytes_before_reload_can_accumulate_them_in_backoff() {
    let mut normalizer = Normalizer::new();
    let url = format!("https://example.com/{}", "x".repeat(4 * 1024 * 1024));
    let emitted = normalizer
        .push(raw("Safari", "browser", Some(&url), Some("body")))
        .unwrap();
    assert_eq!(
        emitted.len(),
        1,
        "large selector cannot remain in the coalescing window"
    );
    assert!(normalizer.flush().is_empty());
}

#[test]
fn coalescing_flushes_many_distinct_windows_before_they_join_a_retry_batch() {
    let mut normalizer = Normalizer::new();
    let now = time::OffsetDateTime::now_utc();
    let mut emitted = Vec::new();
    for id in 0..512 {
        let mut event = raw("Example", "window", None, Some("body"));
        event.window.as_mut().unwrap().id = Some(id);
        emitted.extend(normalizer.push_at(event, now, 0).unwrap());
    }
    assert_eq!(emitted.len(), 512);
    assert!(normalizer.flush().is_empty());
}
