use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use zanei_collector::Permission;
use zanei_core::{
    config::{CONFIG_WATCH_INTERVAL, Config, ConfigWatcher},
    normalize::format_timestamp,
    store::{
        DaemonMode, DaemonPermissions, DaemonState, LockedReason, StoreError, StoreFormat,
        StoreReader, StoreStatus, StoreWriter, purge_retired_plaintext, retired_plaintext_stores,
        set_aside_plaintext,
    },
};
use zanei_macos::permission::{PermissionError, PermissionStatus, permission_status};

use super::{
    DaemonError, StoreOwner, StoreOwnership,
    collectors::{CollectorSet, merge_collector_failures},
    executable_guard::ExecutableGuard,
    main_thread,
    permission_worker::{PermissionRequestPoll, PermissionRequestWorker},
    pipeline::{Pipeline, SharedStoreWriter},
    runtime_support::{
        ShutdownSignals, StdinEofWatcher, ensure_store_parent, record_writer,
        restrict_store_permissions,
    },
    shutdown::shutdown_daemon,
};
use crate::permissions::{PermissionRequestOutcome, probe_permissions};
use crate::store_access::{self, KeyAccess, KeyPrompt};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const PAUSE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const RETENTION_PURGE_INTERVAL: time::Duration = time::Duration::minutes(10);
const RUNTIME_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PERMISSION_REQUEST_TIMEOUT_MESSAGE: &str =
    "permission request timed out before macOS reported a decision";
const PERMISSION_REQUEST_WORKER_STOPPED_MESSAGE: &str =
    "permission request worker terminated without a result";
const EXECUTABLE_REMOVED_MESSAGE: &str =
    "zanei: executable removed (uninstalled?); recorder shutting down.";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordOutput {
    Stdout,
    File(PathBuf),
}

pub fn required_permissions_for(config: &Config) -> BTreeSet<Permission> {
    CollectorSet::new(config).required_permissions()
}

pub fn run_daemon(
    config_path: &Path,
    store_path: &Path,
    mode: DaemonMode,
) -> Result<(), DaemonError> {
    let config = Config::load(config_path)?;
    let mut config_watcher = ConfigWatcher::new(config_path.to_owned())?;
    let executable_guard =
        ExecutableGuard::new(crate::executable::current().map_err(DaemonError::CurrentExecutable)?);
    ensure_store_parent(store_path)?;
    restrict_store_permissions(store_path)?;
    let started_at = format_timestamp(OffsetDateTime::now_utc());
    let owner = StoreOwner::new(mode, started_at);
    // Ownership comes first: setting a plaintext store aside renames files, and
    // only the single recorder that owns the store may do that.
    let ownership = StoreOwnership::acquire(store_path, owner.clone())?;
    let (mut writer, reader) = open_encrypted_store(store_path)?;
    let initial_status = reader.status()?;
    let base_dropped = initial_status.events_dropped;
    let base_collector_failures = initial_status.collector_failures.clone();
    let initial_heartbeat =
        initial_heartbeat(&owner, config.output.retention_hours, &initial_status);
    let (
        main_run_loop,
        mut collectors,
        initial_input_monitoring_status,
        permission_request_worker,
        main_thread_observers,
    ) = initialize_permission_dependent_runtime(&writer, &initial_heartbeat, || {
        let main_run_loop = main_thread::prepare()?;
        let mut collectors = CollectorSet::new(&config);
        let initial_input_monitoring_status = collectors
            .has_eventtap()
            .then(|| permission_status(&Permission::InputMonitoring));
        let permission_request_worker =
            PermissionRequestWorker::start(collectors.required_permissions())?;
        let main_thread_observers = collectors.prepare_main_thread();
        Ok((
            main_run_loop,
            collectors,
            initial_input_monitoring_status,
            permission_request_worker,
            main_thread_observers,
        ))
    })?;
    apply_retention(
        &mut writer,
        store_path,
        OffsetDateTime::now_utc(),
        config.output.retention_hours,
    )?;
    let writer = Arc::new(Mutex::new(writer));
    let mut pipeline = Pipeline::store(&config, Arc::clone(&writer))?;
    let mut runtime_degraded = BTreeMap::new();
    let mut paused = false;
    let _main_thread_observers = main_thread_observers;
    main_thread::run(main_run_loop, "daemon-runtime", move || {
        let _ownership = ownership;
        let loop_result = ActiveDaemon {
            store_path,
            config_watcher: &mut config_watcher,
            active_retention_hours: config.output.retention_hours,
            pending_retention_hours: None,
            writer: &writer,
            reader: &reader,
            pipeline: &pipeline,
            collectors: &mut collectors,
            owner: &owner,
            base_dropped,
            base_collector_failures: &base_collector_failures,
            paused: &mut paused,
            intake_suspended: false,
            degraded: &mut runtime_degraded,
            last_status: initial_status,
            last_permissions: None,
            initial_input_monitoring_status,
            permission_request_worker: Some(permission_request_worker),
            executable_guard,
        }
        .run();

        shutdown_daemon(
            loop_result,
            &writer,
            &reader,
            &mut collectors,
            &mut pipeline,
            base_dropped,
            &base_collector_failures,
        )
    })?
}

/// Applies the retention window everywhere it matters: expired events in the
/// live store, expired events inside set-aside plaintext stores, and set-aside
/// stores whose timestamp has left the window (deleted whole). Every purge the
/// recorder runs — at startup, periodically, and when retention changes — goes
/// through here.
fn apply_retention(
    writer: &mut StoreWriter,
    store_path: &Path,
    now: OffsetDateTime,
    retention_hours: u64,
) -> Result<(), StoreError> {
    writer.purge_retention(now, retention_hours)?;
    for retired in purge_retired_plaintext(store_path, now, retention_hours)? {
        eprintln!(
            "zanei: removed the set-aside plaintext store {} (older than the retention window)",
            retired.path.display()
        );
    }
    Ok(())
}

/// Opens the recorder's store, creating its key on first use. A store written
/// before encryption existed is not rewritten: it is set aside under a
/// timestamped name, a fresh encrypted store takes its place, and readers keep
/// returning the old events alongside the new ones until they age out. The
/// recorder never shows key store dialogs: a locked or inaccessible key store
/// fails the start with a message.
fn open_encrypted_store(store_path: &Path) -> Result<(StoreWriter, StoreReader), DaemonError> {
    let format = StoreFormat::probe(store_path)?;
    // An encrypted store only ever gets its existing key: generating a fresh one
    // would turn "key missing" into "key mismatch" and collide with the original
    // item if it is restored later. A new or plaintext store gets a key created.
    // The key comes first so a locked or denied key store leaves a plaintext
    // store exactly where it was, still readable by every command.
    let key = match format {
        StoreFormat::Encrypted => Some(
            store_access::load_store_key(KeyAccess::Existing, KeyPrompt::Suppressed)?
                .ok_or(StoreError::Locked(LockedReason::KeyMissing))?,
        ),
        StoreFormat::Missing | StoreFormat::Plaintext => Some(
            store_access::load_store_key(KeyAccess::CreateIfMissing, KeyPrompt::Suppressed)?
                .ok_or(StoreError::Locked(LockedReason::KeyMissing))?,
        ),
        // Let the open below report the damage instead of touching any key.
        StoreFormat::Unrecognized => None,
    };
    let format = if format == StoreFormat::Plaintext {
        if let Some(retired) = set_aside_plaintext(store_path, OffsetDateTime::now_utc())? {
            eprintln!(
                "zanei: kept the previous plaintext store as {}; its events stay readable \
                 next to the new encrypted store until they age out of retention",
                retired.path.display()
            );
        }
        StoreFormat::Missing
    } else {
        format
    };
    // The format was probed before any connection existed; probing again while
    // the writer is open would drop its WAL-mode file lock (see
    // `StoreFormat::probe`).
    let writer = StoreWriter::open_known(store_path, format, key.as_ref())?;
    let reader = StoreReader::open_known(store_path, writer.format(), key.as_ref())?;
    adopt_previous_state_if_fresh(&writer, &reader, store_path)?;
    restrict_store_permissions(store_path)?;
    Ok((writer, reader))
}

/// Carries the newest set-aside store's daemon state — an active pause, the
/// counters, the last event time, collector failures, the last permission
/// report — into a live store the recorder has never used. "Never used" is
/// `started_at IS NULL`: the first heartbeat sets it, so a crash between
/// creating the encrypted store and this step is simply retried on the next
/// start, and a store that has recorded is never touched again. The state is
/// the user's, not the file's, so an unreadable set-aside store is reported
/// rather than allowed to stop the start.
fn adopt_previous_state_if_fresh(
    writer: &StoreWriter,
    reader: &StoreReader,
    store_path: &Path,
) -> Result<(), DaemonError> {
    if reader.status()?.started_at.is_some() {
        return Ok(());
    }
    let Some(previous) = retired_plaintext_stores(store_path)?.pop() else {
        return Ok(());
    };
    // Probing opens the set-aside file only, never the live store.
    if StoreFormat::probe(&previous.path)? != StoreFormat::Plaintext {
        return Ok(());
    }
    match StoreReader::open_known(&previous.path, StoreFormat::Plaintext, None)
        .and_then(|previous| previous.status())
    {
        Ok(state) => writer.adopt_daemon_state(&state)?,
        Err(error) => eprintln!(
            "zanei: could not carry the previous store's state over from {}: {error}",
            previous.path.display()
        ),
    }
    Ok(())
}

pub fn run_record(config_path: &Path, output: RecordOutput) -> Result<(), DaemonError> {
    let config = Config::load(config_path)?;
    let main_run_loop = main_thread::prepare()?;
    let writer = record_writer(output)?;
    let mut pipeline = Pipeline::stream(&config, writer)?;
    let mut collectors = CollectorSet::new(&config);
    let _main_thread_observers = collectors.prepare_main_thread();
    main_thread::run(main_run_loop, "record-runtime", move || {
        let record_result = wait_for_record_shutdown(&pipeline, &mut collectors);
        collectors.stop();
        let pipeline_result = pipeline.shutdown();
        record_result.and(pipeline_result)
    })?
}

struct ActiveDaemon<'a> {
    store_path: &'a Path,
    config_watcher: &'a mut ConfigWatcher,
    active_retention_hours: u64,
    pending_retention_hours: Option<u64>,
    writer: &'a SharedStoreWriter,
    reader: &'a StoreReader,
    pipeline: &'a Pipeline,
    collectors: &'a mut CollectorSet,
    owner: &'a StoreOwner,
    base_dropped: u64,
    base_collector_failures: &'a BTreeMap<String, u64>,
    paused: &'a mut bool,
    intake_suspended: bool,
    degraded: &'a mut BTreeMap<String, String>,
    last_status: StoreStatus,
    last_permissions: Option<DaemonPermissions>,
    initial_input_monitoring_status: Option<Result<PermissionStatus, PermissionError>>,
    permission_request_worker: Option<PermissionRequestWorker>,
    executable_guard: ExecutableGuard,
}

impl ActiveDaemon<'_> {
    fn run(mut self) -> Result<(), DaemonError> {
        let signals = ShutdownSignals::install()?;
        *self.paused =
            normalize_pause_request(self.writer, self.last_status.paused_until.as_deref())?;
        configure_eventtap_start_gate(
            self.initial_input_monitoring_status.take(),
            &mut self.collectors.eventtap_start_gate,
            self.degraded,
        );
        if !*self.paused {
            self.collectors.start(self.pipeline.sender());
        }
        self.poll_permission_request();
        self.publish_heartbeat()?;
        self.run_loop(signals.stop_flag())
    }

    fn run_loop(&mut self, stop: Arc<AtomicBool>) -> Result<(), DaemonError> {
        let mut last_heartbeat = Instant::now();
        let mut last_pause_poll = Instant::now();
        let mut last_config_poll = Instant::now();
        let mut retention_purge_deadline = OffsetDateTime::now_utc() + RETENTION_PURGE_INTERVAL;

        while !stop.load(Ordering::Relaxed) {
            self.poll_permission_request();
            self.sync_store_intake()?;
            if last_pause_poll.elapsed() >= PAUSE_POLL_INTERVAL {
                self.update_pause_state()?;
                last_pause_poll = Instant::now();
            }
            let now = OffsetDateTime::now_utc();
            if last_config_poll.elapsed() >= CONFIG_WATCH_INTERVAL {
                let retention_promoted = match self.config_watcher.reload_if_changed() {
                    Ok(Some(config)) => {
                        self.collectors.replace_filter(config.filter.clone());
                        self.pipeline.replace_filter(&config.filter)?;
                        self.degraded.remove("config");
                        self.request_retention_reload(config.output.retention_hours, now)?
                    }
                    Ok(None) => self.retry_pending_retention(now)?,
                    Err(error) => {
                        self.degraded.insert("config".to_owned(), error.to_string());
                        false
                    }
                };
                if retention_promoted {
                    retention_purge_deadline = now + RETENTION_PURGE_INTERVAL;
                }
                last_config_poll = Instant::now();
            }
            if now >= retention_purge_deadline {
                self.purge_active_retention(now)?;
                retention_purge_deadline = now + RETENTION_PURGE_INTERVAL;
            }
            if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                if executable_shutdown_requested(
                    &mut self.executable_guard,
                    |path| fs::metadata(path).is_ok(),
                    |message| eprintln!("{message}"),
                ) {
                    return Ok(());
                }
                ensure_pipeline_running(self.pipeline, self.collectors)?;
                let permissions = self.refresh_permissions();
                if !*self.paused && !self.intake_suspended {
                    self.collectors.supervise(
                        self.pipeline.sender(),
                        permissions.as_ref(),
                        Instant::now(),
                    )?;
                }
                self.publish_heartbeat_with_permissions(permissions)?;
                last_heartbeat = Instant::now();
            }
            thread::sleep(RUNTIME_POLL_INTERVAL);
        }
        Ok(())
    }

    fn request_retention_reload(
        &mut self,
        requested_retention_hours: u64,
        now: OffsetDateTime,
    ) -> Result<bool, DaemonError> {
        if requested_retention_hours == self.active_retention_hours {
            self.pending_retention_hours = None;
            self.degraded.remove("retention");
            return Ok(false);
        }
        self.pending_retention_hours = Some(requested_retention_hours);
        self.retry_pending_retention(now)
    }

    fn retry_pending_retention(&mut self, now: OffsetDateTime) -> Result<bool, DaemonError> {
        let Some(retention_hours) = self.pending_retention_hours else {
            return Ok(false);
        };
        let purge_result = apply_retention(
            &mut *lock_writer(self.writer)?,
            self.store_path,
            now,
            retention_hours,
        );
        match purge_result {
            Ok(_) => {
                self.active_retention_hours = retention_hours;
                self.pending_retention_hours = None;
                self.degraded.remove("retention");
                self.publish_heartbeat()?;
                Ok(true)
            }
            Err(error) => {
                self.degraded
                    .insert("retention".to_owned(), error.to_string());
                Ok(false)
            }
        }
    }

    fn purge_active_retention(&mut self, now: OffsetDateTime) -> Result<(), DaemonError> {
        let purge_result = apply_retention(
            &mut *lock_writer(self.writer)?,
            self.store_path,
            now,
            self.active_retention_hours,
        );
        match purge_result {
            Ok(_) if self.pending_retention_hours.is_none() => {
                self.degraded.remove("retention");
            }
            Ok(_) => {}
            Err(error) => {
                self.degraded
                    .insert("retention".to_owned(), error.to_string());
            }
        }
        Ok(())
    }

    fn sync_store_intake(&mut self) -> Result<(), DaemonError> {
        let accepts_intake = self.pipeline.store_health()?.accepts_intake();
        match (accepts_intake, self.intake_suspended) {
            (false, false) => {
                self.collectors.suspend();
                self.intake_suspended = true;
                self.degraded.insert(
                    "store_intake".to_owned(),
                    "capture intake is stopped while the store recovers".to_owned(),
                );
                self.publish_heartbeat()?;
            }
            (true, true) => {
                self.intake_suspended = false;
                self.degraded.remove("store_intake");
                if !*self.paused {
                    self.collectors.start(self.pipeline.sender());
                }
                self.publish_heartbeat()?;
            }
            _ => {}
        }
        Ok(())
    }

    fn update_pause_state(&mut self) -> Result<(), DaemonError> {
        let status = self.reader.status()?;
        let requested = normalize_pause_request(self.writer, status.paused_until.as_deref())?;
        match (requested, *self.paused) {
            (true, false) => {
                self.collectors.stop();
                self.pipeline.flush()?;
                *self.paused = true;
            }
            (false, true) => {
                if !self.intake_suspended {
                    self.collectors.start(self.pipeline.sender());
                }
                *self.paused = false;
            }
            _ => {}
        }
        Ok(())
    }

    fn publish_heartbeat(&mut self) -> Result<(), DaemonError> {
        let permissions = self.refresh_permissions();
        self.publish_heartbeat_with_permissions(permissions)
    }

    fn refresh_permissions(&mut self) -> Option<DaemonPermissions> {
        if self.permission_request_worker.is_some() {
            return None;
        }
        match probe_permissions(&self.collectors.required_permissions()) {
            Ok(permissions) => {
                self.degraded.remove("permissions");
                self.last_permissions = Some(permissions.clone());
            }
            Err(error) => {
                self.degraded
                    .insert("permissions".to_owned(), error.to_string());
            }
        }
        self.last_permissions.clone()
    }

    fn poll_permission_request(&mut self) {
        let start_now = !*self.paused && !self.intake_suspended;
        service_permission_request_worker(
            &mut self.permission_request_worker,
            self.degraded,
            start_now,
            |start_now| {
                self.collectors.eventtap_start_gate.allow();
                if start_now {
                    self.collectors
                        .start_eventtap(self.pipeline.sender(), Instant::now());
                }
            },
        );
    }

    fn publish_heartbeat_with_permissions(
        &mut self,
        permissions: Option<DaemonPermissions>,
    ) -> Result<(), DaemonError> {
        match self.reader.status() {
            Ok(status) => {
                self.last_status = status;
                self.degraded.remove("store_read");
            }
            Err(error) => {
                self.degraded
                    .insert("store_read".to_owned(), error.to_string());
            }
        }
        let collector_health = self.collectors.health();
        let mut degraded = self.degraded.clone();
        degraded.extend(self.pipeline.degraded()?);
        degraded.extend(collector_health.degraded);
        self.pipeline.heartbeat(DaemonState {
            pid: Some(i64::from(self.owner.pid)),
            started_at: Some(self.owner.started_at.clone()),
            instance_id: Some(self.owner.instance_id.clone()),
            mode: Some(self.owner.mode),
            heartbeat_at: Some(format_timestamp(OffsetDateTime::now_utc())),
            retention_hours: Some(self.active_retention_hours),
            paused_until: self.last_status.paused_until.clone(),
            events_captured: self.last_status.events_captured,
            events_dropped: self
                .base_dropped
                .saturating_add(collector_health.dropped)
                .saturating_add(self.pipeline.dropped()),
            last_event_ts: self.last_status.last_event_ts.clone(),
            degraded,
            collector_failures: merge_collector_failures(
                self.base_collector_failures,
                &collector_health.collector_failures,
            ),
            permissions,
        })
    }
}

pub(super) fn configure_eventtap_start_gate(
    status: Option<Result<PermissionStatus, PermissionError>>,
    gate: &mut super::supervisor::EventTapStartGate,
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

pub(super) fn service_permission_request_worker(
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

fn initialize_permission_dependent_runtime<T>(
    writer: &StoreWriter,
    initial_heartbeat: &DaemonState,
    initialize: impl FnOnce() -> Result<T, DaemonError>,
) -> Result<T, DaemonError> {
    writer.write_daemon_state(initial_heartbeat)?;
    initialize()
}

fn initial_heartbeat(
    owner: &StoreOwner,
    retention_hours: u64,
    status: &StoreStatus,
) -> DaemonState {
    DaemonState {
        pid: Some(i64::from(owner.pid)),
        started_at: Some(owner.started_at.clone()),
        instance_id: Some(owner.instance_id.clone()),
        mode: Some(owner.mode),
        heartbeat_at: Some(format_timestamp(OffsetDateTime::now_utc())),
        retention_hours: Some(retention_hours),
        paused_until: status.paused_until.clone(),
        events_captured: status.events_captured,
        events_dropped: status.events_dropped,
        last_event_ts: status.last_event_ts.clone(),
        degraded: BTreeMap::new(),
        collector_failures: status.collector_failures.clone(),
        permissions: None,
    }
}

fn ensure_pipeline_running(
    pipeline: &Pipeline,
    collectors: &mut CollectorSet,
) -> Result<(), DaemonError> {
    if pipeline.is_finished() {
        collectors.suspend();
        Err(DaemonError::ThreadTerminated { thread: "pipeline" })
    } else {
        Ok(())
    }
}

fn wait_for_record_shutdown(
    pipeline: &Pipeline,
    collectors: &mut CollectorSet,
) -> Result<(), DaemonError> {
    let signals = ShutdownSignals::install()?;
    let stop = signals.stop_flag();
    let stdin = StdinEofWatcher::start()?;
    collectors.start(pipeline.sender());

    while !stop.load(Ordering::Relaxed) {
        if let Some(watcher) = stdin.as_ref() {
            match watcher.try_result() {
                Some(Ok(())) => break,
                Some(Err(error)) => return Err(DaemonError::Stdin(error)),
                None => {}
            }
        }
        thread::sleep(RUNTIME_POLL_INTERVAL);
    }
    Ok(())
}

fn normalize_pause_request(
    writer: &SharedStoreWriter,
    paused_until: Option<&str>,
) -> Result<bool, DaemonError> {
    let Some(paused_until) = paused_until else {
        return Ok(false);
    };
    if paused_until == "infinity" {
        return Ok(true);
    }
    let deadline = OffsetDateTime::parse(paused_until, &Rfc3339).map_err(|source| {
        DaemonError::InvalidPausedUntil {
            value: paused_until.to_owned(),
            source,
        }
    })?;
    if deadline > OffsetDateTime::now_utc() {
        return Ok(true);
    }
    lock_writer(writer)?.set_paused_until(None)?;
    Ok(false)
}

fn executable_shutdown_requested(
    guard: &mut ExecutableGuard,
    exists: impl FnOnce(&Path) -> bool,
    notify: impl FnOnce(&str),
) -> bool {
    if !guard.check_with(exists) {
        return false;
    }
    notify(EXECUTABLE_REMOVED_MESSAGE);
    true
}

fn lock_writer(
    writer: &SharedStoreWriter,
) -> Result<std::sync::MutexGuard<'_, StoreWriter>, DaemonError> {
    writer
        .lock()
        .map_err(|_| DaemonError::SynchronizationPoisoned {
            name: "store writer",
        })
}

#[cfg(test)]
mod tests;
