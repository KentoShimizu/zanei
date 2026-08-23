//! OS-independent contracts implemented by platform event collectors.

use std::sync::mpsc::SyncSender;

mod app_directory;

pub use app_directory::{AppDirectory, AppDirectoryError, AppInfo};
pub use zanei_core::RawEvent;

pub const COLLECTOR_CHANNEL_CAPACITY: usize = 4_096;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Permission {
    Accessibility,
    InputMonitoring,
    Automation { bundle_id: String },
}

#[derive(Debug, thiserror::Error)]
pub enum CollectorError {
    #[error("collector {collector} is already running")]
    AlreadyRunning { collector: String },
    #[error("collector {collector} failed to start: {message}")]
    Start { collector: String, message: String },
}

pub trait Collector: Send {
    fn name(&self) -> &str;

    fn required_permissions(&self) -> &[Permission];

    fn start(&mut self, sender: SyncSender<RawEvent>) -> Result<(), CollectorError>;

    fn stop(&mut self);
}
