mod store;
mod worker;

#[cfg(test)]
use std::time::Instant;
use std::{
    collections::BTreeMap,
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use zanei_collector::{COLLECTOR_CHANNEL_CAPACITY, RawEvent};
#[cfg(test)]
use zanei_core::schema::Event;
use zanei_core::{
    config::{Config, FilterConfig},
    privacy::PrivacyFilter,
    sink::StreamSink,
    store::{DaemonState, StoreWriter},
};

use super::{DaemonError, collectors::SourceGate};
use store::StoreDestination;
pub(crate) use store::StoreHealth;
#[cfg(test)]
use store::StorePersistence;
#[cfg(test)]
use worker::MAX_BATCH_EVENTS;
use worker::{Control, Destination, Worker};

pub(crate) type SharedStoreWriter = Arc<Mutex<StoreWriter>>;

pub(crate) struct Pipeline {
    raw_sender: SyncSender<RawEvent>,
    control_sender: mpsc::Sender<Control>,
    handle: Option<JoinHandle<Result<(), DaemonError>>>,
    dropped: Arc<AtomicU64>,
    degraded: Arc<Mutex<BTreeMap<String, String>>>,
    store_health: Arc<Mutex<StoreHealth>>,
}

impl Pipeline {
    pub(crate) fn store(config: &Config, writer: SharedStoreWriter) -> Result<Self, DaemonError> {
        let store_health = Arc::new(Mutex::new(StoreHealth::Healthy));
        let destination = Destination::Store(Box::new(StoreDestination::new(
            writer,
            Arc::clone(&store_health),
        )));
        Self::spawn(config, destination, store_health)
    }

    pub(crate) fn stream(
        config: &Config,
        writer: Box<dyn Write + Send>,
    ) -> Result<Self, DaemonError> {
        Self::spawn(
            config,
            Destination::Stream(StreamSink::new(writer)),
            Arc::new(Mutex::new(StoreHealth::Healthy)),
        )
    }

    fn spawn(
        config: &Config,
        destination: Destination,
        store_health: Arc<Mutex<StoreHealth>>,
    ) -> Result<Self, DaemonError> {
        let (raw_sender, raw_receiver) = mpsc::sync_channel(COLLECTOR_CHANNEL_CAPACITY);
        let (control_sender, control_receiver) = mpsc::channel();
        let dropped = Arc::new(AtomicU64::new(0));
        let degraded = Arc::new(Mutex::new(BTreeMap::new()));
        let worker_dropped = Arc::clone(&dropped);
        let worker_degraded = Arc::clone(&degraded);
        let gate = SourceGate::new(&config.capture.sources);
        let filter = PrivacyFilter::new(config.filter.clone());
        let batch_interval = Duration::from_secs(config.output.batch_interval_s);
        let handle = thread::Builder::new()
            .name("zanei-pipeline".to_owned())
            .spawn(move || {
                Worker::new(
                    raw_receiver,
                    control_receiver,
                    gate,
                    filter,
                    destination,
                    batch_interval,
                    worker_dropped,
                    worker_degraded,
                )
                .run()
            })
            .map_err(|source| DaemonError::ThreadSpawn {
                thread: "pipeline",
                source,
            })?;
        Ok(Self {
            raw_sender,
            control_sender,
            handle: Some(handle),
            dropped,
            degraded,
            store_health,
        })
    }

    #[cfg(test)]
    fn store_with_persistence(
        config: &Config,
        writer: Box<dyn StorePersistence>,
    ) -> Result<Self, DaemonError> {
        let store_health = Arc::new(Mutex::new(StoreHealth::Healthy));
        let destination = Destination::Store(Box::new(StoreDestination::with_writer(
            writer,
            Arc::clone(&store_health),
        )));
        Self::spawn(config, destination, store_health)
    }

    #[cfg(test)]
    pub(super) fn panicking_store(config: &Config) -> Result<Self, DaemonError> {
        Self::store_with_persistence(config, Box::new(PanickingStore))
    }

    pub(crate) fn sender(&self) -> &SyncSender<RawEvent> {
        &self.raw_sender
    }

    pub(crate) fn heartbeat(&self, state: DaemonState) -> Result<(), DaemonError> {
        self.control_sender
            .send(Control::Heartbeat(state))
            .map_err(|_| DaemonError::PipelineControl {
                operation: "heartbeat",
            })
    }

    pub(crate) fn flush(&self) -> Result<(), DaemonError> {
        let (acknowledge, acknowledged) = mpsc::sync_channel(0);
        self.control_sender
            .send(Control::Flush(acknowledge))
            .map_err(|_| DaemonError::PipelineControl { operation: "flush" })?;
        acknowledged
            .recv()
            .map_err(|_| DaemonError::PipelineControl { operation: "flush" })
    }

    pub(crate) fn replace_filter(&self, filter_config: &FilterConfig) -> Result<(), DaemonError> {
        let (acknowledge, acknowledged) = mpsc::sync_channel(0);
        self.control_sender
            .send(Control::FlushAndReplaceFilter {
                filter: PrivacyFilter::new(filter_config.clone()),
                acknowledge,
            })
            .map_err(|_| DaemonError::PipelineControl {
                operation: "replace privacy filter",
            })?;
        acknowledged
            .recv()
            .map_err(|_| DaemonError::PipelineControl {
                operation: "replace privacy filter",
            })
    }

    pub(crate) fn shutdown(&mut self) -> Result<(), DaemonError> {
        let _ = self.control_sender.send(Control::Shutdown);
        let handle = self
            .handle
            .take()
            .ok_or(DaemonError::ThreadTerminated { thread: "pipeline" })?;
        handle
            .join()
            .map_err(|_| DaemonError::ThreadTerminated { thread: "pipeline" })?
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(crate) fn store_health(&self) -> Result<StoreHealth, DaemonError> {
        self.store_health
            .lock()
            .map(|health| health.clone())
            .map_err(|_| DaemonError::SynchronizationPoisoned {
                name: "store health",
            })
    }

    #[cfg(test)]
    fn retry_store_at(&self, now: Instant) -> Result<(), DaemonError> {
        self.control_sender
            .send(Control::RetryAt(now))
            .map_err(|_| DaemonError::PipelineControl {
                operation: "test store retry",
            })
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub(crate) fn degraded(&self) -> Result<BTreeMap<String, String>, DaemonError> {
        self.degraded
            .lock()
            .map(|degraded| degraded.clone())
            .map_err(|_| DaemonError::SynchronizationPoisoned {
                name: "pipeline degradation state",
            })
    }
}

#[cfg(test)]
struct PanickingStore;

#[cfg(test)]
impl StorePersistence for PanickingStore {
    fn persist(
        &mut self,
        _events: &[Event],
        _state: Option<&DaemonState>,
    ) -> Result<usize, String> {
        panic!("simulated pipeline writer panic");
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        io::BufWriter,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    use tempfile::NamedTempFile;
    use zanei_core::{
        config::{CaptureSource, Config},
        schema::Event,
        schema::{App, EmptyData, EventData, InputKeyData, InputKeyKind, Window},
        store::{DaemonState, StoreReader, StoreWriter},
    };

    use super::{MAX_BATCH_EVENTS, Pipeline, StoreHealth, store::StorePersistence};

    #[test]
    fn stream_pipeline_applies_source_gate_before_output() {
        let mut config = Config::default();
        config.capture.sources = vec![CaptureSource::App];
        let output = NamedTempFile::new().expect("temporary output");
        let path = output.path().to_owned();
        let file = output.reopen().expect("reopen output");
        let mut pipeline =
            Pipeline::stream(&config, Box::new(BufWriter::new(file))).expect("pipeline");

        pipeline
            .sender()
            .send(raw("app.launch", EventData::AppLaunch(EmptyData {})))
            .expect("app event");
        pipeline
            .sender()
            .send(raw("window.focus", EventData::WindowFocus(EmptyData {})))
            .expect("window event");
        pipeline.shutdown().expect("shutdown");

        let output = fs::read_to_string(path).expect("read output");
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"type\":\"app.launch\""));
    }

    #[test]
    fn filter_reload_flushes_pending_events_with_the_previous_filter() {
        let mut config = Config::default();
        config.capture.sources = vec![CaptureSource::Input];
        config
            .filter
            .exclude_apps
            .push("dev.example.App".to_owned());
        let output = NamedTempFile::new().expect("temporary output");
        let path = output.path().to_owned();
        let file = output.reopen().expect("reopen output");
        let mut pipeline =
            Pipeline::stream(&config, Box::new(BufWriter::new(file))).expect("pipeline");

        pipeline
            .sender()
            .send(raw("input.key", input_text("old")))
            .expect("pre-reload input");
        let mut replacement = config.filter.clone();
        replacement
            .exclude_apps
            .retain(|value| value != "dev.example.App");
        pipeline
            .replace_filter(&replacement)
            .expect("replace filter");
        pipeline
            .sender()
            .send(raw("input.key", input_text("new")))
            .expect("post-reload input");
        pipeline.shutdown().expect("shutdown");

        let output = fs::read_to_string(path).expect("read output");
        let events = output
            .lines()
            .map(|line| serde_json::from_str::<Event>(line).expect("stored event"))
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        let EventData::InputKey(data) = &events[0].data else {
            panic!("expected input.key");
        };
        assert_eq!(data.count, 1);
        assert_eq!(data.text.as_deref(), Some("new"));
    }

    #[test]
    fn store_pipeline_flushes_pending_events_during_shutdown() {
        let mut config = Config::default();
        config.capture.sources = vec![CaptureSource::App];
        let store = NamedTempFile::new().expect("temporary store");
        let writer = Arc::new(Mutex::new(
            StoreWriter::open(store.path()).expect("store writer"),
        ));
        let mut pipeline = Pipeline::store(&config, writer).expect("pipeline");

        pipeline
            .sender()
            .send(raw("app.launch", EventData::AppLaunch(EmptyData {})))
            .expect("app event");
        pipeline.shutdown().expect("shutdown");

        let status = StoreReader::open(store.path())
            .expect("store reader")
            .status()
            .expect("store status");
        assert_eq!(status.events_captured, 1);
    }

    #[test]
    fn heartbeat_write_failure_keeps_pipeline_alive_until_recovery() {
        let mut config = Config::default();
        config.capture.sources = vec![];
        let mock = Arc::new(Mutex::new(MockStore {
            results: VecDeque::from([Err("disk full".to_owned()), Ok(0)]),
            states: Vec::new(),
            event_batches: Vec::new(),
        }));
        let mut pipeline =
            Pipeline::store_with_persistence(&config, Box::new(MockStoreHandle(Arc::clone(&mock))))
                .expect("pipeline");
        pipeline
            .heartbeat(DaemonState {
                events_dropped: 9,
                ..DaemonState::default()
            })
            .expect("heartbeat control");

        let retry_at = wait_for_health(&pipeline, |health| match health {
            StoreHealth::Backoff { retry_at, .. } => Some(retry_at),
            StoreHealth::Healthy | StoreHealth::Recovering => None,
        });
        assert!(!pipeline.is_finished());
        pipeline
            .retry_store_at(retry_at)
            .expect("force due recovery");
        wait_for_health(&pipeline, |health| {
            matches!(health, StoreHealth::Healthy).then_some(())
        });
        pipeline.shutdown().expect("shutdown");

        let states = &mock.lock().expect("mock store").states;
        assert_eq!(states.len(), 2);
        assert_eq!(
            states[1].as_ref().map(|state| state.events_dropped),
            Some(9)
        );
    }

    #[test]
    fn failed_batch_stays_in_the_live_pipeline_and_recovers_with_latest_state() {
        let mut config = Config::default();
        config.capture.sources = vec![CaptureSource::App];
        let mock = Arc::new(Mutex::new(MockStore {
            results: VecDeque::from([Ok(0), Err("disk full".to_owned()), Ok(MAX_BATCH_EVENTS)]),
            states: Vec::new(),
            event_batches: Vec::new(),
        }));
        let mut pipeline =
            Pipeline::store_with_persistence(&config, Box::new(MockStoreHandle(Arc::clone(&mock))))
                .expect("pipeline");
        pipeline
            .heartbeat(DaemonState {
                events_dropped: 11,
                ..DaemonState::default()
            })
            .expect("initial heartbeat");
        wait_for_calls(&mock, 1);

        for _ in 0..MAX_BATCH_EVENTS {
            pipeline
                .sender()
                .send(raw("app.launch", EventData::AppLaunch(EmptyData {})))
                .expect("app event");
        }
        let retry_at = wait_for_health(&pipeline, |health| match health {
            StoreHealth::Backoff { retry_at, .. } => Some(retry_at),
            StoreHealth::Healthy | StoreHealth::Recovering => None,
        });
        assert!(!pipeline.is_finished());

        pipeline
            .retry_store_at(retry_at)
            .expect("force due recovery");
        wait_for_health(&pipeline, |health| {
            matches!(health, StoreHealth::Healthy).then_some(())
        });
        pipeline.shutdown().expect("shutdown");

        let mock = mock.lock().expect("mock store");
        assert_eq!(mock.event_batches[1].len(), MAX_BATCH_EVENTS);
        assert_eq!(mock.event_batches[2].len(), MAX_BATCH_EVENTS);
        assert_eq!(
            mock.states[2].as_ref().map(|state| state.events_dropped),
            Some(11)
        );
    }

    struct MockStore {
        results: VecDeque<Result<usize, String>>,
        states: Vec<Option<DaemonState>>,
        event_batches: Vec<Vec<Event>>,
    }

    struct MockStoreHandle(Arc<Mutex<MockStore>>);

    impl StorePersistence for MockStoreHandle {
        fn persist(
            &mut self,
            events: &[Event],
            state: Option<&DaemonState>,
        ) -> Result<usize, String> {
            let mut mock = self.0.lock().expect("mock store");
            mock.states.push(state.cloned());
            mock.event_batches.push(events.to_vec());
            mock.results.pop_front().unwrap_or(Ok(0))
        }
    }

    fn wait_for_calls(mock: &Arc<Mutex<MockStore>>, expected: usize) {
        for _ in 0..100 {
            if mock.lock().expect("mock store").states.len() >= expected {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("pipeline store did not receive {expected} calls");
    }

    fn wait_for_health<T>(pipeline: &Pipeline, select: impl Fn(StoreHealth) -> Option<T>) -> T {
        for _ in 0..100 {
            if let Some(value) = select(pipeline.store_health().expect("store health")) {
                return value;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("pipeline store health did not reach the expected state");
    }

    fn raw(event_type: &str, data: EventData) -> zanei_collector::RawEvent {
        let window = (!event_type.starts_with("app.")).then_some(Window {
            title: None,
            id: None,
        });
        zanei_collector::RawEvent {
            source: "test.collector".to_owned(),
            event_type: event_type.to_owned(),
            app: App {
                name: "Example".to_owned(),
                bundle_id: Some("dev.example.App".to_owned()),
                pid: Some(1),
            },
            window,
            element: None,
            data,
            capture_context: Default::default(),
        }
    }

    fn input_text(text: &str) -> EventData {
        EventData::InputKey(InputKeyData {
            kind: InputKeyKind::Text,
            modifiers: Vec::new(),
            count: 1,
            combo: None,
            text: Some(text.to_owned()),
            field_kind: None,
        })
    }
}
