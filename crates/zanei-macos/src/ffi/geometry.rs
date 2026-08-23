//! Shared macOS Accessibility and CoreGraphics geometry shapes.

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxFrame {
    pub origin: AxPoint,
    pub size: AxSize,
}
