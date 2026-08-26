//! Canonical AX observer and operation health state.

use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AxFailurePhase {
    Attach,
    Runtime,
    Observer,
    ValueLifecycle,
    SecureInput,
}

impl fmt::Display for AxFailurePhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Attach => "attach",
            Self::Runtime => "runtime",
            Self::Observer => "observer",
            Self::ValueLifecycle => "value_lifecycle",
            Self::SecureInput => "secure_input",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AxFailureKind {
    InvalidPid,
    NativeAx { operation: &'static str, code: i32 },
    SecureInputProbeDisconnected,
    SecureInputProbeTimeout,
}

impl fmt::Display for AxFailureKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPid => formatter.write_str("invalid_pid"),
            Self::NativeAx { operation, code } => {
                write!(formatter, "native_ax operation={operation} code={code}")
            }
            Self::SecureInputProbeDisconnected => {
                formatter.write_str("secure_input_probe_disconnected")
            }
            Self::SecureInputProbeTimeout => formatter.write_str("secure_input_probe_timeout"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AxFailure {
    pub pid: Option<i64>,
    pub phase: AxFailurePhase,
    pub kind: AxFailureKind,
}

impl AxFailure {
    pub(crate) const fn new(pid: Option<i64>, phase: AxFailurePhase, kind: AxFailureKind) -> Self {
        Self { pid, phase, kind }
    }
}

impl fmt::Display for AxFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "phase={} kind={}", self.phase, self.kind)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AxFailureState {
    current: Option<AxFailure>,
    unresolved_sites: usize,
}

impl AxFailureState {
    #[must_use]
    pub const fn current(self) -> Option<AxFailure> {
        self.current
    }

    #[must_use]
    pub const fn unresolved_sites(self) -> usize {
        self.unresolved_sites
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum AxRecoverySite {
    Attach,
    WindowCreatedRegistration,
    LoadCompleteRegistration,
    Decode,
    HitTestElement,
    HitTestSnapshot,
    SecureInputAttach,
    SecureInputDetach,
    SecureInputPoll,
    SecureInputFlush,
    SecureInputDecode,
    #[cfg(test)]
    SecureInputTest,
    ApplicationRole,
    FocusedWindow,
    FocusedElement,
    WindowTitleUnregistration,
    FocusedValueRegistration,
    FocusedValueUnregistration,
    ValueSnapshot,
    FieldClassSnapshot,
    FocusChangeSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FailureKey {
    pid: Option<i64>,
    site: AxRecoverySite,
}

#[derive(Default)]
struct FailureLedger {
    sequence: u64,
    unresolved: HashMap<FailureKey, (AxFailure, u64)>,
}

#[derive(Clone, Default)]
pub(crate) struct AxFailurePublisher(Arc<Mutex<FailureLedger>>);

impl AxFailurePublisher {
    pub(crate) fn record(&self, counter: &AtomicU64, site: AxRecoverySite, failure: AxFailure) {
        counter.fetch_add(1, Ordering::Relaxed);
        let key = FailureKey {
            pid: failure.pid,
            site,
        };
        let mut ledger = self.0.lock().unwrap_or_else(|error| error.into_inner());
        ledger.sequence = ledger.sequence.saturating_add(1);
        let sequence = ledger.sequence;
        let action = ledger
            .unresolved
            .get(&key)
            .map_or("occurrence", |(old, _)| {
                if *old == failure { "repeat" } else { "change" }
            });
        ledger.unresolved.insert(key, (failure, sequence));
        drop(ledger);
        if action != "repeat" {
            crate::trace::trace!(
                "component=ax phase=health action={} pid={:?} site={:?} {}",
                action,
                failure.pid,
                site,
                failure
            );
        }
    }

    pub(crate) fn record_native(
        &self,
        counter: &AtomicU64,
        pid: Option<i64>,
        phase: AxFailurePhase,
        site: AxRecoverySite,
        operation: &'static str,
        code: i32,
    ) {
        self.record(
            counter,
            site,
            AxFailure::new(pid, phase, AxFailureKind::NativeAx { operation, code }),
        );
    }

    pub(crate) fn recover(&self, pid: Option<i64>, site: AxRecoverySite) {
        let key = FailureKey { pid, site };
        let recovered = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unresolved
            .remove(&key)
            .is_some();
        if recovered {
            crate::trace::trace!(
                "component=ax phase=health action=recovery pid={:?} site={:?}",
                pid,
                site
            );
        }
    }

    pub(crate) fn remove_pid(&self, pid: i64) {
        self.recover_matching(|key| key.pid == Some(pid));
    }

    pub(crate) fn clear(&self) {
        self.recover_matching(|_| true);
    }

    fn recover_matching(&self, predicate: impl Fn(FailureKey) -> bool) {
        let keys: Vec<_> = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unresolved
            .keys()
            .copied()
            .filter(|key| predicate(*key))
            .collect();
        for key in keys {
            self.recover(key.pid, key.site);
        }
    }

    pub(crate) fn state(&self) -> AxFailureState {
        let ledger = self.0.lock().unwrap_or_else(|error| error.into_inner());
        AxFailureState {
            current: ledger
                .unresolved
                .values()
                .max_by_key(|(_, sequence)| sequence)
                .map(|(failure, _)| *failure),
            unresolved_sites: ledger.unresolved.len(),
        }
    }
}

pub(super) struct ObserverHealth {
    used_pids: BTreeSet<i64>,
    unavailable_pids: BTreeSet<i64>,
    published_count: Arc<AtomicU64>,
}

impl ObserverHealth {
    pub(super) fn new(published_count: Arc<AtomicU64>) -> Self {
        Self {
            used_pids: BTreeSet::new(),
            unavailable_pids: BTreeSet::new(),
            published_count,
        }
    }

    pub(super) fn mark_used(&mut self, pid: i64) {
        self.used_pids.insert(pid);
        self.publish();
    }

    pub(super) fn mark_unavailable(&mut self, pid: i64) {
        self.unavailable_pids.insert(pid);
        self.publish();
    }

    pub(super) fn mark_available(&mut self, pid: i64) {
        self.unavailable_pids.remove(&pid);
        self.publish();
    }

    pub(super) fn remove(&mut self, pid: i64) {
        self.used_pids.remove(&pid);
        self.unavailable_pids.remove(&pid);
        self.publish();
    }

    pub(super) fn clear(&mut self) {
        self.used_pids.clear();
        self.unavailable_pids.clear();
        self.publish();
    }

    fn publish(&self) {
        let unavailable_used = self.unavailable_pids.intersection(&self.used_pids).count();
        let count = u64::try_from(unavailable_used).unwrap_or(u64::MAX);
        self.published_count.store(count, Ordering::Relaxed);
    }
}
