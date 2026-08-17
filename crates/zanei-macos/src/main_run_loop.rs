//! Safe ownership for running and stopping the process main CFRunLoop.

use thiserror::Error;

use crate::ffi::main_run_loop::{NativeMainRunLoop, NativeMainRunLoopStopper};

#[derive(Debug, Error)]
#[error("failed to install the process main CFRunLoop source")]
pub struct MainRunLoopError;

pub struct MainRunLoop {
    native: NativeMainRunLoop,
}

impl MainRunLoop {
    pub fn new() -> Result<Self, MainRunLoopError> {
        NativeMainRunLoop::new()
            .map(|native| Self { native })
            .ok_or(MainRunLoopError)
    }

    #[must_use]
    pub fn stopper(&self) -> MainRunLoopStopper {
        MainRunLoopStopper {
            native: self.native.stopper(),
        }
    }

    pub fn run(&self) {
        self.native.run();
    }
}

pub struct MainRunLoopStopper {
    native: NativeMainRunLoopStopper,
}

impl MainRunLoopStopper {
    pub fn stop(&self) {
        self.native.stop();
    }
}
