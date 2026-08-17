//! Synchronous Secure Input queries with Carbon calls owned by the EventTap thread.

use std::{
    fmt,
    sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel},
    time::Duration,
};

const SECURE_INPUT_QUERY_TIMEOUT: Duration = Duration::from_millis(150);

type SecureInputReply = SyncSender<bool>;

#[derive(Clone)]
pub struct SecureInputProbe {
    sender: Sender<SecureInputReply>,
}

pub struct SecureInputResponder {
    receiver: Receiver<SecureInputReply>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SecureInputProbeError {
    Disconnected,
    Timeout,
}

impl fmt::Display for SecureInputProbeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => formatter.write_str("Secure Input monitor is unavailable"),
            Self::Timeout => formatter.write_str("Secure Input query timed out"),
        }
    }
}

#[must_use]
pub fn secure_input_channel() -> (SecureInputProbe, SecureInputResponder) {
    let (sender, receiver) = channel();
    (
        SecureInputProbe { sender },
        SecureInputResponder { receiver },
    )
}

impl SecureInputProbe {
    pub(crate) fn enabled(&self) -> Result<bool, SecureInputProbeError> {
        let (reply, response) = sync_channel(1);
        self.sender
            .send(reply)
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

impl SecureInputResponder {
    pub(crate) fn take_pending(&self) -> Vec<SecureInputReply> {
        self.receiver.try_iter().collect()
    }

    pub(crate) fn respond(replies: Vec<SecureInputReply>, enabled: bool) {
        for reply in replies {
            let _ = reply.try_send(enabled);
        }
    }

    #[cfg(test)]
    pub(crate) fn respond_next(&self, enabled: bool) {
        let reply = self
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("Secure Input probe should send a query");
        reply
            .send(enabled)
            .expect("Secure Input probe should await its response");
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::{SecureInputProbeError, secure_input_channel};

    #[test]
    fn probe_returns_the_eventtap_owned_secure_input_state() {
        let (probe, responder) = secure_input_channel();
        let query = thread::spawn(move || probe.enabled());
        responder.respond_next(true);

        assert_eq!(query.join().expect("probe thread"), Ok(true));
    }

    #[test]
    fn disconnected_responder_fails_closed() {
        let (probe, responder) = secure_input_channel();
        drop(responder);

        assert_eq!(probe.enabled(), Err(SecureInputProbeError::Disconnected));
    }
}
