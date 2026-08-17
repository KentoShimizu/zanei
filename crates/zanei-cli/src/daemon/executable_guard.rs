use std::{path::Path, path::PathBuf};

// Three 5-second misses tolerate transient filesystem failures while still stopping within tens
// of seconds after an uninstall removes the cached executable path.
const REQUIRED_CONSECUTIVE_METADATA_FAILURES: u8 = 3;

pub(super) struct ExecutableGuard {
    executable: PathBuf,
    consecutive_metadata_failures: u8,
}

impl ExecutableGuard {
    pub(super) const fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            consecutive_metadata_failures: 0,
        }
    }

    pub(super) fn check_with(&mut self, exists: impl FnOnce(&Path) -> bool) -> bool {
        if exists(&self.executable) {
            self.consecutive_metadata_failures = 0;
            return false;
        }
        self.consecutive_metadata_failures = self.consecutive_metadata_failures.saturating_add(1);
        self.consecutive_metadata_failures >= REQUIRED_CONSECUTIVE_METADATA_FAILURES
    }
}
