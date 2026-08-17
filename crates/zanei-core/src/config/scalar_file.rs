use std::fs;
use std::ops::Range;
use std::path::Path;

use serde::Deserialize;
use toml::Spanned;

use super::edit::save_encoded;
use super::{Config, ConfigError, parse_config};

const CAPTURE_SECTION: &str = "capture";
const TEXT_CONTENT_KEY: &str = "text_content";

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
}

pub fn capture_text_content_is_explicit(path: impl AsRef<Path>) -> Result<bool, ConfigError> {
    let path = path.as_ref();
    let source = read_optional(path)?;
    Ok(parse_spans(&source, path)?
        .capture
        .is_some_and(|capture| capture.get_ref().text_content.is_some()))
}

/// Persists only `capture.text_content`, retaining the source document's comments and layout.
///
/// Returns whether the file changed. A missing key is a change even when the effective value was
/// already the default `false`, because explicit presence records the user's decision.
pub fn save_capture_text_content(
    config: &Config,
    path: impl AsRef<Path>,
) -> Result<bool, ConfigError> {
    config.validate()?;
    let path = path.as_ref();
    let source = read_optional(path)?;
    let spans = parse_spans(&source, path)?;
    let value = if config.capture.text_content {
        "true"
    } else {
        "false"
    };
    let edited = rewrite_text_content(source.clone(), spans.capture.as_ref(), value);
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

fn rewrite_text_content(
    mut source: String,
    capture: Option<&Spanned<CaptureSpans>>,
    value: &str,
) -> String {
    let Some(capture) = capture else {
        append_capture_section(&mut source, value);
        return source;
    };
    if let Some(current) = capture.get_ref().text_content.as_ref() {
        source.replace_range(current.span(), value);
        return source;
    }

    let span = capture.span();
    let representation = &source[span.clone()];
    if representation.starts_with('{') {
        insert_inline_value(&mut source, capture.get_ref().sources.as_ref(), span, value);
    } else if representation.starts_with('[') {
        insert_table_value(&mut source, span, value);
    } else {
        insert_dotted_value(&mut source, span, value);
    }
    source
}

fn append_capture_section(source: &mut String, value: &str) {
    if !source.is_empty() {
        if !source.ends_with('\n') {
            source.push('\n');
        }
        if !source.ends_with("\n\n") {
            source.push('\n');
        }
    }
    source.push_str(&format!(
        "[{CAPTURE_SECTION}]\n{TEXT_CONTENT_KEY} = {value}\n"
    ));
}

fn insert_inline_value(
    source: &mut String,
    sources: Option<&Spanned<toml::Value>>,
    span: Range<usize>,
    value: &str,
) {
    let closing_brace = span.end - 1;
    let inner = &source[span.start + 1..closing_brace];
    if inner.trim().is_empty() {
        source.replace_range(
            span.start + 1..closing_brace,
            &format!(" {TEXT_CONTENT_KEY} = {value} "),
        );
        return;
    }
    let insertion = sources
        .expect("a non-empty validated capture table contains sources")
        .span()
        .end;
    source.insert_str(insertion, &format!(", {TEXT_CONTENT_KEY} = {value}"));
}

fn insert_table_value(source: &mut String, span: Range<usize>, value: &str) {
    let insertion = source[span.end..]
        .find('\n')
        .map_or(source.len(), |offset| span.end + offset + 1);
    let prefix = if insertion == source.len() && !source.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    source.insert_str(
        insertion,
        &format!("{prefix}{TEXT_CONTENT_KEY} = {value}\n"),
    );
}

fn insert_dotted_value(source: &mut String, span: Range<usize>, value: &str) {
    let line_start = source[..span.start]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    source.insert_str(
        line_start,
        &format!("{CAPTURE_SECTION}.{TEXT_CONTENT_KEY} = {value}\n"),
    );
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn missing_file_records_an_explicit_default() {
        let directory = TestDirectory::new();
        let path = directory.path().join("config.toml");

        assert!(!capture_text_content_is_explicit(&path).expect("missing config presence"));
        assert!(
            save_capture_text_content(&Config::default(), &path).expect("persist explicit default")
        );
        assert!(capture_text_content_is_explicit(&path).expect("saved config presence"));
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

        assert!(save_capture_text_content(&config, &path).expect("save text content"));
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

            save_capture_text_content(&config, &path).expect("save text content");
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

        save_capture_text_content(&config, &path).expect("save text content");
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

        save_capture_text_content(&config, &path).expect("save text content");
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
