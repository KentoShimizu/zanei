pub(super) mod relay;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::mpsc::{Receiver, SyncSender},
};

use zanei_collector::{Collector, Permission, RawEvent};
use zanei_core::config::{CaptureSource, Config, FilterConfig};
use zanei_core::privacy::{CHROME_BUNDLE_ID, PrivacyFilter};
use zanei_core::schema::App;
use zanei_macos::{
    SecureInputMonitor, TextContentPolicy,
    ax::{AxCollector, AxCollectorOptions, click_channel},
    chrome::{ChromeCollector, ChromeEligibilityPublisher, chrome_eligibility_channel},
    content_snapshot::{ContentSnapshotCollector, snapshot_trigger_channel},
    eventtap::{EventTapCollector, EventTapMode, InputSourceObserver},
    focus_context::FocusContext,
    focused_field_channel, input_authorization_channel,
    workspace::{WorkspaceCollector, WorkspaceEvent, WorkspaceObserver, notification_channel},
};

use super::supervisor::Managed;

#[cfg(test)]
use self::relay::Relay;
#[cfg(test)]
use super::supervisor::{
    ManagedCollector, start_collector, start_collector_if_allowed, supervise_collector,
};

pub(crate) struct CollectorSet {
    pub(super) workspace: Option<Managed<WorkspaceCollector>>,
    pub(super) ax: Option<Managed<AxCollector>>,
    pub(super) content_snapshot: Option<Managed<ContentSnapshotCollector>>,
    pub(super) eventtap: Option<Managed<EventTapCollector>>,
    pub(super) chrome: Option<Managed<ChromeCollector>>,
    chrome_eligibility: ChromeEligibilityPublisher,
    text_policy: TextContentPolicy,
    _secure_input_monitor: Option<SecureInputMonitor>,
    pub(super) start_errors: BTreeMap<String, String>,
    pub(super) eventtap_start_gate: super::supervisor::EventTapStartGate,
}

impl CollectorSet {
    pub(crate) fn new(config: &Config) -> Self {
        let sources = &config.capture.sources;
        let capture_app = sources.contains(&CaptureSource::App);
        let capture_window = sources.contains(&CaptureSource::Window);
        let capture_ui = sources.contains(&CaptureSource::Ui);
        let capture_content = config.capture.content_snapshot;
        let capture_input = sources.contains(&CaptureSource::Input);
        let capture_browser = sources.contains(&CaptureSource::Browser);
        let chrome = App {
            name: "Google Chrome".to_owned(),
            bundle_id: Some(CHROME_BUNDLE_ID.to_owned()),
            pid: None,
        };
        let needs_chrome_privacy = config.capture.text_content && (capture_ui || capture_input)
            || capture_content
                && PrivacyFilter::new(config.filter.clone())
                    .content_snapshot_app_is_allowed(&chrome);
        let capture_ax = capture_window
            || capture_ui
            || capture_input
            || capture_browser
            || capture_content
            || needs_chrome_privacy;
        let (chrome_eligibility, chrome_tracker) =
            chrome_eligibility_channel(config.filter.clone());
        let focus_context = FocusContext::new();
        let text_policy = TextContentPolicy::new(chrome_tracker.clone(), config.filter.clone());

        let mut subscribers = Vec::new();
        let ax_lifecycle = capture_ax.then(|| subscriber(&mut subscribers));
        let content_lifecycle = capture_content.then(|| subscriber(&mut subscribers));
        let chrome_lifecycle =
            (capture_browser || needs_chrome_privacy).then(|| subscriber(&mut subscribers));
        let workspace = (capture_app || capture_ax || capture_browser || needs_chrome_privacy)
            .then(|| Managed::new(WorkspaceCollector::new(subscribers)));

        let (click_sender, click_receiver) = click_channel();
        let focused_field = (capture_ax && capture_input).then(focused_field_channel);
        let (authorization_publisher, authorizations) = input_authorization_channel();
        let authorization_publisher = (capture_ax && capture_input && config.capture.text_content)
            .then_some(authorization_publisher);
        let monitor_required = config.capture.text_content && capture_input || capture_content;
        let mut start_errors = BTreeMap::new();
        let (secure_input_monitor, secure_input_probe) = if monitor_required {
            match SecureInputMonitor::start() {
                Ok((monitor, probe)) => (Some(monitor), Some(probe)),
                Err(error) => {
                    start_errors.insert("secure_input".to_owned(), error.to_string());
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
        let (snapshot_trigger_publisher, snapshot_trigger_receiver) =
            if capture_content && secure_input_probe.is_some() {
                let (publisher, receiver) = snapshot_trigger_channel();
                (Some(publisher), Some(receiver))
            } else {
                (None, None)
            };
        let content_snapshot = match (
            snapshot_trigger_receiver,
            content_lifecycle,
            secure_input_probe.clone(),
        ) {
            (Some(trigger), Some(lifecycle), Some(secure_input)) => {
                Some(Managed::new(ContentSnapshotCollector::new(
                    trigger,
                    lifecycle,
                    secure_input,
                    chrome_tracker,
                    focus_context.clone(),
                    config.filter.clone(),
                )))
            }
            _ => None,
        };
        let ax = ax_lifecycle.map(|lifecycle| {
            Managed::new(AxCollector::new(
                lifecycle,
                click_receiver,
                focused_field
                    .as_ref()
                    .map(|(publisher, _)| publisher.clone()),
                authorizations,
                AxCollectorOptions {
                    secure_input_probe: secure_input_probe.clone(),
                    capture_text_content: config.capture.text_content,
                    capture_content_snapshot: config.capture.content_snapshot,
                    filter: config.filter.clone(),
                    text_policy: text_policy.clone(),
                    snapshot_trigger_publisher,
                    focus_context: focus_context.clone(),
                },
            ))
        });
        let eventtap_mode = match (capture_input, capture_ui) {
            (true, true) => Some(EventTapMode::InputAndClicks {
                capture_text_content: config.capture.text_content,
            }),
            (true, false) => Some(EventTapMode::InputOnly {
                capture_text_content: config.capture.text_content,
            }),
            (false, true) => Some(EventTapMode::ClickOnly),
            (false, false) => None,
        };
        let click_sender = capture_ui.then_some(click_sender);
        let eventtap = eventtap_mode.map(|mode| {
            Managed::new(EventTapCollector::new(
                mode,
                click_sender,
                focused_field.map(|(_, tracker)| tracker),
                authorization_publisher,
                secure_input_probe,
                text_policy.clone(),
                focus_context.clone(),
            ))
        });
        let chrome = chrome_lifecycle.map(|lifecycle| {
            Managed::new(ChromeCollector::new(
                lifecycle,
                chrome_eligibility.clone(),
                focus_context,
            ))
        });

        Self {
            workspace,
            ax,
            content_snapshot,
            eventtap,
            chrome,
            chrome_eligibility,
            text_policy,
            _secure_input_monitor: secure_input_monitor,
            start_errors,
            eventtap_start_gate: super::supervisor::EventTapStartGate::open(),
        }
    }

    pub(crate) fn has_eventtap(&self) -> bool {
        self.eventtap.is_some()
    }

    pub(crate) fn replace_filter(&mut self, filter: FilterConfig) {
        self.chrome_eligibility.replace_filter(filter.clone());
        if let Some(ax) = &self.ax {
            ax.collector.replace_filter(filter.clone());
        } else {
            self.text_policy.replace_filter(filter.clone());
        }
        if let Some(content) = self.content_snapshot.as_mut() {
            match content.collector.replace_filter(filter) {
                Ok(()) => {
                    self.start_errors.remove("content_snapshot");
                }
                Err(error) => {
                    self.start_errors
                        .insert("content_snapshot".to_owned(), error.to_string());
                }
            }
        }
    }

    pub(crate) fn required_permissions(&self) -> BTreeSet<Permission> {
        let mut permissions = BTreeSet::new();
        extend_permissions(&mut permissions, self.workspace.as_ref());
        extend_permissions(&mut permissions, self.ax.as_ref());
        extend_permissions(&mut permissions, self.content_snapshot.as_ref());
        extend_permissions(&mut permissions, self.eventtap.as_ref());
        extend_permissions(&mut permissions, self.chrome.as_ref());
        permissions
    }

    pub(crate) fn prepare_main_thread(&mut self) -> MainThreadObservers {
        let workspace = self.workspace.as_mut().and_then(|managed| {
            match managed.collector.prepare_main_thread() {
                Ok(observer) => {
                    self.start_errors.remove(managed.collector.name());
                    Some(observer)
                }
                Err(error) => {
                    self.start_errors
                        .insert(managed.collector.name().to_owned(), error.to_string());
                    None
                }
            }
        });
        let input_source = self.eventtap.as_mut().and_then(|managed| {
            let collector_name = managed.collector.name().to_owned();
            match managed.collector.prepare_main_thread() {
                Ok(observer) => {
                    self.start_errors.remove(&collector_name);
                    observer
                }
                Err(error) => {
                    self.start_errors.insert(collector_name, error.to_string());
                    None
                }
            }
        });
        MainThreadObservers {
            _workspace: workspace,
            _input_source: input_source,
        }
    }
}

pub(crate) struct MainThreadObservers {
    _workspace: Option<WorkspaceObserver>,
    _input_source: Option<InputSourceObserver>,
}

#[derive(Default)]
pub(crate) struct CollectorHealth {
    pub(crate) dropped: u64,
    pub(crate) degraded: BTreeMap<String, String>,
    pub(crate) collector_failures: BTreeMap<String, u64>,
}

#[derive(Clone)]
pub(crate) struct SourceGate {
    sources: BTreeSet<CaptureSource>,
    content_snapshot: bool,
}

impl SourceGate {
    pub(crate) fn new(sources: &[CaptureSource], content_snapshot: bool) -> Self {
        Self {
            sources: sources.iter().copied().collect(),
            content_snapshot,
        }
    }

    pub(crate) fn allows(&self, event: &RawEvent) -> bool {
        self.allows_type(&event.event_type)
    }

    fn allows_type(&self, event_type: &str) -> bool {
        let source = if event_type.starts_with("app.") {
            Some(CaptureSource::App)
        } else if event_type.starts_with("window.") {
            Some(CaptureSource::Window)
        } else if event_type.starts_with("ui.") {
            Some(CaptureSource::Ui)
        } else if event_type.starts_with("input.") || event_type.starts_with("clipboard.") {
            Some(CaptureSource::Input)
        } else if event_type.starts_with("browser.") {
            Some(CaptureSource::Browser)
        } else if event_type.starts_with("content.") {
            return self.content_snapshot;
        } else {
            None
        };
        source.is_some_and(|source| self.sources.contains(&source))
    }
}

fn subscriber(subscribers: &mut Vec<SyncSender<WorkspaceEvent>>) -> Receiver<WorkspaceEvent> {
    let (sender, receiver) = notification_channel();
    subscribers.push(sender);
    receiver
}

fn extend_permissions<C: Collector>(
    target: &mut BTreeSet<Permission>,
    collector: Option<&Managed<C>>,
) {
    if let Some(collector) = collector {
        target.extend(collector.collector.required_permissions().iter().cloned());
    }
}

pub(crate) fn merge_collector_failures(
    base: &BTreeMap<String, u64>,
    current: &BTreeMap<String, u64>,
) -> BTreeMap<String, u64> {
    let mut merged = base.clone();
    for (collector, failures) in current {
        merged
            .entry(collector.clone())
            .and_modify(|total| *total = total.saturating_add(*failures))
            .or_insert(*failures);
    }
    merged
}

#[cfg(test)]
mod tests;
