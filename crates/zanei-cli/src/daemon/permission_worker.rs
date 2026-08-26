use std::{
    collections::BTreeSet,
    sync::mpsc::{Receiver, TryRecvError, sync_channel},
    thread,
};

use zanei_collector::Capability;
use zanei_macos::permission::PermissionError;

use super::DaemonError;
use crate::permissions::{PermissionRequestOutcome, request_missing_permissions};

const PERMISSION_REQUEST_THREAD: &str = "permission-request";
const PERMISSION_REQUEST_THREAD_NAME: &str = "zanei-permission-request";
pub(super) enum PermissionRequestPoll {
    Pending,
    Complete(Result<PermissionRequestOutcome, PermissionError>),
    Stopped,
}

pub(super) struct PermissionRequestWorker {
    result: Receiver<Result<PermissionRequestOutcome, PermissionError>>,
}

impl PermissionRequestWorker {
    pub(super) fn start(required: BTreeSet<Capability>) -> Result<Self, DaemonError> {
        Self::start_with(move || request_missing_permissions(&required))
    }

    pub(super) fn start_with(
        request: impl FnOnce() -> Result<PermissionRequestOutcome, PermissionError> + Send + 'static,
    ) -> Result<Self, DaemonError> {
        let (result_sender, result) = sync_channel(1);
        drop(
            thread::Builder::new()
                .name(PERMISSION_REQUEST_THREAD_NAME.to_owned())
                .spawn(move || {
                    let _ = result_sender.send(request());
                })
                .map_err(|source| DaemonError::ThreadSpawn {
                    thread: PERMISSION_REQUEST_THREAD,
                    source,
                })?,
        );
        Ok(Self { result })
    }

    pub(super) fn poll(&self) -> PermissionRequestPoll {
        match self.result.try_recv() {
            Ok(result) => PermissionRequestPoll::Complete(result),
            Err(TryRecvError::Empty) => PermissionRequestPoll::Pending,
            Err(TryRecvError::Disconnected) => PermissionRequestPoll::Stopped,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use zanei_macos::permission::PermissionError;

    use super::{PermissionRequestOutcome, PermissionRequestPoll, PermissionRequestWorker};

    #[test]
    fn request_runs_on_a_detached_worker_and_reports_completion() {
        let (started_sender, started_receiver) = sync_channel(1);
        let (release_sender, release_receiver) = sync_channel(1);
        let worker = PermissionRequestWorker::start_with(move || {
            started_sender.send(()).expect("test should observe worker");
            release_receiver.recv().expect("test should release worker");
            Ok(PermissionRequestOutcome::Completed)
        })
        .expect("permission request worker");

        started_receiver.recv().expect("worker should start");
        assert!(matches!(worker.poll(), PermissionRequestPoll::Pending));

        release_sender.send(()).expect("release worker");
        loop {
            match worker.poll() {
                PermissionRequestPoll::Pending => std::thread::yield_now(),
                PermissionRequestPoll::Complete(result) => {
                    assert!(matches!(result, Ok(PermissionRequestOutcome::Completed)));
                    break;
                }
                PermissionRequestPoll::Stopped => panic!("worker must report its result"),
            }
        }
    }

    #[test]
    fn request_error_remains_typed_across_the_worker_boundary() {
        let worker = PermissionRequestWorker::start_with(|| {
            Err(PermissionError::AccessibilityRequestOptionsCreation)
        })
        .expect("permission request worker");

        loop {
            match worker.poll() {
                PermissionRequestPoll::Pending => std::thread::yield_now(),
                PermissionRequestPoll::Complete(result) => {
                    assert!(matches!(
                        result,
                        Err(PermissionError::AccessibilityRequestOptionsCreation)
                    ));
                    break;
                }
                PermissionRequestPoll::Stopped => panic!("worker must report its error"),
            }
        }
    }

    #[test]
    fn worker_panic_is_reported_as_a_stopped_worker() {
        let worker = PermissionRequestWorker::start_with(|| {
            panic!("simulated permission worker panic");
        })
        .expect("permission request worker");

        loop {
            match worker.poll() {
                PermissionRequestPoll::Pending => std::thread::yield_now(),
                PermissionRequestPoll::Stopped => break,
                PermissionRequestPoll::Complete(_) => panic!("panicked worker has no result"),
            }
        }
    }
}
