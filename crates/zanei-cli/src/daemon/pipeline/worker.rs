use std::{
    collections::BTreeMap,
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use zanei_collector::RawEvent;
use zanei_core::{
    normalize::Normalizer,
    privacy::PrivacyFilter,
    schema::Event,
    sink::{Sink, StreamSink},
    store::DaemonState,
};

use super::store::{StoreDestination, StoreHealth};
use crate::daemon::{DaemonError, collectors::SourceGate};

pub(super) const MAX_BATCH_EVENTS: usize = 512;
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(super) enum Control {
    Flush(SyncSender<()>),
    FlushAndReplaceFilter {
        filter: PrivacyFilter,
        acknowledge: SyncSender<()>,
    },
    Heartbeat(DaemonState),
    #[cfg(test)]
    RetryAt(Instant),
    Shutdown,
}

pub(super) enum Destination {
    Store(Box<StoreDestination>),
    Stream(StreamSink<Box<dyn Write + Send>>),
}

pub(super) struct Worker {
    raw_receiver: Receiver<RawEvent>,
    control_receiver: Receiver<Control>,
    gate: SourceGate,
    filter: PrivacyFilter,
    normalizer: Normalizer,
    destination: Destination,
    batch_interval: Duration,
    last_flush: Instant,
    dropped: Arc<AtomicU64>,
    degraded: Arc<Mutex<BTreeMap<String, String>>>,
    flush_waiters: Vec<SyncSender<()>>,
    shutdown_requested: bool,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        raw_receiver: Receiver<RawEvent>,
        control_receiver: Receiver<Control>,
        gate: SourceGate,
        filter: PrivacyFilter,
        destination: Destination,
        batch_interval: Duration,
        dropped: Arc<AtomicU64>,
        degraded: Arc<Mutex<BTreeMap<String, String>>>,
    ) -> Self {
        Self {
            raw_receiver,
            control_receiver,
            gate,
            filter,
            normalizer: Normalizer::new(),
            destination,
            batch_interval,
            last_flush: Instant::now(),
            dropped,
            degraded,
            flush_waiters: Vec::new(),
            shutdown_requested: false,
        }
    }

    pub(super) fn run(mut self) -> Result<(), DaemonError> {
        loop {
            while let Ok(control) = self.control_receiver.try_recv() {
                self.handle_control(control)?;
            }
            self.destination.retry_if_due(Instant::now());
            self.sync_store_degraded()?;
            if self.destination.accepts_intake() {
                for acknowledge in self.flush_waiters.drain(..) {
                    let _ = acknowledge.send(());
                }
                if self.shutdown_requested {
                    return Ok(());
                }
            } else {
                thread::sleep(CONTROL_POLL_INTERVAL);
                continue;
            }

            let until_flush = self
                .batch_interval
                .saturating_sub(self.last_flush.elapsed());
            let wait = until_flush.min(CONTROL_POLL_INTERVAL);
            match self.raw_receiver.recv_timeout(wait) {
                Ok(raw) => self.process(raw)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.flush_all()?;
                    return Ok(());
                }
            }
            if self.last_flush.elapsed() >= self.batch_interval {
                self.flush_all()?;
            }
        }
    }

    fn handle_control(&mut self, control: Control) -> Result<(), DaemonError> {
        match control {
            Control::Flush(acknowledge) => {
                self.drain_raw()?;
                self.flush_all()?;
                if self.destination.accepts_intake() {
                    let _ = acknowledge.send(());
                } else {
                    self.flush_waiters.push(acknowledge);
                }
            }
            Control::FlushAndReplaceFilter {
                filter,
                acknowledge,
            } => {
                self.drain_raw()?;
                self.flush_all()?;
                self.filter = filter;
                let _ = acknowledge.send(());
            }
            Control::Heartbeat(state) => {
                self.destination.heartbeat(state, Instant::now());
                self.sync_store_degraded()?;
            }
            #[cfg(test)]
            Control::RetryAt(now) => {
                self.destination.retry_if_due(now);
                self.sync_store_degraded()?;
            }
            Control::Shutdown => {
                self.drain_raw()?;
                self.flush_all()?;
                self.shutdown_requested = true;
            }
        }
        Ok(())
    }

    fn drain_raw(&mut self) -> Result<(), DaemonError> {
        while let Ok(raw) = self.raw_receiver.try_recv() {
            self.process(raw)?;
        }
        Ok(())
    }

    fn process(&mut self, raw: RawEvent) -> Result<(), DaemonError> {
        if !self.gate.allows(&raw) {
            return Ok(());
        }
        match self.normalizer.push(raw) {
            Ok(events) => {
                self.clear_degraded("pipeline")?;
                self.write_events(events)
            }
            Err(error) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.set_degraded("pipeline", error.to_string())
            }
        }
    }

    fn write_events(&mut self, events: Vec<Event>) -> Result<(), DaemonError> {
        for event in events {
            let event = self.filter.process(event);
            if let Some(event) = event {
                self.destination.write(event)?;
            }
        }
        if self.destination.batch_len() >= MAX_BATCH_EVENTS || self.destination.byte_limit_reached()
        {
            self.destination.flush(Instant::now())?;
            self.sync_store_degraded()?;
            self.last_flush = Instant::now();
        }
        Ok(())
    }

    fn flush_all(&mut self) -> Result<(), DaemonError> {
        let pending = self.normalizer.flush();
        self.write_events(pending)?;
        self.destination.flush(Instant::now())?;
        self.sync_store_degraded()?;
        self.last_flush = Instant::now();
        Ok(())
    }

    fn set_degraded(&self, key: &str, message: String) -> Result<(), DaemonError> {
        self.degraded
            .lock()
            .map_err(|_| DaemonError::SynchronizationPoisoned {
                name: "pipeline degradation state",
            })?
            .insert(key.to_owned(), message);
        Ok(())
    }

    fn clear_degraded(&self, key: &str) -> Result<(), DaemonError> {
        self.degraded
            .lock()
            .map_err(|_| DaemonError::SynchronizationPoisoned {
                name: "pipeline degradation state",
            })?
            .remove(key);
        Ok(())
    }

    fn sync_store_degraded(&self) -> Result<(), DaemonError> {
        let mut degraded =
            self.degraded
                .lock()
                .map_err(|_| DaemonError::SynchronizationPoisoned {
                    name: "pipeline degradation state",
                })?;
        match self.destination.store_health() {
            Some(StoreHealth::Backoff { error, .. }) => {
                degraded.insert("store".to_owned(), error);
            }
            Some(StoreHealth::Recovering) => {
                degraded.insert("store".to_owned(), "recovering store writes".to_owned());
            }
            Some(StoreHealth::Healthy) | None => {
                degraded.remove("store");
            }
        }
        Ok(())
    }
}

impl Destination {
    fn write(&mut self, event: Event) -> Result<(), DaemonError> {
        match self {
            Self::Store(store) => store.write(event).map(|_| ()),
            Self::Stream(sink) => {
                sink.write(&event)?;
                sink.flush().map_err(DaemonError::from)
            }
        }
    }

    fn batch_len(&self) -> usize {
        match self {
            Self::Store(store) => store.batch_len(),
            Self::Stream(_) => 0,
        }
    }

    fn byte_limit_reached(&self) -> bool {
        match self {
            Self::Store(store) => store.byte_limit_reached(),
            Self::Stream(_) => false,
        }
    }

    fn flush(&mut self, now: Instant) -> Result<(), DaemonError> {
        match self {
            Self::Stream(sink) => sink.flush().map_err(DaemonError::from),
            Self::Store(store) => {
                store.flush(now);
                Ok(())
            }
        }
    }

    fn heartbeat(&mut self, state: DaemonState, now: Instant) {
        if let Self::Store(store) = self {
            store.heartbeat(state, now);
        }
    }

    fn retry_if_due(&mut self, now: Instant) {
        if let Self::Store(store) = self {
            store.retry_if_due(now);
        }
    }

    fn accepts_intake(&self) -> bool {
        match self {
            Self::Store(store) => store.accepts_intake(),
            Self::Stream(_) => true,
        }
    }

    fn store_health(&self) -> Option<StoreHealth> {
        match self {
            Self::Store(store) => store.health(),
            Self::Stream(_) => None,
        }
    }
}
