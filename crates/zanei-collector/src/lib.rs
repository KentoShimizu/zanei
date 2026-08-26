//! OS-independent contracts implemented by platform event collectors.

use std::sync::mpsc::SyncSender;

mod app_directory;

pub use app_directory::{AppDirectory, AppDirectoryError, AppInfo, InstalledApps};
pub use zanei_core::{Capability, RawEvent};

pub const COLLECTOR_CHANNEL_CAPACITY: usize = 4_096;

#[derive(Debug, thiserror::Error)]
pub enum CollectorError {
    #[error("collector {collector} is already running")]
    AlreadyRunning { collector: String },
    #[error("collector {collector} failed to start: {message}")]
    Start { collector: String, message: String },
}

pub trait Collector: Send {
    fn name(&self) -> &str;

    fn required_capabilities(&self) -> &[Capability];

    fn start(&mut self, sender: SyncSender<RawEvent>) -> Result<(), CollectorError>;

    fn stop(&mut self);
}
