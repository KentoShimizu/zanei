//! Focused UI-element classification shared by the AX and EventTap collectors.

use zanei_core::schema::FieldKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FieldClass {
    SecureText,
    KnownText(FieldKind),
    KnownSafeNonText,
    Unknown,
}

impl FieldClass {
    #[must_use]
    pub(crate) const fn is_known_text(self) -> bool {
        matches!(self, Self::KnownText(_))
    }

    #[must_use]
    pub(crate) const fn field_kind(self) -> Option<FieldKind> {
        match self {
            Self::KnownText(field_kind) => Some(field_kind),
            Self::SecureText | Self::KnownSafeNonText | Self::Unknown => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FocusedField {
    pub(crate) generation: u64,
    pub(crate) class: FieldClass,
}

impl FocusedField {
    #[must_use]
    pub(crate) const fn field_kind(self) -> Option<FieldKind> {
        self.class.field_kind()
    }
}

#[must_use]
pub(crate) fn field_class(role: Option<&str>, subrole: Option<&str>) -> FieldClass {
    if matches!(role, Some("AXSecureTextField")) || matches!(subrole, Some("AXSecureTextField")) {
        return FieldClass::SecureText;
    }

    match (role, subrole) {
        (Some("AXTextField"), Some("AXSearchField")) => FieldClass::KnownText(FieldKind::Search),
        (Some("AXTextField" | "AXTextArea"), _) => FieldClass::KnownText(FieldKind::Text),
        (Some("AXIncrementor"), _) => FieldClass::KnownText(FieldKind::Number),
        (Some("AXComboBox" | "AXDateField" | "AXTimeField"), _) => {
            FieldClass::KnownText(FieldKind::Other)
        }
        (
            Some(
                "AXButton" | "AXCheckBox" | "AXRadioButton" | "AXSlider" | "AXPopUpButton"
                | "AXMenuItem" | "AXTab" | "AXStaticText",
            ),
            _,
        ) => FieldClass::KnownSafeNonText,
        _ => FieldClass::Unknown,
    }
}

#[must_use]
pub(crate) fn observed_field_class(
    role: Option<&str>,
    subrole: Option<&str>,
    secure_input: bool,
) -> FieldClass {
    if secure_input {
        FieldClass::SecureText
    } else {
        field_class(role, subrole)
    }
}

#[cfg(test)]
mod tests {
    use zanei_core::{
        schema::{
            App, Event, EventData, FieldKind, InputKeyData, InputKeyKind, Redaction, Window,
            event_schema_version,
        },
        sink::{Sink, StreamSink},
    };

    use super::{FieldClass, field_class, observed_field_class};

    #[test]
    fn classifies_known_text_roles_and_subroles() {
        let cases = [
            (
                Some("AXTextField"),
                Some("AXSearchField"),
                FieldKind::Search,
            ),
            (Some("AXTextField"), None, FieldKind::Text),
            (Some("AXTextArea"), None, FieldKind::Text),
            (Some("AXIncrementor"), None, FieldKind::Number),
            (Some("AXComboBox"), None, FieldKind::Other),
            (Some("AXDateField"), None, FieldKind::Other),
            (Some("AXTimeField"), None, FieldKind::Other),
        ];

        for (role, subrole, expected) in cases {
            assert_eq!(field_class(role, subrole), FieldClass::KnownText(expected));
        }
    }

    #[test]
    fn classifies_secure_text_role_and_subrole() {
        assert_eq!(
            field_class(Some("AXSecureTextField"), None),
            FieldClass::SecureText
        );
        assert_eq!(
            field_class(Some("AXTextField"), Some("AXSecureTextField")),
            FieldClass::SecureText
        );
    }

    #[test]
    fn secure_input_overrides_a_known_text_role() {
        assert_eq!(
            observed_field_class(Some("AXTextField"), None, true),
            FieldClass::SecureText
        );
    }

    #[test]
    fn classifies_every_known_safe_non_text_role() {
        let roles = [
            "AXButton",
            "AXCheckBox",
            "AXRadioButton",
            "AXSlider",
            "AXPopUpButton",
            "AXMenuItem",
            "AXTab",
            "AXStaticText",
        ];

        for role in roles {
            assert_eq!(field_class(Some(role), None), FieldClass::KnownSafeNonText);
        }
    }

    #[test]
    fn classifies_unknown_or_missing_roles_as_unknown() {
        assert_eq!(field_class(Some("AXDocument"), None), FieldClass::Unknown);
        assert_eq!(
            field_class(None, Some("AXSearchField")),
            FieldClass::Unknown
        );
        assert_eq!(field_class(None, None), FieldClass::Unknown);
    }

    #[test]
    fn field_kind_is_exposed_only_for_known_text() {
        assert_eq!(
            FieldClass::KnownText(FieldKind::Search).field_kind(),
            Some(FieldKind::Search)
        );
        assert_eq!(FieldClass::SecureText.field_kind(), None);
        assert_eq!(FieldClass::KnownSafeNonText.field_kind(), None);
        assert_eq!(FieldClass::Unknown.field_kind(), None);
    }

    #[test]
    fn field_kind_json_values_match_the_public_schema_enum() {
        let cases = [
            (FieldKind::Text, "text"),
            (FieldKind::Search, "search"),
            (FieldKind::Url, "url"),
            (FieldKind::Email, "email"),
            (FieldKind::Number, "number"),
            (FieldKind::Other, "other"),
        ];

        for (field_kind, expected) in cases {
            let event = Event {
                version: event_schema_version("input.key").expect("input.key schema version"),
                id: "evt_00000000000000000000000000".to_owned(),
                ts: "1970-01-01T00:00:00Z".to_owned(),
                mono_ns: 0,
                source: "macos.eventtap".to_owned(),
                event_type: "input.key".to_owned(),
                app: App {
                    name: "Test".to_owned(),
                    bundle_id: Some("dev.zanei.test".to_owned()),
                    pid: Some(41),
                },
                window: Some(Window {
                    title: Some("Test".to_owned()),
                    id: Some(1),
                }),
                element: None,
                data: EventData::InputKey(InputKeyData {
                    kind: InputKeyKind::Text,
                    modifiers: Vec::new(),
                    count: 1,
                    combo: None,
                    text: None,
                    field_kind: Some(field_kind),
                }),
                redaction: Redaction {
                    applied: false,
                    rules: Vec::new(),
                },
            };
            let mut sink = StreamSink::new(Vec::new());
            sink.write(&event).expect("event should serialize");
            let json = String::from_utf8(sink.into_inner()).expect("JSON should be UTF-8");

            assert!(json.contains(&format!(r#""field_kind":"{expected}""#)));
        }
    }
}
