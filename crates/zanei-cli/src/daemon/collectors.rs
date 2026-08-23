pub(super) mod relay;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::mpsc::{Receiver, SyncSender},
};

use zanei_collector::{Collector, Permission, RawEvent};
use zanei_core::config::{CaptureConfig, CaptureSource, Config, FilterConfig};
use zanei_core::privacy::{CHROME_BUNDLE_ID, PrivacyFilter, PrivacyScope, app_is_allowed_for};
use zanei_core::schema::App;
use zanei_macos::{
    CapturePolicy, SecureInputMonitor,
    ax::{AxCollector, AxCollectorOptions, click_channel},
    chrome::{
        ChromeCollector, ChromeEligibilityPublisher, ChromeObserver, chrome_eligibility_channel,
    },
    content_snapshot::{ContentSnapshotCollector, snapshot_trigger_channel},
    eventtap::{EventTapCollector, EventTapMode, InputSourceObserver},
    focus_context::FocusContext,
    input_authorization_channel,
    workspace::{WorkspaceCollector, WorkspaceEvent, WorkspaceObserver, notification_channel},
};

use super::supervisor::{CollectorCounters, Managed};

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
    capture_policy: CapturePolicy,
    chrome_observer: ChromeObserver,
    focus_context: FocusContext,
    capture: CaptureConfig,
    _secure_input_monitor: Option<SecureInputMonitor>,
    pub(super) retained_collector_health: BTreeMap<String, CollectorCounters>,
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
        let chrome_required = chrome_tracking_required(&config.capture, &config.filter);
        let capture_ax = capture_window
            || capture_ui
            || capture_input
            || capture_browser
            || capture_content
            || chrome_required;
        let (chrome_eligibility, chrome_tracker) =
            chrome_eligibility_channel(config.filter.clone());
        let focus_context = FocusContext::new();
        let chrome_observer = ChromeObserver::new();

        let mut subscribers = Vec::new();
        // Queue the content reset before AX publishes the focus resync trigger.
        let content_lifecycle = capture_content.then(|| subscriber(&mut subscribers));
        let ax_lifecycle = capture_ax.then(|| subscriber(&mut subscribers));
        let workspace = (capture_app || capture_ax || capture_browser || chrome_required)
            .then(|| Managed::new(WorkspaceCollector::new(subscribers)));

        let (click_sender, click_receiver) = click_channel();
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
        let capture_policy = CapturePolicy::new(
            chrome_tracker.clone(),
            config.filter.clone(),
            secure_input_probe.clone(),
        );
        let content_snapshot = match (
            snapshot_trigger_receiver,
            content_lifecycle,
            secure_input_probe.clone(),
        ) {
            (Some(trigger), Some(lifecycle), Some(_secure_input)) => {
                Some(Managed::new(ContentSnapshotCollector::new(
                    trigger,
                    lifecycle,
                    capture_policy.clone(),
                    chrome_observer.clone(),
                    focus_context.clone(),
                )))
            }
            _ => None,
        };
        let ax = ax_lifecycle.map(|lifecycle| {
            Managed::new(AxCollector::new(
                lifecycle,
                click_receiver,
                authorizations,
                AxCollectorOptions {
                    secure_input_probe: secure_input_probe.clone(),
                    capture_text_content: config.capture.text_content,
                    capture_content_snapshot: config.capture.content_snapshot,
                    filter: config.filter.clone(),
                    capture_policy: capture_policy.clone(),
                    chrome_observer: Some(chrome_observer.clone()),
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
                authorization_publisher,
                secure_input_probe,
                capture_policy.clone(),
                chrome_observer.clone(),
                focus_context.clone(),
            ))
        });
        let chrome = chrome_required.then(|| {
            Managed::new(ChromeCollector::new(
                chrome_eligibility.clone(),
                focus_context.clone(),
                chrome_observer.clone(),
            ))
        });

        Self {
            workspace,
            ax,
            content_snapshot,
            eventtap,
            chrome,
            chrome_eligibility,
            capture_policy,
            chrome_observer,
            focus_context: focus_context.clone(),
            capture: config.capture.clone(),
            _secure_input_monitor: secure_input_monitor,
            retained_collector_health: BTreeMap::new(),
            start_errors,
            eventtap_start_gate: super::supervisor::EventTapStartGate::open(),
        }
    }

    pub(crate) fn has_eventtap(&self) -> bool {
        self.eventtap.is_some()
    }

    pub(crate) fn replace_filter(&mut self, filter: FilterConfig) {
        let chrome_required = chrome_tracking_required(&self.capture, &filter);
        self.capture_policy.replace_filter(filter.clone());
        if let Some(ax) = &self.ax {
            ax.collector.replace_filter(filter.clone());
        }
        if let Some(content) = self.content_snapshot.as_mut() {
            match content.collector.filter_replaced() {
                Ok(()) => {
                    self.start_errors.remove("content_snapshot");
                }
                Err(error) => {
                    self.start_errors
                        .insert("content_snapshot".to_owned(), error.to_string());
                }
            }
        }
        match (chrome_required, self.chrome.is_some()) {
            (true, false) => self.add_chrome_collector(),
            (false, true) => {
                self.remove_chrome_collector();
                self.chrome_eligibility.clear_all();
            }
            (true, true) | (false, false) => {}
        }
    }

    fn add_chrome_collector(&mut self) {
        self.chrome = Some(Managed::new(ChromeCollector::new(
            self.chrome_eligibility.clone(),
            self.focus_context.clone(),
            self.chrome_observer.clone(),
        )));
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

#[must_use]
pub(crate) fn chrome_tracking_required(capture: &CaptureConfig, filter: &FilterConfig) -> bool {
    let sources = &capture.sources;
    let captures_ui_or_input =
        sources.contains(&CaptureSource::Ui) || sources.contains(&CaptureSource::Input);
    let chrome = App {
        name: "Google Chrome".to_owned(),
        bundle_id: Some(CHROME_BUNDLE_ID.to_owned()),
        pid: None,
    };
    let captures_browser = sources.contains(&CaptureSource::Browser)
        && app_is_allowed_for(PrivacyScope::AllEvents, &chrome, filter);
    let privacy = PrivacyFilter::new(filter.clone());
    let needs_chrome_privacy = capture.text_content
        && captures_ui_or_input
        && privacy.text_content_app_is_allowed(&chrome)
        || capture.content_snapshot && privacy.content_snapshot_app_is_allowed(&chrome);
    captures_browser || needs_chrome_privacy
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
