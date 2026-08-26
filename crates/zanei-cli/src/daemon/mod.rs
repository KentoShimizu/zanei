//! Recording orchestration and launchd integration.

mod collectors;
mod control;
mod executable_guard;
mod main_thread;
mod ownership;
mod permission_worker;
mod pipeline;
mod runtime;
mod runtime_support;
mod shutdown;
mod supervisor;

use std::{io, path::PathBuf, process::ExitStatus};

pub(crate) use collectors::chrome_tracking_required;
pub(crate) use control::{
    DAEMON_CONTROL_POLL_INTERVAL, DAEMON_CONTROL_TIMEOUT, bootout, is_bootstrapped,
    start_launch_agent, terminate, wait_for_launch_agent_removal,
};
pub(crate) use ownership::{StoreOwner, StoreOwnership, mode_name};
pub use runtime::{RecordOutput, required_capabilities_for, run_daemon, run_record};

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error(transparent)]
    Config(#[from] zanei_core::config::ConfigError),
    #[error(transparent)]
    Store(#[from] zanei_core::store::StoreError),
    #[error("required environment variable {name} is not set")]
    MissingEnvironment { name: &'static str },
    #[error("failed to {operation} {path}: {source}")]
    File {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to run {program} for {operation}: {source}")]
    CommandLaunch {
        program: &'static str,
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("{program} failed while attempting to {operation} with {status}: {stderr}")]
    CommandFailed {
        program: &'static str,
        operation: &'static str,
        status: ExitStatus,
        stderr: String,
    },
    #[error("{program} returned an invalid user ID: {output}")]
    InvalidUserId {
        program: &'static str,
        output: String,
    },
    #[error("failed to register the {signal} shutdown handler: {source}")]
    SignalRegistration {
        signal: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to resolve the current executable: {0}")]
    CurrentExecutable(#[source] io::Error),
    #[error("failed to spawn {thread} thread: {source}")]
    ThreadSpawn {
        thread: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("another recorder owns this store (pid {pid})")]
    StoreOwned { pid: u32 },
    #[error("failed to {operation} recorder ownership file {path}: {source}")]
    OwnershipFile {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid recorder ownership metadata in {path}: {reason}")]
    InvalidOwnershipMetadata { path: PathBuf, reason: String },
    #[error("invalid foreground recorder process ID {pid}")]
    InvalidRecorderPid { pid: u32 },
    #[error("recorder instance changed while attempting to stop {instance_id}")]
    RecorderInstanceChanged { instance_id: String },
    #[error("timed out waiting for recorder instance {instance_id} to stop")]
    RecorderStopTimeout { instance_id: String },
    #[error("launchd recorder instance {instance_id} is not registered")]
    LaunchdRecorderNotRegistered { instance_id: String },
    #[error(
        "Zanei launch agent is still registered after {timeout_seconds} seconds; run `zanei stop`, then run `zanei start` again"
    )]
    LaunchAgentStillLoaded { timeout_seconds: u64 },
    #[error(
        "Zanei daemon did not report that it was alive within {timeout_seconds} seconds; it remains registered with launchd. Run `zanei status` to inspect its state, check the last exit code with `launchctl print gui/$(id -u)/dev.zanei.agent`, or run `zanei start --foreground` to see the error directly"
    )]
    DaemonDidNotStart { timeout_seconds: u64 },
    #[error(transparent)]
    MainRunLoop(#[from] zanei_macos::main_run_loop::MainRunLoopError),
    #[error(transparent)]
    Permission(#[from] zanei_macos::permission::PermissionError),
    #[error("{thread} thread terminated unexpectedly")]
    ThreadTerminated { thread: &'static str },
    #[error("pipeline control channel disconnected during {operation}")]
    PipelineControl { operation: &'static str },
    #[error("pipeline output failed: {0}")]
    PipelineOutput(#[from] zanei_core::sink::SinkError),
    #[error("pipeline synchronization primitive was poisoned: {name}")]
    SynchronizationPoisoned { name: &'static str },
    #[error("invalid persisted paused_until value {value}: {source}")]
    InvalidPausedUntil {
        value: String,
        #[source]
        source: time::error::Parse,
    },
    #[error("failed to read standard input while waiting for EOF: {0}")]
    Stdin(io::Error),
}
