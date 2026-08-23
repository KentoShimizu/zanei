//! Single-owner Secure Input monitor and synchronous fail-closed probes.

use std::{
    fmt,
    sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel},
    thread::{self, JoinHandle},
    time::Duration,
};

const SECURE_INPUT_QUERY_TIMEOUT: Duration = Duration::from_millis(150);

type SecureInputReply = SyncSender<bool>;

enum MonitorRequest {
    Query(SecureInputReply),
    Stop,
}

#[derive(Clone)]
pub struct SecureInputProbe {
    sender: Sender<MonitorRequest>,
}

pub struct SecureInputMonitor {
    sender: Sender<MonitorRequest>,
    worker: Option<JoinHandle<()>>,
}

#[cfg(test)]
pub(crate) struct SecureInputTestResponder {
    receiver: Receiver<MonitorRequest>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SecureInputProbeError {
    Disconnected,
    Timeout,
}

#[derive(Debug)]
pub struct SecureInputMonitorError {
    source: std::io::Error,
}

impl fmt::Display for SecureInputProbeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => formatter.write_str("Secure Input monitor is unavailable"),
            Self::Timeout => formatter.write_str("Secure Input query timed out"),
        }
    }
}

impl fmt::Display for SecureInputMonitorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to start the Secure Input monitor: {}",
            self.source
        )
    }
}

impl std::error::Error for SecureInputMonitorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl SecureInputMonitor {
    pub fn start() -> Result<(Self, SecureInputProbe), SecureInputMonitorError> {
        Self::start_with(crate::ffi::secure_input::enabled)
    }

    fn start_with(
        read_enabled: impl Fn() -> bool + Send + 'static,
    ) -> Result<(Self, SecureInputProbe), SecureInputMonitorError> {
        let (sender, receiver) = channel();
        let worker = thread::Builder::new()
            .name("zanei-secure-input".to_owned())
            .spawn(move || run_monitor(receiver, read_enabled))
            .map_err(|source| SecureInputMonitorError { source })?;
        Ok((
            Self {
                sender: sender.clone(),
                worker: Some(worker),
            },
            SecureInputProbe { sender },
        ))
    }
}

impl Drop for SecureInputMonitor {
    fn drop(&mut self) {
        let _ = self.sender.send(MonitorRequest::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl SecureInputProbe {
    pub(crate) fn enabled(&self) -> Result<bool, SecureInputProbeError> {
        let (reply, response) = sync_channel(1);
        self.sender
            .send(MonitorRequest::Query(reply))
            .map_err(|_| SecureInputProbeError::Disconnected)?;
        response
            .recv_timeout(SECURE_INPUT_QUERY_TIMEOUT)
            .map_err(|error| match error {
                std::sync::mpsc::RecvTimeoutError::Timeout => SecureInputProbeError::Timeout,
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    SecureInputProbeError::Disconnected
                }
            })
    }
}

#[cfg(test)]
pub(crate) fn secure_input_test_channel() -> (SecureInputProbe, SecureInputTestResponder) {
    let (sender, receiver) = channel();
    (
        SecureInputProbe { sender },
        SecureInputTestResponder { receiver },
    )
}

#[cfg(test)]
impl SecureInputTestResponder {
    pub(crate) fn respond_next(&self, enabled: bool) {
        let MonitorRequest::Query(reply) = self
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("Secure Input probe should send a query")
        else {
            panic!("test responder received a stop request");
        };
        reply
            .send(enabled)
            .expect("Secure Input probe should await its response");
    }
}

fn run_monitor(receiver: Receiver<MonitorRequest>, read_enabled: impl Fn() -> bool) {
    while let Ok(request) = receiver.recv() {
        match request {
            MonitorRequest::Query(reply) => {
                let _ = reply.try_send(read_enabled());
            }
            MonitorRequest::Stop => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn all_queries_are_served_by_the_single_owner_thread() {
        let thread_ids = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&thread_ids);
        let (monitor, probe) = SecureInputMonitor::start_with(move || {
            observed
                .lock()
                .expect("thread id observations")
                .push(thread::current().id());
            true
        })
        .expect("Secure Input monitor");

        assert_eq!(probe.enabled(), Ok(true));
        assert_eq!(probe.enabled(), Ok(true));
        drop(monitor);

        let ids = thread_ids.lock().expect("thread id observations");
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], ids[1]);
    }

    #[test]
    fn disconnected_monitor_fails_closed() {
        let (sender, receiver) = channel();
        drop(receiver);
        let probe = SecureInputProbe { sender };

        assert_eq!(probe.enabled(), Err(SecureInputProbeError::Disconnected));
    }

    #[test]
    fn unresponsive_monitor_times_out_and_fails_closed() {
        let (sender, receiver) = channel();
        let probe = SecureInputProbe { sender };

        assert_eq!(probe.enabled(), Err(SecureInputProbeError::Timeout));
        drop(receiver);
    }
}
