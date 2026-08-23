//! Native AX observation and error shapes.

use std::fmt;

pub(super) const AX_ERROR_ATTRIBUTE_UNSUPPORTED: i32 = -25_205;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeWindow {
    pub title: Option<String>,
    pub id: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeElement {
    pub(crate) role: Option<String>,
    pub(crate) subrole: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) value_len: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NativeAxEvent {
    WindowFocused {
        pid: i32,
        window: NativeWindow,
    },
    WindowTitleChanged {
        pid: i32,
        window: NativeWindow,
    },
    UiFocused {
        pid: i32,
        generation: u64,
        window: Option<NativeWindow>,
        element: Option<NativeElement>,
    },
    UiValueChanged {
        pid: i32,
        window: Option<NativeWindow>,
        element: NativeElement,
        text: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeHitTest {
    pub(crate) pid: i32,
    pub(crate) window: Option<NativeWindow>,
    pub(crate) element: NativeElement,
}

#[derive(Debug)]
pub(crate) struct NativeAxError {
    pub(super) operation: &'static str,
    pub(super) code: i32,
}

impl fmt::Display for NativeAxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failed with AXError {}",
            self.operation, self.code
        )
    }
}

impl NativeAxError {
    pub(super) const fn operation(&self) -> &'static str {
        self.operation
    }

    pub(super) const fn code(&self) -> i32 {
        self.code
    }

    pub(super) const fn is_attribute_unsupported(&self) -> bool {
        self.code == AX_ERROR_ATTRIBUTE_UNSUPPORTED
    }
}
