//! Native macOS collectors and permission diagnostics.

#[cfg(target_os = "macos")]
pub mod app_directory;
#[cfg(target_os = "macos")]
pub mod ax;
#[cfg(target_os = "macos")]
pub mod chrome;
#[cfg(target_os = "macos")]
pub mod content_snapshot;
#[cfg(target_os = "macos")]
pub mod eventtap;
#[cfg(target_os = "macos")]
mod focused_field;
mod input_source;
#[cfg(target_os = "macos")]
pub mod main_run_loop;
#[cfg(target_os = "macos")]
pub mod permission;
#[cfg(target_os = "macos")]
mod secure_input;
#[cfg(target_os = "macos")]
pub mod store_key;
#[cfg(target_os = "macos")]
mod text_capture;
#[cfg(target_os = "macos")]
mod trace;
#[cfg(target_os = "macos")]
pub mod workspace;

#[cfg(target_os = "macos")]
pub use focused_field::{FocusedFieldPublisher, FocusedFieldTracker, focused_field_channel};
#[cfg(target_os = "macos")]
pub use secure_input::{SecureInputMonitor, SecureInputMonitorError, SecureInputProbe};
#[cfg(target_os = "macos")]
pub use text_capture::TextContentPolicy;
#[cfg(target_os = "macos")]
pub use text_capture::{
    InputAuthorizationPublisher, InputAuthorizations, input_authorization_channel,
};

// The crate denies unsafe code everywhere except this private native boundary.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod ffi;
