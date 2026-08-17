//! Current per-application AX observer health.

use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

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
