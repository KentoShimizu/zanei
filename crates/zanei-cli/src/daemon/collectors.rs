pub(super) mod relay;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::mpsc::{Receiver, SyncSender},
};

use zanei_collector::{Collector, Permission, RawEvent};
use zanei_core::config::{CaptureSource, Config, FilterConfig};
use zanei_macos::{
    TextContentPolicy,
    ax::{AxCollector, click_channel},
    chrome::{ChromeCollector, ChromeEligibilityPublisher, chrome_eligibility_channel},
    eventtap::{EventTapCollector, EventTapMode, InputSourceObserver},
    focused_field_channel, input_authorization_channel, secure_input_channel,
    workspace::{WorkspaceCollector, WorkspaceEvent, WorkspaceObserver, notification_channel},
};

use super::supervisor::Managed;

#[cfg(test)]
use self::relay::Relay;
#[cfg(test)]
use super::supervisor::{ManagedCollector, start_collector, supervise_collector};

pub(crate) struct CollectorSet {
    pub(super) workspace: Option<Managed<WorkspaceCollector>>,
    pub(super) ax: Option<Managed<AxCollector>>,
    pub(super) eventtap: Option<Managed<EventTapCollector>>,
    pub(super) chrome: Option<Managed<ChromeCollector>>,
    chrome_eligibility: ChromeEligibilityPublisher,
    pub(super) start_errors: BTreeMap<String, String>,
}

impl CollectorSet {
    pub(crate) fn new(config: &Config) -> Self {
        let sources = &config.capture.sources;
        let capture_app = sources.contains(&CaptureSource::App);
        let capture_ui = sources.contains(&CaptureSource::Ui);
        let capture_ax = sources.contains(&CaptureSource::Window) || capture_ui;
        let capture_input = sources.contains(&CaptureSource::Input);
        let capture_browser = sources.contains(&CaptureSource::Browser);
        let needs_chrome_privacy = config.capture.text_content && (capture_ui || capture_input);
        let (chrome_eligibility, chrome_tracker) =
            chrome_eligibility_channel(config.filter.clone());
        let text_policy = TextContentPolicy::new(chrome_tracker);

        let mut subscribers = Vec::new();
        let ax_lifecycle = capture_ax.then(|| subscriber(&mut subscribers));
        let chrome_lifecycle =
            (capture_browser || needs_chrome_privacy).then(|| subscriber(&mut subscribers));
        let workspace = (capture_app || capture_ax || capture_browser || needs_chrome_privacy)
            .then(|| Managed::new(WorkspaceCollector::new(subscribers)));

        let (click_sender, click_receiver) = click_channel();
        let focused_field = (capture_ax && capture_input).then(focused_field_channel);
        let (authorization_publisher, authorizations) = input_authorization_channel();
        let authorization_publisher = (capture_ax && capture_input && config.capture.text_content)
            .then_some(authorization_publisher);
        let (secure_input_probe, secure_input_responder) =
            if capture_ax && capture_input && config.capture.text_content {
                let (probe, responder) = secure_input_channel();
                (Some(probe), Some(responder))
            } else {
                (None, None)
            };
        let ax = ax_lifecycle.map(|lifecycle| {
            Managed::new(AxCollector::new(
                lifecycle,
                click_receiver,
                focused_field
                    .as_ref()
                    .map(|(publisher, _)| publisher.clone()),
                authorizations,
                secure_input_probe,
                config.capture.text_content,
                text_policy.clone(),
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
                secure_input_responder,
                text_policy,
            ))
        });
        let chrome = chrome_lifecycle.map(|lifecycle| {
            Managed::new(ChromeCollector::new(lifecycle, chrome_eligibility.clone()))
        });

        Self {
            workspace,
            ax,
            eventtap,
            chrome,
            chrome_eligibility,
            start_errors: BTreeMap::new(),
        }
    }

    pub(crate) fn replace_filter(&self, filter: FilterConfig) {
        self.chrome_eligibility.replace_filter(filter);
    }

    pub(crate) fn required_permissions(&self) -> BTreeSet<Permission> {
        let mut permissions = BTreeSet::new();
        extend_permissions(&mut permissions, self.workspace.as_ref());
        extend_permissions(&mut permissions, self.ax.as_ref());
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
}

impl SourceGate {
    pub(crate) fn new(sources: &[CaptureSource]) -> Self {
        Self {
            sources: sources.iter().copied().collect(),
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

#[cfg(test)]
mod tests;
