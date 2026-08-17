#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventTapMode {
    InputOnly { capture_text_content: bool },
    ClickOnly,
    InputAndClicks { capture_text_content: bool },
}

impl EventTapMode {
    pub(crate) const fn captures_input(self) -> bool {
        matches!(self, Self::InputOnly { .. } | Self::InputAndClicks { .. })
    }

    pub(crate) const fn captures_clicks(self) -> bool {
        matches!(self, Self::ClickOnly | Self::InputAndClicks { .. })
    }

    pub(crate) const fn captures_text_content(self) -> bool {
        match self {
            Self::InputOnly {
                capture_text_content,
            }
            | Self::InputAndClicks {
                capture_text_content,
            } => capture_text_content,
            Self::ClickOnly => false,
        }
    }
}
