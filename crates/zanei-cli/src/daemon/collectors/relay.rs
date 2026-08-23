use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use zanei_collector::{COLLECTOR_CHANNEL_CAPACITY, RawEvent};

use crate::daemon::DaemonError;

const FORWARD_RETRY_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::daemon) enum RelayExit {
    SourceClosed,
    PipelineClosed,
    Stopped { dropped: u64 },
}

pub(in crate::daemon) struct Relay {
    handle: Option<JoinHandle<RelayExit>>,
    stop: Arc<AtomicBool>,
}

impl Relay {
    pub(in crate::daemon) fn spawn(
        name: &str,
        destination: SyncSender<RawEvent>,
    ) -> Result<(Self, SyncSender<RawEvent>), DaemonError> {
        let (sender, receiver) = sync_channel(COLLECTOR_CHANNEL_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread_name = format!("zanei-{name}-relay");
        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || forward(receiver, destination, &worker_stop))
            .map_err(|source| DaemonError::ThreadSpawn {
                thread: "collector relay",
                source,
            })?;
        Ok((
            Self {
                handle: Some(handle),
                stop,
            },
            sender,
        ))
    }

    pub(in crate::daemon) fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(in crate::daemon) fn join(&mut self) -> Result<RelayExit, DaemonError> {
        self.join_handle()
    }

    pub(in crate::daemon) fn stop(&mut self) -> Result<u64, DaemonError> {
        self.stop.store(true, Ordering::Release);
        match self.join_handle()? {
            RelayExit::Stopped { dropped } => Ok(dropped),
            RelayExit::SourceClosed | RelayExit::PipelineClosed => Ok(0),
        }
    }

    fn join_handle(&mut self) -> Result<RelayExit, DaemonError> {
        self.handle
            .take()
            .ok_or(DaemonError::ThreadTerminated {
                thread: "collector relay",
            })?
            .join()
            .map_err(|_| DaemonError::ThreadTerminated {
                thread: "collector relay",
            })
    }
}

fn forward(
    receiver: Receiver<RawEvent>,
    destination: SyncSender<RawEvent>,
    stop: &AtomicBool,
) -> RelayExit {
    let mut pending = None;
    loop {
        if stop.load(Ordering::Acquire) {
            return RelayExit::Stopped {
                dropped: pending_count(pending.is_some(), &receiver),
            };
        }
        let event = match pending.take() {
            Some(event) => event,
            None => match receiver.recv_timeout(FORWARD_RETRY_INTERVAL) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return RelayExit::SourceClosed,
            },
        };
        match destination.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(event)) => {
                pending = Some(event);
                thread::sleep(FORWARD_RETRY_INTERVAL);
            }
            Err(TrySendError::Disconnected(_)) => return RelayExit::PipelineClosed,
        }
    }
}

fn pending_count(has_pending: bool, receiver: &Receiver<RawEvent>) -> u64 {
    let queued = u64::try_from(receiver.try_iter().count()).unwrap_or(u64::MAX);
    queued.saturating_add(u64::from(has_pending))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use zanei_core::schema::{App, EmptyData, EventData};

    use super::{Relay, RelayExit};

    #[test]
    fn source_disconnect_finishes_after_forwarding_pending_events() {
        let (destination, received) = mpsc::sync_channel(4);
        let (mut relay, source) = Relay::spawn("test", destination).expect("relay");
        source.send(raw()).expect("source event");
        drop(source);

        assert_eq!(
            received.recv().expect("forwarded event").event_type,
            "app.launch"
        );
        assert_eq!(relay.join().expect("relay exit"), RelayExit::SourceClosed);
    }

    #[test]
    fn pipeline_disconnect_is_distinct_from_collector_exit() {
        let (destination, received) = mpsc::sync_channel(4);
        drop(received);
        let (mut relay, source) = Relay::spawn("test", destination).expect("relay");
        source.send(raw()).expect("source event");

        assert_eq!(relay.join().expect("relay exit"), RelayExit::PipelineClosed);
    }

    #[test]
    fn forced_stop_counts_events_that_never_entered_the_pipeline() {
        let (destination, _received) = mpsc::sync_channel(0);
        let (mut relay, source) = Relay::spawn("test", destination).expect("relay");
        source.send(raw()).expect("source event");

        assert_eq!(relay.stop().expect("stop relay"), 1);
    }

    fn raw() -> zanei_collector::RawEvent {
        zanei_collector::RawEvent {
            source: "test.collector".to_owned(),
            event_type: "app.launch".to_owned(),
            app: App {
                name: "Example".to_owned(),
                bundle_id: None,
                pid: None,
            },
            window: None,
            element: None,
            data: EventData::AppLaunch(EmptyData {}),
            capture_context: Default::default(),
        }
    }
}
