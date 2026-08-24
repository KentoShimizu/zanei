//! macOS Accessibility roles mapped to OS-neutral snapshot behavior.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotNodeClass {
    SecureInput,
    SingleLineInput,
    MultiLineText,
    ReadableText,
    Image,
    Container,
    Unknown,
    Menu,
}

impl SnapshotNodeClass {
    #[must_use]
    pub const fn descends(self) -> bool {
        matches!(
            self,
            Self::ReadableText | Self::Image | Self::Container | Self::Unknown
        )
    }

    #[must_use]
    pub const fn reads_value(self) -> bool {
        matches!(self, Self::ReadableText)
    }

    #[must_use]
    pub const fn reads_visible_range(self) -> bool {
        matches!(self, Self::MultiLineText)
    }
}

#[must_use]
pub fn classify_role(role: Option<&str>, subrole: Option<&str>) -> SnapshotNodeClass {
    if role == Some("AXSecureTextField") || subrole == Some("AXSecureTextField") {
        return SnapshotNodeClass::SecureInput;
    }
    match role {
        Some("AXTextField" | "AXComboBox" | "AXDateField" | "AXTimeField" | "AXIncrementor") => {
            SnapshotNodeClass::SingleLineInput
        }
        Some("AXTextArea") => SnapshotNodeClass::MultiLineText,
        Some(
            "AXStaticText" | "AXHeading" | "AXLink" | "AXCell" | "AXMenuItem" | "AXButton"
            | "AXCheckBox" | "AXRadioButton" | "AXTab" | "AXPopUpButton",
        ) => SnapshotNodeClass::ReadableText,
        Some("AXImage") => SnapshotNodeClass::Image,
        Some("AXMenuBar" | "AXMenu") => SnapshotNodeClass::Menu,
        Some(
            "AXGroup" | "AXWebArea" | "AXScrollArea" | "AXList" | "AXTable" | "AXRow" | "AXOutline"
            | "AXSplitGroup" | "AXToolbar" | "AXTabGroup" | "AXSheet" | "AXPopover" | "AXWindow"
            | "AXApplication",
        ) => SnapshotNodeClass::Container,
        Some(_) | None => SnapshotNodeClass::Unknown,
    }
}
