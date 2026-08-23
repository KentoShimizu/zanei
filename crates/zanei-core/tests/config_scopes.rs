use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use zanei_core::config::{
    CaptureBoolKey, Config, FilterEdit, FilterList, FilterScope,
    PRIVATE_WINDOW_UNDETECTABLE_BROWSER_BUNDLE_IDS, apply_scalar_edit, capture_bool_is_explicit,
    edit_filter, save_capture_bool,
};

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn defaults_and_serialized_shape_cover_all_eighteen_options() {
    let config = Config::default();
    assert!(!config.capture.text_content);
    assert!(!config.capture.content_snapshot);
    let expected = PRIVATE_WINDOW_UNDETECTABLE_BROWSER_BUNDLE_IDS.map(str::to_owned);
    assert_eq!(config.filter.text_content.exclude_apps, expected);
    assert_eq!(config.filter.content_snapshot.exclude_apps, expected);

    let encoded = toml::to_string_pretty(&config).expect("serialize defaults");
    let value: toml::Value = toml::from_str(&encoded).expect("parse serialized defaults");
    assert_eq!(leaf_count(&value), 18, "{encoded}");
    assert!(encoded.find("[filter]").unwrap() < encoded.find("[filter.text_content]").unwrap());
    assert!(
        encoded.find("[filter.text_content]").unwrap()
            < encoded.find("[filter.content_snapshot]").unwrap()
    );
    assert!(encoded.find("[filter.content_snapshot]").unwrap() < encoded.find("[output]").unwrap());
}

#[test]
fn all_eight_scoped_lists_use_existing_validation_rules() {
    for (scope, list) in [
        ("filter.text_content", "exclude_apps"),
        ("filter.text_content", "include_only_apps"),
        ("filter.content_snapshot", "exclude_apps"),
        ("filter.content_snapshot", "include_only_apps"),
    ] {
        let invalid = format!("[{scope}]\n{list} = [\" app \" ]");
        assert!(Config::from_toml(&invalid).is_err(), "{scope}.{list}");
    }
    for (scope, list) in [
        ("filter.text_content", "exclude_websites"),
        ("filter.text_content", "include_only_websites"),
        ("filter.content_snapshot", "exclude_websites"),
        ("filter.content_snapshot", "include_only_websites"),
    ] {
        let invalid = format!("[{scope}]\n{list} = [\"https://example.com\" ]");
        assert!(Config::from_toml(&invalid).is_err(), "{scope}.{list}");
    }
}

#[test]
fn capture_boole_share_comment_preserving_scalar_persistence() {
    let directory = TestDirectory::new("capture-bools");
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        "# root\n[capture] # section\ntext_content = false # text\nsources = [\"app\"]\n",
    )
    .expect("write fixture");
    let config = Config::load(&path).expect("load fixture");
    let edited =
        apply_scalar_edit(&config, "capture.content_snapshot", "true").expect("edit snapshot bool");
    assert!(edited.restart_recording);
    assert!(
        save_capture_bool(&edited.config, &path, CaptureBoolKey::ContentSnapshot,)
            .expect("save snapshot bool")
    );

    let source = std::fs::read_to_string(&path).expect("read edited fixture");
    assert!(source.contains("# root"));
    assert!(source.contains("text_content = false # text"));
    assert!(source.contains("content_snapshot = true"));
    assert!(capture_bool_is_explicit(&path, CaptureBoolKey::TextContent).expect("text presence"));
    assert!(
        capture_bool_is_explicit(&path, CaptureBoolKey::ContentSnapshot)
            .expect("snapshot presence")
    );
}

#[test]
fn filter_scope_selects_one_nested_list_without_touching_global_state() {
    let directory = TestDirectory::new("filter-scope");
    let path = directory.path().join("config.toml");
    let result = edit_filter(
        &path,
        FilterScope::TextContent,
        FilterList::IncludeOnlyApps,
        FilterEdit::Add,
        "dev.example.Editor",
    )
    .expect("edit nested scope");

    assert_eq!(
        result.config.filter.text_content.include_only_apps,
        ["dev.example.Editor"]
    );
    assert!(result.config.filter.include_only_apps.is_empty());
    assert!(
        result
            .config
            .filter
            .content_snapshot
            .include_only_apps
            .is_empty()
    );
}

fn leaf_count(value: &toml::Value) -> usize {
    match value {
        toml::Value::Table(table) => table.values().map(leaf_count).sum(),
        _ => 1,
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "zanei-config-scopes-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove test directory");
    }
}
