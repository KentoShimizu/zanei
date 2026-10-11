//! Unsafe macOS bindings hidden behind owned, typed wrappers.

pub(crate) mod activity;
pub(crate) mod app_directory;
pub(crate) mod applescript;
pub(crate) mod ax;
pub(crate) mod eventtap;
pub(crate) mod geometry;
pub(crate) mod input_source;
pub(crate) mod keychain;
pub(crate) mod main_run_loop;
pub(crate) mod permission;
#[expect(
    dead_code,
    reason = "consumed by the OCR engine and the daemon permission sequence"
)]
pub(crate) mod screen_capture;
pub(crate) mod secure_input;
#[cfg_attr(not(test), expect(dead_code, reason = "consumed by the OCR engine"))]
pub(crate) mod text_recognition;
pub(crate) mod window_list;
pub(crate) mod workspace;
