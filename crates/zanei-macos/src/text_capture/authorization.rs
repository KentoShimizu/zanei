//! Time-window key-input authorization shared by EventTap and AX.

use std::{
    collections::VecDeque,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    time::{Duration, Instant},
};

use crate::trace;

pub(crate) const INPUT_WINDOW: Duration = Duration::from_secs(3);
pub(crate) const AUTHORIZATION_QUEUE_CAPACITY: usize = 1_024;

#[derive(Clone)]
pub(crate) struct InputAuthorization {
    sequence: u64,
    pid: i32,
    target_generation: u64,
    input_at: Instant,
    state: Arc<AtomicU8>,
}

impl InputAuthorization {
    fn pending(sequence: u64, pid: i32, target_generation: u64, input_at: Instant) -> Self {
        Self {
            sequence,
            pid,
            target_generation,
            input_at,
            state: Arc::new(AtomicU8::new(AuthorizationState::Pending as u8)),
        }
    }

    pub(crate) fn confirm(&self) {
        let transitioned = self
            .state
            .compare_exchange(
                AuthorizationState::Pending as u8,
                AuthorizationState::Confirmed as u8,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_ok();
        trace::trace!(
            "component=authorization event=transition authorization_id={} pid={} target_generation={} authorization={} reason=worker_sent transitioned={}",
            self.sequence,
            self.pid,
            self.target_generation,
            if transitioned {
                "confirmed"
            } else {
                self.state_name()
            },
            transitioned
        );
    }

    pub(crate) fn reject(&self) {
        let transitioned = self
            .state
            .compare_exchange(
                AuthorizationState::Pending as u8,
                AuthorizationState::Rejected as u8,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_ok();
        trace::trace!(
            "component=authorization event=transition authorization_id={} pid={} target_generation={} authorization={} reason=worker_dropped transitioned={}",
            self.sequence,
            self.pid,
            self.target_generation,
            if transitioned {
                "rejected"
            } else {
                self.state_name()
            },
            transitioned
        );
    }

    pub(super) fn is_pending(&self) -> bool {
        self.state() == AuthorizationState::Pending
    }

    fn can_open_window(&self) -> bool {
        matches!(
            self.state(),
            AuthorizationState::Pending | AuthorizationState::Confirmed
        )
    }

    fn invalidate(&self, reason: &'static str) {
        let previous = AuthorizationState::from_raw(
            self.state
                .swap(AuthorizationState::Invalidated as u8, Ordering::AcqRel),
        );
        trace::trace!(
            "component=authorization event=transition authorization_id={} pid={} target_generation={} authorization=invalidated previous={} reason={}",
            self.sequence,
            self.pid,
            self.target_generation,
            previous.name(),
            reason
        );
    }

    fn state(&self) -> AuthorizationState {
        AuthorizationState::from_raw(self.state.load(Ordering::Acquire))
    }

    fn state_name(&self) -> &'static str {
        self.state().name()
    }
}

impl PartialEq for InputAuthorization {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

impl Eq for InputAuthorization {}

impl fmt::Debug for InputAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputAuthorization")
            .field("sequence", &self.sequence)
            .field("pid", &self.pid)
            .field("target_generation", &self.target_generation)
            .field("input_at", &self.input_at)
            .field("state", &self.state_name())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum AuthorizationState {
    Pending,
    Confirmed,
    Rejected,
    Invalidated,
}

impl AuthorizationState {
    const fn from_raw(raw: u8) -> Self {
        match raw {
            value if value == Self::Pending as u8 => Self::Pending,
            value if value == Self::Confirmed as u8 => Self::Confirmed,
            value if value == Self::Rejected as u8 => Self::Rejected,
            _ => Self::Invalidated,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Confirmed => "confirmed",
            Self::Rejected => "rejected",
            Self::Invalidated => "invalidated",
        }
    }
}

#[derive(Debug)]
pub(super) struct InputWindowMatch {
    authorizations: Vec<InputAuthorization>,
}

impl InputWindowMatch {
    pub(super) fn is_pending(&self) -> bool {
        !self.has_confirmed()
            && self
                .authorizations
                .iter()
                .any(InputAuthorization::is_pending)
    }

    pub(super) fn resolve_for_flush(&self) -> bool {
        let confirmed = self.has_confirmed();
        let pending = self
            .authorizations
            .iter()
            .filter(|authorization| authorization.is_pending())
            .count();
        trace::trace!(
            "component=authorization event=window_resolution authorization={} reason={} candidate_count={} pending_count={}",
            if confirmed { "confirmed" } else { "none" },
            if confirmed {
                "window_match"
            } else if pending > 0 {
                "keystroke_pending"
            } else {
                "keystroke_rejected"
            },
            self.authorizations.len(),
            pending
        );
        confirmed
    }

    fn has_confirmed(&self) -> bool {
        self.authorizations
            .iter()
            .any(|authorization| authorization.state() == AuthorizationState::Confirmed)
    }
}

#[derive(Clone)]
pub struct InputAuthorizationPublisher {
    sender: SyncSender<AuthorizationAttempt>,
    issued_sequence: Arc<AtomicU64>,
    integrity_lost: Arc<AtomicBool>,
}

impl InputAuthorizationPublisher {
    pub(crate) fn prepare(
        &self,
        pid: i32,
        target_generation: u64,
        input_at: Instant,
    ) -> Result<InputAuthorization, AuthorizationQueueError> {
        let sequence = self.next_sequence();
        let authorization = InputAuthorization::pending(sequence, pid, target_generation, input_at);
        let attempt = AuthorizationAttempt {
            sequence,
            at: input_at,
            kind: AttemptKind::Reservation(authorization.clone()),
        };
        if self.sender.try_send(attempt).is_err() {
            self.integrity_lost.store(true, Ordering::Release);
            authorization.invalidate("authorization_queue_full");
            return Err(AuthorizationQueueError);
        }
        trace::trace!(
            "component=eventtap event=key_authorization authorization_id={} pid={} target_generation={} authorization=reserved reason=known_text_target",
            sequence,
            pid,
            target_generation
        );
        Ok(authorization)
    }

    pub(crate) fn reject_attempt(
        &self,
        pid: Option<i32>,
        input_at: Instant,
    ) -> Result<(), AuthorizationQueueError> {
        let sequence = self.next_sequence();
        self.sender
            .try_send(AuthorizationAttempt {
                sequence,
                at: input_at,
                kind: AttemptKind::RejectedInput { pid },
            })
            .map_err(|_| {
                self.integrity_lost.store(true, Ordering::Release);
                AuthorizationQueueError
            })?;
        trace::trace!(
            "component=eventtap event=key_authorization authorization_id={} pid={} target_generation=none authorization=rejected reason=invalid_target",
            sequence,
            pid.map_or_else(|| "none".to_owned(), |pid| pid.to_string())
        );
        Ok(())
    }

    fn next_sequence(&self) -> u64 {
        self.issued_sequence.fetch_add(1, Ordering::AcqRel) + 1
    }
}

#[derive(Debug)]
pub(crate) struct AuthorizationQueueError;

struct AuthorizationAttempt {
    sequence: u64,
    at: Instant,
    kind: AttemptKind,
}

enum AttemptKind {
    Reservation(InputAuthorization),
    RejectedInput { pid: Option<i32> },
}

pub struct InputAuthorizations {
    receiver: Receiver<AuthorizationAttempt>,
    integrity_lost: Arc<AtomicBool>,
    received_sequence: u64,
    pending: VecDeque<AuthorizationAttempt>,
}

#[must_use]
pub fn input_authorization_channel() -> (InputAuthorizationPublisher, InputAuthorizations) {
    let (sender, receiver) = sync_channel(AUTHORIZATION_QUEUE_CAPACITY);
    let issued_sequence = Arc::new(AtomicU64::new(0));
    let integrity_lost = Arc::new(AtomicBool::new(false));
    (
        InputAuthorizationPublisher {
            sender,
            issued_sequence,
            integrity_lost: Arc::clone(&integrity_lost),
        },
        InputAuthorizations {
            receiver,
            integrity_lost,
            received_sequence: 0,
            pending: VecDeque::new(),
        },
    )
}

impl InputAuthorizations {
    #[cfg(test)]
    pub(crate) fn matching_for_test(
        &mut self,
        pid: i32,
        target_generation: u64,
        notification_at: Instant,
    ) -> bool {
        self.matching(pid, target_generation, notification_at)
            .is_some()
    }

    pub(super) fn matching(
        &mut self,
        pid: i32,
        target_generation: u64,
        notification_at: Instant,
    ) -> Option<InputWindowMatch> {
        self.receive_pending();
        let mut candidates = Vec::new();
        let mut same_target_keystroke = false;
        let mut rejected_keystroke = false;
        for attempt in &self.pending {
            let AttemptKind::Reservation(authorization) = &attempt.kind else {
                continue;
            };
            if authorization.pid != pid || authorization.target_generation != target_generation {
                continue;
            }
            same_target_keystroke = true;
            if authorization.input_at > notification_at
                || notification_at.saturating_duration_since(authorization.input_at) > INPUT_WINDOW
            {
                continue;
            }
            if authorization.can_open_window() {
                candidates.push(authorization.clone());
            } else if authorization.state() == AuthorizationState::Rejected {
                rejected_keystroke = true;
            }
        }

        let matched = (!candidates.is_empty()).then_some(InputWindowMatch {
            authorizations: candidates,
        });
        if let Some(matched) = matched.as_ref() {
            trace::trace!(
                "component=authorization event=match pid={} target_generation={} authorization=matched reason=window_match candidate_count={}",
                pid,
                target_generation,
                matched.authorizations.len()
            );
        } else {
            let reason = if rejected_keystroke {
                "rejected_keystroke"
            } else if same_target_keystroke {
                "outside_window"
            } else {
                "no_keystroke"
            };
            trace::trace!(
                "component=authorization event=match pid={} target_generation={} authorization=none reason={}",
                pid,
                target_generation,
                reason
            );
        }
        self.prune(notification_at);
        matched
    }

    pub(super) fn invalidate(&mut self, pid: i32, target_generation: u64) {
        self.receive_pending();
        self.retain_attempts(
            |authorization| {
                authorization.pid != pid || authorization.target_generation != target_generation
            },
            "target_invalidated",
        );
    }

    pub(crate) fn remove_pid(&mut self, pid: i32) {
        self.receive_pending();
        self.retain_attempts(|authorization| authorization.pid != pid, "pid_detached");
        self.pending.retain(|attempt| {
            !matches!(attempt.kind, AttemptKind::RejectedInput { pid: Some(value) } if value == pid)
        });
    }

    pub(crate) fn receive_pending(&mut self) {
        let mut sequence_gap = false;
        while let Ok(attempt) = self.receiver.try_recv() {
            if attempt.sequence != self.received_sequence.saturating_add(1) {
                sequence_gap = true;
            }
            self.received_sequence = attempt.sequence;
            if self.pending.len() == AUTHORIZATION_QUEUE_CAPACITY {
                trace::trace!(
                    "component=authorization event=queue_integrity authorization=invalidated reason=pending_capacity capacity={}",
                    AUTHORIZATION_QUEUE_CAPACITY
                );
                self.invalidate_all("pending_capacity");
            }
            trace::trace!(
                "component=authorization event=queue_receive authorization_id={} kind={} pending_count={}",
                attempt.sequence,
                attempt.kind.name(),
                self.pending.len() + 1
            );
            self.pending.push_back(attempt);
        }
        let integrity_lost = self.integrity_lost.swap(false, Ordering::AcqRel);
        if sequence_gap || integrity_lost {
            let reason = if sequence_gap {
                "sequence_gap"
            } else {
                "channel_publish_failed"
            };
            trace::trace!(
                "component=authorization event=queue_integrity authorization=invalidated reason={} received_sequence={}",
                reason,
                self.received_sequence
            );
            self.invalidate_all(reason);
        }
    }

    fn prune(&mut self, now: Instant) {
        let mut retained = VecDeque::with_capacity(self.pending.len());
        let mut expired_count = 0;
        while let Some(attempt) = self.pending.pop_front() {
            let expired = now.saturating_duration_since(attempt.at) > INPUT_WINDOW;
            let opens_window = match &attempt.kind {
                AttemptKind::Reservation(authorization) => authorization.can_open_window(),
                AttemptKind::RejectedInput { .. } => false,
            };
            if expired || !opens_window {
                expired_count += usize::from(expired);
            } else {
                retained.push_back(attempt);
            }
        }
        self.pending = retained;
        if expired_count > 0 {
            trace::trace!(
                "component=authorization event=prune authorization_count={} reason=window_expired",
                expired_count
            );
        }
    }

    fn retain_attempts(
        &mut self,
        keep: impl Fn(&InputAuthorization) -> bool,
        reason: &'static str,
    ) {
        let mut retained = VecDeque::with_capacity(self.pending.len());
        while let Some(attempt) = self.pending.pop_front() {
            match attempt.kind {
                AttemptKind::Reservation(authorization) if !keep(&authorization) => {
                    authorization.invalidate(reason);
                }
                kind => retained.push_back(AuthorizationAttempt {
                    sequence: attempt.sequence,
                    at: attempt.at,
                    kind,
                }),
            }
        }
        self.pending = retained;
    }

    fn invalidate_all(&mut self, reason: &'static str) {
        while let Some(attempt) = self.pending.pop_front() {
            if let AttemptKind::Reservation(authorization) = attempt.kind {
                authorization.invalidate(reason);
            }
        }
    }
}

impl AttemptKind {
    const fn name(&self) -> &'static str {
        match self {
            Self::Reservation(_) => "reservation",
            Self::RejectedInput { .. } => "rejected_input",
        }
    }
}
