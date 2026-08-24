use std::fs;
use std::ops::Range;
use std::path::Path;

use serde::Deserialize;
use toml::Spanned;

use super::edit::save_encoded;
use super::{Config, ConfigError, parse_config};

const CAPTURE_SECTION: &str = "capture";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureBoolKey {
    TextContent,
    ContentSnapshot,
}

impl CaptureBoolKey {
    const fn name(self) -> &'static str {
        match self {
            Self::TextContent => "text_content",
            Self::ContentSnapshot => "content_snapshot",
        }
    }

    const fn value(self, config: &Config) -> bool {
        match self {
            Self::TextContent => config.capture.text_content,
            Self::ContentSnapshot => config.capture.content_snapshot,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ConfigSpans {
    capture: Option<Spanned<CaptureSpans>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CaptureSpans {
    sources: Option<Spanned<toml::Value>>,
    text_content: Option<Spanned<bool>>,
    content_snapshot: Option<Spanned<bool>>,
}

impl CaptureSpans {
    fn value(&self, key: CaptureBoolKey) -> Option<&Spanned<bool>> {
        match key {
            CaptureBoolKey::TextContent => self.text_content.as_ref(),
            CaptureBoolKey::ContentSnapshot => self.content_snapshot.as_ref(),
        }
    }

    fn last_value_end(&self) -> Option<usize> {
        self.sources
            .as_ref()
            .map(Spanned::span)
            .into_iter()
            .chain(self.text_content.as_ref().map(Spanned::span))
            .chain(self.content_snapshot.as_ref().map(Spanned::span))
            .map(|span| span.end)
            .max()
    }
}

pub fn capture_bool_is_explicit(
    path: impl AsRef<Path>,
    key: CaptureBoolKey,
) -> Result<bool, ConfigError> {
    let path = path.as_ref();
    let source = read_optional(path)?;
    Ok(parse_spans(&source, path)?
        .capture
        .is_some_and(|capture| capture.get_ref().value(key).is_some()))
}

/// Persists one capture boolean while retaining comments and layout.
///
/// Returns whether the file changed. A missing key is a change even when the effective value was
/// already the default `false`, because explicit presence records the user's decision.
pub fn save_capture_bool(
    config: &Config,
    path: impl AsRef<Path>,
    key: CaptureBoolKey,
) -> Result<bool, ConfigError> {
    config.validate()?;
    let path = path.as_ref();
    let source = read_optional(path)?;
    let spans = parse_spans(&source, path)?;
    let value = if key.value(config) { "true" } else { "false" };
    let edited = rewrite_capture_bool(source.clone(), spans.capture.as_ref(), key, value);
    parse_config(&edited, path)?;
    if edited == source {
        return Ok(false);
    }
    save_encoded(&edited, path)?;
    Ok(true)
}

fn read_optional(path: &Path) -> Result<String, ConfigError> {
    match fs::read_to_string(path) {
        Ok(source) => Ok(source),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn parse_spans(source: &str, path: &Path) -> Result<ConfigSpans, ConfigError> {
    toml::from_str(source).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn rewrite_capture_bool(
    mut source: String,
    capture: Option<&Spanned<CaptureSpans>>,
    key: CaptureBoolKey,
    value: &str,
) -> String {
    let Some(capture) = capture else {
        append_capture_section(&mut source, key, value);
        return source;
    };
    if let Some(current) = capture.get_ref().value(key) {
        source.replace_range(current.span(), value);
        return source;
    }

    let span = capture.span();
    let representation = &source[span.clone()];
    if representation.starts_with('{') {
        insert_inline_value(&mut source, capture.get_ref(), span, key, value);
    } else if representation.starts_with('[') {
        insert_table_value(&mut source, span, key, value);
    } else {
        insert_dotted_value(&mut source, span, key, value);
    }
    source
}

fn append_capture_section(source: &mut String, key: CaptureBoolKey, value: &str) {
    if !source.is_empty() {
        if !source.ends_with('\n') {
            source.push('\n');
        }
        if !source.ends_with("\n\n") {
            source.push('\n');
        }
    }
    source.push_str(&format!("[{CAPTURE_SECTION}]\n{} = {value}\n", key.name()));
}

fn insert_inline_value(
    source: &mut String,
    capture: &CaptureSpans,
    span: Range<usize>,
    key: CaptureBoolKey,
    value: &str,
) {
    let closing_brace = span.end - 1;
    let inner = &source[span.start + 1..closing_brace];
    if inner.trim().is_empty() {
        source.replace_range(
            span.start + 1..closing_brace,
            &format!(" {} = {value} ", key.name()),
        );
        return;
    }
    let insertion = capture.last_value_end().unwrap_or(closing_brace);
    source.insert_str(insertion, &format!(", {} = {value}", key.name()));
}

fn insert_table_value(source: &mut String, span: Range<usize>, key: CaptureBoolKey, value: &str) {
    let insertion = source[span.end..]
        .find('\n')
        .map_or(source.len(), |offset| span.end + offset + 1);
    let prefix = if insertion == source.len() && !source.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    source.insert_str(insertion, &format!("{prefix}{} = {value}\n", key.name()));
}

fn insert_dotted_value(source: &mut String, span: Range<usize>, key: CaptureBoolKey, value: &str) {
    let line_start = source[..span.start]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    source.insert_str(
        line_start,
        &format!("{CAPTURE_SECTION}.{} = {value}\n", key.name()),
    );
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    const TEXT_CONTENT: CaptureBoolKey = CaptureBoolKey::TextContent;

    #[test]
    fn missing_file_records_an_explicit_default() {
        let directory = TestDirectory::new();
        let path = directory.path().join("config.toml");

        assert!(!capture_bool_is_explicit(&path, TEXT_CONTENT).expect("missing config presence"));
        assert!(
            save_capture_bool(&Config::default(), &path, TEXT_CONTENT)
                .expect("persist explicit default")
        );
        assert!(capture_bool_is_explicit(&path, TEXT_CONTENT).expect("saved config presence"));
        assert_eq!(
            fs::read_to_string(path).expect("saved config"),
            "[capture]\ntext_content = false\n"
        );
    }

    #[test]
    fn existing_table_comments_and_unrelated_layout_are_preserved() {
        let directory = TestDirectory::new();
        let path = directory.path().join("config.toml");
        let original = concat!(
            "# retained root comment\n",
            "[capture] # retained section comment\n",
            "sources = [\"app\"] # retained value comment\n",
            "\n",
            "[output]\n",
            "retention_hours = 72\n",
        );
        fs::write(&path, original).expect("test config");
        let mut config = Config::load(&path).expect("load test config");
        config.capture.text_content = true;

        assert!(save_capture_bool(&config, &path, TEXT_CONTENT).expect("save text content"));
        assert_eq!(
            fs::read_to_string(path).expect("edited config"),
            concat!(
                "# retained root comment\n",
                "[capture] # retained section comment\n",
                "text_content = true\n",
                "sources = [\"app\"] # retained value comment\n",
                "\n",
                "[output]\n",
                "retention_hours = 72\n",
            )
        );
    }

    #[test]
    fn inline_and_dotted_capture_tables_remain_valid() {
        for (source, expected) in [
            (
                "capture = { sources = [\"app\"] }\n",
                "capture = { sources = [\"app\"], text_content = true }\n",
            ),
            (
                "capture.sources = [\"app\"]\n",
                "capture.text_content = true\ncapture.sources = [\"app\"]\n",
            ),
        ] {
            let directory = TestDirectory::new();
            let path = directory.path().join("config.toml");
            fs::write(&path, source).expect("test config");
            let mut config = Config::load(&path).expect("load test config");
            config.capture.text_content = true;

            save_capture_bool(&config, &path, TEXT_CONTENT).expect("save text content");
            assert_eq!(fs::read_to_string(path).expect("edited config"), expected);
        }
    }

    #[test]
    fn multiline_inline_table_comments_are_preserved() {
        let directory = TestDirectory::new();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            concat!(
                "capture = {\n",
                "  sources = [\"app\"] # retained inline comment\n",
                "}\n",
            ),
        )
        .expect("test config");
        let mut config = Config::load(&path).expect("load test config");
        config.capture.text_content = true;

        save_capture_bool(&config, &path, TEXT_CONTENT).expect("save text content");
        assert_eq!(
            fs::read_to_string(path).expect("edited config"),
            concat!(
                "capture = {\n",
                "  sources = [\"app\"], text_content = true # retained inline comment\n",
                "}\n",
            )
        );
    }

    #[test]
    fn replacing_an_explicit_value_keeps_its_inline_comment() {
        let directory = TestDirectory::new();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "[capture]\ntext_content = false # retained decision comment\n",
        )
        .expect("test config");
        let mut config = Config::load(&path).expect("load test config");
        config.capture.text_content = true;

        save_capture_bool(&config, &path, TEXT_CONTENT).expect("save text content");
        assert_eq!(
            fs::read_to_string(path).expect("edited config"),
            "[capture]\ntext_content = true # retained decision comment\n"
        );
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "zanei-scalar-file-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("temporary directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove temporary directory");
        }
    }
}
