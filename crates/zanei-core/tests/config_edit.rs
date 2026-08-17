use zanei_core::config::{Config, ConfigSetError, apply_scalar_edit};

#[test]
fn scalar_edits_cover_every_supported_key() {
    let cases = [
        ("capture.text_content", "true"),
        ("output.batch_interval_s", "30"),
        ("output.retention_hours", "48"),
    ];
    let config = cases
        .into_iter()
        .try_fold(Config::default(), |config, (key, value)| {
            apply_scalar_edit(&config, key, value).map(|result| result.config)
        })
        .expect("all documented scalar values should be accepted");

    assert!(config.capture.text_content);
    assert_eq!(config.output.batch_interval_s, 30);
    assert_eq!(config.output.retention_hours, 48);
}

#[test]
fn scalar_edits_reject_unknown_array_and_invalid_values() {
    let config = Config::default();

    assert!(matches!(
        apply_scalar_edit(&config, "capture.unknown", "true"),
        Err(ConfigSetError::UnknownKey(_))
    ));
    for removed_key in ["output.mode", "output.store"] {
        assert!(matches!(
            apply_scalar_edit(&config, removed_key, "legacy"),
            Err(ConfigSetError::UnknownKey(_))
        ));
    }
    assert!(matches!(
        apply_scalar_edit(&config, "capture.sources", "app"),
        Err(ConfigSetError::ArrayKey(_))
    ));
    assert!(matches!(
        apply_scalar_edit(&config, "capture.text_content", "yes"),
        Err(ConfigSetError::InvalidValue { .. })
    ));
    assert!(matches!(
        apply_scalar_edit(&config, "output.retention_hours", "0"),
        Err(ConfigSetError::Validation { .. })
    ));
}

#[test]
fn unchanged_scalar_value_is_reported_without_mutation() {
    let config = Config::default();
    let result = apply_scalar_edit(&config, "capture.text_content", "false")
        .expect("current scalar value should remain valid");

    assert!(!result.changed);
    assert_eq!(result.config, config);
}
