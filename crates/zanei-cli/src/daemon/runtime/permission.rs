//! Permission worker completion and EventTap startup gating.

use std::collections::{BTreeMap, BTreeSet};

use zanei_collector::Capability;
use zanei_macos::permission::{PermissionError, PermissionStatus};

use super::{
    PERMISSION_REQUEST_TIMEOUT_MESSAGE, PERMISSION_REQUEST_WORKER_STOPPED_MESSAGE,
    PermissionRequestOutcome, PermissionRequestPoll, PermissionRequestWorker,
};

pub(in crate::daemon) fn configure_eventtap_start_gate(
    status: Option<Result<PermissionStatus, PermissionError>>,
    gate: &mut super::super::supervisor::EventTapStartGate,
    degraded: &mut BTreeMap<String, String>,
) {
    let Some(status) = status else {
        return;
    };
    if !matches!(status, Ok(PermissionStatus::Granted)) {
        gate.defer();
    }
    if let Err(error) = status {
        degraded.insert("permissions".to_owned(), error.to_string());
    }
}

pub(in crate::daemon) fn service_permission_request_worker(
    worker: &mut Option<PermissionRequestWorker>,
    degraded: &mut BTreeMap<String, String>,
    start_now: bool,
    on_complete: impl FnOnce(bool),
) {
    let Some(active_worker) = worker.as_ref() else {
        return;
    };
    let result = match active_worker.poll() {
        PermissionRequestPoll::Pending => return,
        PermissionRequestPoll::Complete(result) => result,
        PermissionRequestPoll::Stopped => {
            *worker = None;
            degraded.insert(
                "permission_request".to_owned(),
                PERMISSION_REQUEST_WORKER_STOPPED_MESSAGE.to_owned(),
            );
            on_complete(start_now);
            return;
        }
    };
    *worker = None;
    match result {
        Ok(PermissionRequestOutcome::Completed) => {
            degraded.remove("permission_request");
        }
        Ok(PermissionRequestOutcome::TimedOut) => {
            degraded.insert(
                "permission_request".to_owned(),
                PERMISSION_REQUEST_TIMEOUT_MESSAGE.to_owned(),
            );
        }
        Err(error) => {
            degraded.insert("permission_request".to_owned(), error.to_string());
        }
    }
    on_complete(start_now);
}

pub(in crate::daemon) fn queue_permission_expansion(
    previous: &BTreeSet<Capability>,
    current: &BTreeSet<Capability>,
    pending: &mut Option<BTreeSet<Capability>>,
) {
    if !current.is_subset(previous) {
        *pending = Some(current.clone());
    } else if current != previous {
        *pending = None;
    }
}
