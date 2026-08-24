use crate::content_snapshot::{SnapshotNodeClass, classify_role};

#[test]
fn every_documented_ax_role_maps_to_the_neutral_class() {
    let cases = [
        ("AXSecureTextField", SnapshotNodeClass::SecureInput),
        ("AXTextField", SnapshotNodeClass::SingleLineInput),
        ("AXComboBox", SnapshotNodeClass::SingleLineInput),
        ("AXDateField", SnapshotNodeClass::SingleLineInput),
        ("AXTimeField", SnapshotNodeClass::SingleLineInput),
        ("AXIncrementor", SnapshotNodeClass::SingleLineInput),
        ("AXTextArea", SnapshotNodeClass::MultiLineText),
        ("AXStaticText", SnapshotNodeClass::ReadableText),
        ("AXHeading", SnapshotNodeClass::ReadableText),
        ("AXLink", SnapshotNodeClass::ReadableText),
        ("AXCell", SnapshotNodeClass::ReadableText),
        ("AXMenuItem", SnapshotNodeClass::ReadableText),
        ("AXButton", SnapshotNodeClass::ReadableText),
        ("AXCheckBox", SnapshotNodeClass::ReadableText),
        ("AXRadioButton", SnapshotNodeClass::ReadableText),
        ("AXTab", SnapshotNodeClass::ReadableText),
        ("AXPopUpButton", SnapshotNodeClass::ReadableText),
        ("AXImage", SnapshotNodeClass::Image),
        ("AXGroup", SnapshotNodeClass::Container),
        ("AXWebArea", SnapshotNodeClass::Container),
        ("AXScrollArea", SnapshotNodeClass::Container),
        ("AXList", SnapshotNodeClass::Container),
        ("AXTable", SnapshotNodeClass::Container),
        ("AXRow", SnapshotNodeClass::Container),
        ("AXOutline", SnapshotNodeClass::Container),
        ("AXSplitGroup", SnapshotNodeClass::Container),
        ("AXToolbar", SnapshotNodeClass::Container),
        ("AXTabGroup", SnapshotNodeClass::Container),
        ("AXSheet", SnapshotNodeClass::Container),
        ("AXPopover", SnapshotNodeClass::Container),
        ("AXMenuBar", SnapshotNodeClass::Menu),
        ("AXMenu", SnapshotNodeClass::Menu),
    ];
    for (role, expected) in cases {
        assert_eq!(classify_role(Some(role), None), expected, "{role}");
    }
}

#[test]
fn secure_subrole_wins_and_unknown_roles_fail_closed() {
    assert_eq!(
        classify_role(Some("AXGroup"), Some("AXSecureTextField")),
        SnapshotNodeClass::SecureInput
    );
    assert_eq!(
        classify_role(Some("AXFutureEditableControl"), None),
        SnapshotNodeClass::Unknown
    );
    assert_eq!(classify_role(None, None), SnapshotNodeClass::Unknown);
    assert!(!SnapshotNodeClass::SecureInput.descends());
    assert!(!SnapshotNodeClass::Menu.descends());
    assert!(!SnapshotNodeClass::SingleLineInput.reads_value());
    assert!(!SnapshotNodeClass::Unknown.reads_value());
    assert!(SnapshotNodeClass::MultiLineText.reads_visible_range());
}
