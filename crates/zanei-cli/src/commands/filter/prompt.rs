use std::io::{self, IsTerminal, Write};

use super::super::apps::AppCandidate;
use crate::error::CliError;

pub(super) fn choose_app(
    candidates: &[AppCandidate],
    quiet: bool,
) -> Result<AppCandidate, CliError> {
    let stdin_is_terminal = io::stdin().is_terminal();
    let stderr_is_terminal = io::stderr().is_terminal();
    let mut stderr = io::stderr().lock();
    choose_app_with(
        candidates,
        quiet,
        stdin_is_terminal,
        stderr_is_terminal,
        || {
            let mut answer = String::new();
            io::stdin().read_line(&mut answer).map(|_| answer)
        },
        |message| {
            stderr.write_all(message.as_bytes())?;
            stderr.flush()
        },
    )
}

fn choose_app_with(
    candidates: &[AppCandidate],
    quiet: bool,
    stdin_is_terminal: bool,
    stderr_is_terminal: bool,
    mut read_answer: impl FnMut() -> io::Result<String>,
    mut write_stderr: impl FnMut(&str) -> io::Result<()>,
) -> Result<AppCandidate, CliError> {
    if quiet || !stdin_is_terminal || !stderr_is_terminal {
        return Err(CliError::InvalidValue(
            "an app value is required with non-TTY input or --quiet".to_owned(),
        ));
    }
    let mut visible: Vec<_> = candidates.iter().collect();
    loop {
        if visible.is_empty() {
            return Err(CliError::InvalidValue(
                "no apps are available for selection".to_owned(),
            ));
        }
        write_stderr(&render_choices(&visible)).map_err(CliError::PromptOutput)?;
        let answer = read_answer().map_err(CliError::Input)?;
        let answer = answer.trim();
        if answer.is_empty() {
            return Err(CliError::InvalidValue(
                "an app selection is required".to_owned(),
            ));
        }
        if let Ok(number) = answer.parse::<usize>() {
            if let Some(candidate) = visible.get(number.saturating_sub(1)) {
                return Ok((*candidate).clone());
            }
            write_stderr("Invalid selection number.\n").map_err(CliError::PromptOutput)?;
            continue;
        }
        visible = candidates
            .iter()
            .filter(|candidate| candidate.matches(answer))
            .collect();
        if let [candidate] = visible.as_slice() {
            return Ok((*candidate).clone());
        }
        if visible.is_empty() {
            write_stderr(&format!("No apps match \"{answer}\".\n"))
                .map_err(CliError::PromptOutput)?;
            visible = candidates.iter().collect();
        }
    }
}

fn render_choices(candidates: &[&AppCandidate]) -> String {
    let mut output = String::from("Choose an app (number or filter text):\n");
    for (index, candidate) in candidates.iter().enumerate() {
        output.push_str(&format!("  {}. {}\n", index + 1, candidate.display()));
    }
    output.push_str("> ");
    output
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn non_tty_and_quiet_require_a_value() {
        let candidates = [fixture("Terminal")];
        for (quiet, stdin_tty, stderr_tty) in [
            (false, false, true),
            (false, true, false),
            (true, true, true),
        ] {
            assert!(matches!(
                choose_app_with(
                    &candidates,
                    quiet,
                    stdin_tty,
                    stderr_tty,
                    || Ok("1\n".to_owned()),
                    |_| Ok(())
                ),
                Err(CliError::InvalidValue(_))
            ));
        }
    }

    #[test]
    fn accepts_filter_text_when_one_app_remains() {
        let candidates = [fixture("Terminal"), fixture("TextEdit")];
        let output = RefCell::new(String::new());
        let selected = choose_app_with(
            &candidates,
            false,
            true,
            true,
            || Ok("term\n".to_owned()),
            |message| {
                output.borrow_mut().push_str(message);
                Ok(())
            },
        )
        .expect("interactive selection");
        assert_eq!(selected.name, "Terminal");
        assert!(output.into_inner().contains("1. Terminal"));
    }

    fn fixture(name: &str) -> AppCandidate {
        AppCandidate {
            name: name.to_owned(),
            bundle_id: Some(format!("dev.example.{name}")),
            path: None,
            installed: true,
            running: false,
            last_used: None,
        }
    }
}
