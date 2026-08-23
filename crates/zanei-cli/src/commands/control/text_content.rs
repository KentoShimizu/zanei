use std::io;
use std::path::Path;

use zanei_core::config::{CaptureBoolKey, capture_bool_is_explicit};

use super::super::config::persist_capture_text_content;
use super::super::doctor::StartPermissionState;
use crate::error::CliError;

pub(super) const PROMPT: &str = "Record typed text and clipboard contents too? They stay in the local store like everything else (48-hour retention), password fields are always excluded, and Chrome Incognito text is never captured. You can change this anytime: zanei config set capture.text_content <true|false>  [y/N] ";
const ENABLED_RESULT: &str = "Text content will be recorded.\n";
const DISABLED_RESULT: &str = "Text content stays off.\n";

pub(super) fn maybe_prompt(
    config_path: &Path,
    output_suppressed: bool,
    stdin_is_terminal: impl FnOnce() -> bool,
    stderr_is_terminal: impl FnOnce() -> bool,
    permission_state: impl FnOnce() -> Option<StartPermissionState>,
    read_answer: impl FnOnce() -> io::Result<String>,
    mut write_stderr: impl FnMut(&str) -> io::Result<()>,
) -> Result<Option<bool>, CliError> {
    if capture_bool_is_explicit(config_path, CaptureBoolKey::TextContent)?
        || output_suppressed
        || !stdin_is_terminal()
        || !stderr_is_terminal()
        || permission_state() != Some(StartPermissionState::Ready)
    {
        return Ok(None);
    }

    write_stderr(PROMPT).map_err(CliError::PromptOutput)?;
    let enabled = read_answer().is_ok_and(|answer| matches!(answer.trim(), "y" | "Y"));
    persist_capture_text_content(config_path, enabled)?;
    write_stderr(if enabled {
        ENABLED_RESULT
    } else {
        DISABLED_RESULT
    })
    .map_err(CliError::PromptOutput)?;
    Ok(Some(enabled))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;

    use tempfile::TempDir;
    use zanei_core::config::{CaptureBoolKey, Config, capture_bool_is_explicit};

    use super::*;

    #[test]
    fn undetermined_interactive_ready_start_prompts_and_persists_yes() {
        let fixture = PromptFixture::undetermined();
        let (prompted, output) = fixture.run(
            false,
            false,
            true,
            true,
            StartPermissionState::Ready,
            || Ok("y\n".to_owned()),
        );

        assert_eq!(prompted, Some(true));
        assert_eq!(output, format!("{PROMPT}{ENABLED_RESULT}"));
        assert!(
            Config::load(&fixture.config)
                .expect("saved config")
                .capture
                .text_content
        );
        assert!(
            capture_bool_is_explicit(&fixture.config, CaptureBoolKey::TextContent)
                .expect("explicit decision")
        );
        assert!(
            fs::read_to_string(&fixture.config)
                .expect("saved config source")
                .contains("# retained comment")
        );
    }

    #[test]
    fn no_and_enter_persist_false_and_prevent_a_second_prompt() {
        for answer in ["n\n", "\n"] {
            let fixture = PromptFixture::undetermined();
            let (prompted, output) = fixture.run(
                false,
                false,
                true,
                true,
                StartPermissionState::Ready,
                || Ok(answer.to_owned()),
            );
            assert_eq!(prompted, Some(false));
            assert_eq!(output, format!("{PROMPT}{DISABLED_RESULT}"));
            assert!(
                !Config::load(&fixture.config)
                    .expect("saved config")
                    .capture
                    .text_content
            );
            assert!(
                capture_bool_is_explicit(&fixture.config, CaptureBoolKey::TextContent)
                    .expect("explicit decision")
            );

            let read_again = Cell::new(false);
            let (prompted_again, output_again) = fixture.run(
                false,
                false,
                true,
                true,
                StartPermissionState::Ready,
                || {
                    read_again.set(true);
                    Ok("y\n".to_owned())
                },
            );
            assert_eq!(prompted_again, None);
            assert!(output_again.is_empty());
            assert!(!read_again.get());
        }
    }

    #[test]
    fn explicit_true_or_false_never_prompts() {
        for value in [true, false] {
            let fixture = PromptFixture::explicit(value);
            let read = Cell::new(false);
            let (prompted, output) = fixture.run(
                false,
                false,
                true,
                true,
                StartPermissionState::Ready,
                || {
                    read.set(true);
                    Ok("y\n".to_owned())
                },
            );
            assert_eq!(prompted, None);
            assert!(output.is_empty());
            assert!(!read.get());
        }
    }

    #[test]
    fn non_tty_quiet_json_and_missing_permissions_never_prompt() {
        for (quiet, json, stdin_tty, stderr_tty, permissions) in [
            (false, false, false, true, StartPermissionState::Ready),
            (false, false, true, false, StartPermissionState::Ready),
            (true, false, true, true, StartPermissionState::Ready),
            (false, true, true, true, StartPermissionState::Ready),
            (false, false, true, true, StartPermissionState::Missing),
            (
                false,
                false,
                true,
                true,
                StartPermissionState::PendingSnapshot,
            ),
        ] {
            let fixture = PromptFixture::undetermined();
            let read = Cell::new(false);
            let (prompted, output) =
                fixture.run(quiet, json, stdin_tty, stderr_tty, permissions, || {
                    read.set(true);
                    Ok("y\n".to_owned())
                });
            assert_eq!(prompted, None);
            assert!(output.is_empty());
            assert!(!read.get());
            assert!(
                !capture_bool_is_explicit(&fixture.config, CaptureBoolKey::TextContent)
                    .expect("undetermined")
            );
        }
    }

    #[test]
    fn eof_read_error_and_other_answers_choose_false() {
        let readers: [fn() -> io::Result<String>; 3] = [
            || Ok(String::new()),
            || Err(io::Error::new(io::ErrorKind::BrokenPipe, "fixture error")),
            || Ok("yes\n".to_owned()),
        ];
        for read in readers {
            let fixture = PromptFixture::undetermined();
            let (prompted, output) =
                fixture.run(false, false, true, true, StartPermissionState::Ready, read);
            assert_eq!(prompted, Some(false));
            assert_eq!(output, format!("{PROMPT}{DISABLED_RESULT}"));
            assert!(
                !Config::load(&fixture.config)
                    .expect("saved config")
                    .capture
                    .text_content
            );
        }
    }

    struct PromptFixture {
        _directory: TempDir,
        config: std::path::PathBuf,
    }

    impl PromptFixture {
        fn undetermined() -> Self {
            let directory = TempDir::new().expect("temporary directory");
            let config = directory.path().join("config.toml");
            fs::write(
                &config,
                "# retained comment\n[capture]\nsources = [\"app\"]\n",
            )
            .expect("fixture config");
            Self {
                _directory: directory,
                config,
            }
        }

        fn explicit(value: bool) -> Self {
            let fixture = Self::undetermined();
            fs::write(
                &fixture.config,
                format!("[capture]\ntext_content = {value}\n"),
            )
            .expect("explicit fixture config");
            fixture
        }

        fn run(
            &self,
            quiet: bool,
            json: bool,
            stdin_tty: bool,
            stderr_tty: bool,
            permissions: StartPermissionState,
            read: impl FnOnce() -> io::Result<String>,
        ) -> (Option<bool>, String) {
            let output = RefCell::new(String::new());
            let prompted = maybe_prompt(
                &self.config,
                quiet || json,
                || stdin_tty,
                || stderr_tty,
                || Some(permissions),
                read,
                |message| {
                    output.borrow_mut().push_str(message);
                    Ok(())
                },
            )
            .expect("prompt flow");
            (prompted, output.into_inner())
        }
    }
}
