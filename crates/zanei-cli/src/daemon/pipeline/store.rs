use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use zanei_core::{schema::Event, store::DaemonState};

use super::super::DaemonError;
use super::SharedStoreWriter;

/// A batch is flushed before its serialized representation can grow beyond
/// 4 MiB, bounding retained memory while the SQLite store is unavailable.
pub(crate) const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;

const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
    Duration::from_secs(40),
    Duration::from_secs(60),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StoreHealth {
    Healthy,
    Backoff { retry_at: Instant, error: String },
    Recovering,
}

impl StoreHealth {
    pub(crate) const fn accepts_intake(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

pub(super) trait StorePersistence: Send {
    fn persist(&mut self, events: &[Event], state: Option<&DaemonState>) -> Result<usize, String>;
}

struct LockedStore(SharedStoreWriter);

impl StorePersistence for LockedStore {
    fn persist(&mut self, events: &[Event], state: Option<&DaemonState>) -> Result<usize, String> {
        self.0
            .lock()
            .map_err(|_| "store writer synchronization primitive was poisoned".to_owned())?
            .persist(events, state)
            .map_err(|error| error.to_string())
    }
}

pub(super) struct StoreDestination {
    writer: Box<dyn StorePersistence>,
    batch: Vec<Event>,
    batch_bytes: usize,
    latest_state: Option<DaemonState>,
    state_dirty: bool,
    health: Arc<Mutex<StoreHealth>>,
    retry_index: usize,
}

impl StoreDestination {
    pub(super) fn new(writer: SharedStoreWriter, health: Arc<Mutex<StoreHealth>>) -> Self {
        Self::with_writer(Box::new(LockedStore(writer)), health)
    }

    pub(super) fn with_writer(
        writer: Box<dyn StorePersistence>,
        health: Arc<Mutex<StoreHealth>>,
    ) -> Self {
        Self {
            writer,
            batch: Vec::new(),
            batch_bytes: 0,
            latest_state: None,
            state_dirty: false,
            health,
            retry_index: 0,
        }
    }

    pub(super) fn write(&mut self, event: Event) -> Result<bool, DaemonError> {
        let event_bytes = serde_json::to_vec(&event)
            .map_err(|error| {
                DaemonError::Store(zanei_core::store::StoreError::InvalidJson {
                    field: "pipeline batch event",
                    source: error,
                })
            })?
            .len();
        self.batch_bytes = self.batch_bytes.saturating_add(event_bytes);
        self.batch.push(event);
        Ok(self.batch_bytes >= MAX_BATCH_BYTES)
    }

    pub(super) fn batch_len(&self) -> usize {
        self.batch.len()
    }

    pub(super) const fn byte_limit_reached(&self) -> bool {
        self.batch_bytes >= MAX_BATCH_BYTES
    }

    pub(super) fn heartbeat(&mut self, state: DaemonState, now: Instant) {
        self.latest_state = Some(state);
        self.state_dirty = true;
        if self.accepts_intake() {
            self.persist(now);
        }
    }

    pub(super) fn flush(&mut self, now: Instant) {
        if self.accepts_intake() {
            self.persist(now);
        }
    }

    pub(super) fn retry_if_due(&mut self, now: Instant) {
        let due = self.health_snapshot().is_some_and(
            |health| matches!(health, StoreHealth::Backoff { retry_at, .. } if now >= retry_at),
        );
        if due {
            self.set_health(StoreHealth::Recovering);
            self.persist(now);
        }
    }

    pub(super) fn accepts_intake(&self) -> bool {
        self.health_snapshot()
            .is_some_and(|health| health.accepts_intake())
    }

    pub(super) fn health(&self) -> Option<StoreHealth> {
        self.health_snapshot()
    }

    pub(super) fn has_pending_write(&self) -> bool {
        !self.batch.is_empty() || self.state_dirty
    }

    fn persist(&mut self, now: Instant) {
        if !self.has_pending_write() {
            self.set_health(StoreHealth::Healthy);
            return;
        }
        let state = self
            .state_dirty
            .then_some(self.latest_state.as_ref())
            .flatten();
        match self.writer.persist(&self.batch, state) {
            Ok(_) => {
                self.batch.clear();
                self.batch_bytes = 0;
                self.state_dirty = false;
                self.retry_index = 0;
                self.set_health(StoreHealth::Healthy);
            }
            Err(error) => self.enter_backoff(now, error),
        }
    }

    fn enter_backoff(&mut self, now: Instant, error: String) {
        // Recovery binds retained events to the latest daemon snapshot atomically,
        // including when the failed operation was an event-only batch flush.
        self.state_dirty = self.latest_state.is_some();
        let delay = RETRY_DELAYS[self.retry_index.min(RETRY_DELAYS.len() - 1)];
        self.retry_index = self.retry_index.saturating_add(1);
        let retry_at = now + delay;
        eprintln!(
            "zanei: store write failed: {error}; retaining data and retrying in {} seconds",
            delay.as_secs()
        );
        self.set_health(StoreHealth::Backoff { retry_at, error });
    }

    fn health_snapshot(&self) -> Option<StoreHealth> {
        self.health.lock().ok().map(|health| health.clone())
    }

    fn set_health(&self, next: StoreHealth) {
        if let Ok(mut health) = self.health.lock() {
            *health = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use zanei_core::{
        schema::{App, EmptyData, Event, EventData, Redaction},
        store::DaemonState,
    };

    use super::{MAX_BATCH_BYTES, StoreDestination, StoreHealth, StorePersistence};

    #[derive(Default)]
    struct MockState {
        results: VecDeque<Result<usize, String>>,
        calls: Vec<(Vec<Event>, Option<DaemonState>)>,
    }

    struct MockWriter(Arc<Mutex<MockState>>);

    impl StorePersistence for MockWriter {
        fn persist(
            &mut self,
            events: &[Event],
            state: Option<&DaemonState>,
        ) -> Result<usize, String> {
            let mut mock = self.0.lock().expect("mock writer");
            mock.calls.push((events.to_vec(), state.cloned()));
            mock.results.pop_front().unwrap_or(Ok(events.len()))
        }
    }

    #[test]
    fn failed_write_is_retained_and_recovered_with_heartbeat() {
        let mock = Arc::new(Mutex::new(MockState {
            results: VecDeque::from([Err("disk full".to_owned()), Ok(1)]),
            calls: Vec::new(),
        }));
        let health = Arc::new(Mutex::new(StoreHealth::Healthy));
        let mut destination = StoreDestination::with_writer(
            Box::new(MockWriter(Arc::clone(&mock))),
            Arc::clone(&health),
        );
        let now = Instant::now();
        destination.write(event("one")).expect("buffer event");
        destination.heartbeat(
            DaemonState {
                events_dropped: 7,
                ..DaemonState::default()
            },
            now,
        );

        assert!(!destination.accepts_intake());
        assert_eq!(destination.batch_len(), 1);
        destination.retry_if_due(now + Duration::from_secs(5));

        assert!(destination.accepts_intake());
        assert_eq!(destination.batch_len(), 0);
        let calls = &mock.lock().expect("mock state").calls;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0.len(), 1);
        assert_eq!(
            calls[1].1.as_ref().map(|state| state.events_dropped),
            Some(7)
        );
    }

    #[test]
    fn event_only_flush_does_not_rewrite_a_clean_heartbeat_snapshot() {
        let mock = Arc::new(Mutex::new(MockState::default()));
        let health = Arc::new(Mutex::new(StoreHealth::Healthy));
        let mut destination =
            StoreDestination::with_writer(Box::new(MockWriter(Arc::clone(&mock))), health);
        let now = Instant::now();
        destination.heartbeat(DaemonState::default(), now);
        destination
            .write(event("event-only"))
            .expect("buffer event");
        destination.flush(now);

        let calls = &mock.lock().expect("mock state").calls;
        assert!(calls[0].1.is_some());
        assert!(calls[1].1.is_none());
    }

    #[test]
    fn failed_event_only_flush_recovers_with_the_latest_snapshot() {
        let mock = Arc::new(Mutex::new(MockState {
            results: VecDeque::from([Ok(0), Err("disk full".to_owned()), Ok(1)]),
            calls: Vec::new(),
        }));
        let health = Arc::new(Mutex::new(StoreHealth::Healthy));
        let mut destination =
            StoreDestination::with_writer(Box::new(MockWriter(Arc::clone(&mock))), health);
        let now = Instant::now();
        destination.heartbeat(DaemonState::default(), now);
        destination.write(event("retained")).expect("buffer event");
        destination.flush(now);
        destination.retry_if_due(now + Duration::from_secs(5));

        let calls = &mock.lock().expect("mock state").calls;
        assert_eq!(calls[2].0.len(), 1);
        assert!(calls[2].1.is_some());
    }

    #[test]
    fn serialized_byte_limit_requests_flush() {
        let mock = Arc::new(Mutex::new(MockState::default()));
        let health = Arc::new(Mutex::new(StoreHealth::Healthy));
        let mut destination = StoreDestination::with_writer(Box::new(MockWriter(mock)), health);
        destination.batch_bytes = MAX_BATCH_BYTES - 1;

        assert!(
            destination
                .write(event("byte limit"))
                .expect("buffer event")
        );
    }

    #[test]
    fn write_backoff_grows_to_sixty_seconds_and_caps() {
        let mock = Arc::new(Mutex::new(MockState {
            results: VecDeque::from([
                Err("full-1".to_owned()),
                Err("full-2".to_owned()),
                Err("full-3".to_owned()),
                Err("full-4".to_owned()),
                Err("full-5".to_owned()),
                Err("full-6".to_owned()),
            ]),
            calls: Vec::new(),
        }));
        let health = Arc::new(Mutex::new(StoreHealth::Healthy));
        let mut destination =
            StoreDestination::with_writer(Box::new(MockWriter(mock)), Arc::clone(&health));
        let mut attempt_at = Instant::now();
        destination.heartbeat(DaemonState::default(), attempt_at);

        let expected_delays = [5, 10, 20, 40, 60, 60];
        for (index, expected_delay) in expected_delays.into_iter().enumerate() {
            let retry_at = match health.lock().expect("store health").clone() {
                StoreHealth::Backoff { retry_at, .. } => retry_at,
                state => panic!("expected backoff, got {state:?}"),
            };
            assert_eq!(
                retry_at.duration_since(attempt_at).as_secs(),
                expected_delay
            );
            attempt_at = retry_at;
            if index + 1 < expected_delays.len() {
                destination.retry_if_due(attempt_at);
            }
        }
    }

    fn event(_label: &str) -> Event {
        Event {
            version: 1,
            id: "evt_01J00000000000000000000000".to_owned(),
            ts: "2026-08-17T00:00:00Z".to_owned(),
            mono_ns: 1,
            source: "test.pipeline".to_owned(),
            event_type: "app.launch".to_owned(),
            app: App {
                name: "Example".to_owned(),
                bundle_id: Some("dev.example.App".to_owned()),
                pid: Some(1),
            },
            window: None,
            element: None,
            data: EventData::AppLaunch(EmptyData {}),
            redaction: Redaction {
                applied: false,
                rules: Vec::new(),
            },
        }
    }
}
