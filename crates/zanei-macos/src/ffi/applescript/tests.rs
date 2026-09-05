use super::*;

#[test]
fn front_window_source_leaves_frontmost_authority_to_focus_context() {
    let source = front_window_source(BrowserTarget::Chrome, "/Applications/Google Chrome.app");

    assert!(!source.contains("frontmost is false"));
    assert!(source.contains("set current_window to front window"));
}

#[test]
fn sources_bind_chrome_to_its_bundle_id_and_preserve_status_paths() {
    let application_path = "/Applications/Google Chrome.app";
    let window_id = AppleScriptWindowId::for_test("window-4321");
    let front = front_window_source(BrowserTarget::Chrome, application_path);
    let target = target_window_source(BrowserTarget::Chrome, application_path, &window_id);

    for source in [&front, &target] {
        assert!(source.contains(
            "if not (running of application id \"com.google.Chrome\") then return {\"not_running\"}"
        ));
        assert!(source.contains("tell application id \"com.google.Chrome\""));
        assert!(!source.contains("path to application id"));
        assert!(!source.contains("application chromeApp"));
        assert!(source.contains("if current_mode is \"incognito\" then return {\"incognito\"}"));
        assert!(source.contains(
            "if current_mode is not \"normal\" then return {\"unsupported_mode\", current_mode}"
        ));
        assert!(source.contains(
            "return {\"snapshot\", (id of current_window) as text, name of current_window, (id of current_tab) as text, URL of current_tab, title of current_tab}"
        ));
    }
    assert!(front.contains("if (count of windows) is 0 then return {\"no_window\"}"));
    assert!(
        target.contains("if not (exists window id \"window-4321\") then return {\"no_window\"}")
    );
}

#[test]
fn safari_source_uses_safari_bundle_and_does_not_claim_privacy_or_tab_identity() {
    let application_path = "/Applications/Safari.app";
    let window_id = AppleScriptWindowId::for_test("window-4321");
    let front = front_window_source(BrowserTarget::Safari, application_path);
    let target = target_window_source(BrowserTarget::Safari, application_path, &window_id);

    for source in [&front, &target] {
        assert!(source.contains("application id \"com.apple.Safari\""));
        assert!(source.contains("current tab of current_window"));
        assert!(source.contains(
            "return {\"snapshot\", (id of current_window) as text, name of current_window, URL of current_tab, name of current_tab}"
        ));
        assert!(!source.contains("mode of current_window"));
        assert!(!source.contains("id of current_tab"));
    }
    assert!(target.contains("set current_window to window id \"window-4321\""));
}

#[test]
fn targeted_source_escapes_opaque_window_identity_as_one_string_literal() {
    let window_id = AppleScriptWindowId::for_test("window-\\\" & return {\"private\"} & \"");

    let source = target_window_source(
        BrowserTarget::Chrome,
        "/Applications/Google Chrome.app",
        &window_id,
    );

    let escaped = "window-\\\\\\\" & return {\\\"private\\\"} & \\\"";
    assert_eq!(source.matches(escaped).count(), 2);
    assert!(!source.contains("window id \"window-\" & return"));
}

#[test]
fn targeted_source_does_not_reinterpret_markers_in_values() {
    let application_path = "/Applications/{window_id}/Google Chrome.app";
    let window_id = AppleScriptWindowId::for_test("window-{application_path}");

    let source = target_window_source(BrowserTarget::Chrome, application_path, &window_id);

    assert_eq!(source.matches(application_path).count(), 1);
    assert_eq!(source.matches(window_id.as_str()).count(), 2);
}

#[test]
fn decode_distinguishes_chrome_contract_from_safari_unknowns() {
    let chrome = parse_items(
        BrowserTarget::Chrome,
        vec![
            Some("snapshot".into()),
            Some("chrome-window".into()),
            None,
            Some("tab-1".into()),
            Some("https://example.com".into()),
            None,
        ],
    )
    .expect("Chrome snapshot");
    assert!(
        matches!(chrome, Observation::ChromeSnapshot(Snapshot { ref tab_key, .. }) if tab_key == "tab-1")
    );

    let safari = parse_items(
        BrowserTarget::Safari,
        vec![
            Some("snapshot".into()),
            Some("safari-window".into()),
            None,
            None,
            None,
        ],
    )
    .expect("Safari snapshot");
    assert_eq!(
        safari,
        Observation::SafariSnapshot(SafariSnapshot {
            window_id: AppleScriptWindowId::for_test("safari-window"),
            window_title: None,
            url: None,
            tab_title: None,
        })
    );
}

#[test]
fn decode_keeps_no_window_private_and_malformed_distinct() {
    assert_eq!(
        parse_items(BrowserTarget::Safari, vec![Some("no_window".into())]).unwrap(),
        Observation::NoWindow
    );
    assert_eq!(
        parse_items(BrowserTarget::Chrome, vec![Some("incognito".into())]).unwrap(),
        Observation::Incognito
    );
    assert!(matches!(
        parse_items(
            BrowserTarget::Chrome,
            vec![Some("snapshot".into()), Some("w".into())]
        ),
        Err(AppleScriptError::InvalidResponse(
            AppleScriptResponseError::SnapshotLength
        ))
    ));
    assert!(matches!(
        parse_items(
            BrowserTarget::Chrome,
            vec![Some("unsupported_mode".into()), Some("x".into())]
        ),
        Err(AppleScriptError::UnsupportedMode)
    ));
}

#[test]
fn chrome_requires_url_and_tab_but_safari_allows_missing_url() {
    assert!(matches!(
        parse_items(
            BrowserTarget::Chrome,
            vec![
                Some("snapshot".into()),
                Some("window".into()),
                None,
                Some("tab".into()),
                None,
                None,
            ],
        ),
        Err(AppleScriptError::InvalidResponse(
            AppleScriptResponseError::RequiredItemNotText
        ))
    ));
    let result = parse_items(
        BrowserTarget::Safari,
        vec![
            Some("snapshot".into()),
            Some("window".into()),
            None,
            None,
            Some("title".into()),
        ],
    )
    .expect("Safari URL may be absent");
    assert!(matches!(
        result,
        Observation::SafariSnapshot(SafariSnapshot { url: None, .. })
    ));
}

#[test]
fn safari_does_not_decode_chrome_private_status() {
    assert!(matches!(
        parse_items(BrowserTarget::Safari, vec![Some("incognito".into())]),
        Err(AppleScriptError::InvalidResponse(
            AppleScriptResponseError::UnknownStatus
        ))
    ));
}
