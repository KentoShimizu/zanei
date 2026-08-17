use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus};

use zanei_collector::Permission;
use zanei_macos::permission::PermissionChecker;

use super::DoctorReport;
use crate::error::CliError;

const CODESIGN: &str = "/usr/bin/codesign";
const OPEN: &str = "/usr/bin/open";

const BUNDLED_PERMISSION_NOTE: &str = "Accessibility lists the bundled app as `Zanei`. Input Monitoring may omit its row even after the permission-dialog grant takes effect; the recorder-reported `zanei doctor` result is authoritative. To manage a missing Input Monitoring row from System Settings, click `+` and add `Zanei.app`; the bundled entry persists. Remove a listed permission with its row toggle, or stop Zanei and run `tccutil reset Accessibility dev.zanei.recorder` or `tccutil reset ListenEvent dev.zanei.recorder`.\n";
const UNBUNDLED_PERMISSION_NOTE: &str = "A raw `cargo install` executable can be omitted from these lists, and a manual `+` entry may not persist. Bundle-ID `tccutil` resets do not apply to it; use the bundled distribution for manageable, persistent entries.\n";

pub(super) fn print_human(
    report: &DoctorReport,
    executable: &Path,
    inspect_signature: bool,
    recording: bool,
) {
    let signature_warning = inspect_signature && lacks_persistent_signature(executable);
    print!(
        "{}",
        render_human(report, executable, signature_warning, recording)
    );
}

pub(super) fn render_human(
    report: &DoctorReport,
    executable: &Path,
    signature_warning: bool,
    recording: bool,
) -> String {
    let permission_target = permission_target_path(executable);
    let bundled = permission_target != executable;
    let mut output = String::from("PERMISSION       STATUS          REQUIRED FOR\n");
    output.push_str(&format!(
        "accessibility    {:<15} {}\n",
        report.permissions.accessibility.status,
        report.permissions.accessibility.required_for.join(",")
    ));
    output.push_str(&format!(
        "input_monitoring {:<15} {}\n",
        report.permissions.input_monitoring.status,
        report.permissions.input_monitoring.required_for.join(",")
    ));
    for (app, status) in &report.permissions.automation.per_app {
        output.push_str(&format!("automation       {status:<15} {app}\n"));
    }
    if report.reported_by_recorder {
        output.push_str("\nPermission status as reported by the running recorder.\n");
    } else {
        output.push_str(
            "\n(probed from this process — start the recorder to see its own permissions)\n",
        );
    }

    let automation_pending = report
        .permissions
        .automation
        .per_app
        .values()
        .any(|status| *status == "not_determined");
    if automation_pending {
        output.push_str(
            "\nAutomation: macOS will show a permission dialog the first time Zanei contacts Chrome; no setup is needed in advance.\n",
        );
    }

    if report.missing_permissions.is_empty() {
        output.push('\n');
        output.push_str(permission_list_note(bundled));
    } else {
        if let Some(pane) = report.settings_pane {
            output.push_str(&format!("\nSystem Settings pane: {pane}\n"));
        }
        output.push_str("\nTo grant a missing permission:\n");
        output.push_str(
            "  1. Run `zanei start` (`zanei stop && zanei start` if it is already running) so the recorder asks macOS for the permission.\n",
        );
        if bundled {
            output.push_str(
                "  2. Accessibility lists the bundled app as `Zanei`; switch that row ON.\n",
            );
        } else {
            output.push_str(
                "  2. In Accessibility, switch the executable's row ON if it is listed.\n",
            );
        }
        output.push_str(
            "  3. Input Monitoring may omit its row even after the dialog grant takes effect. The recorder-reported `zanei doctor` result is authoritative.\n",
        );
        output.push_str(
            "  4. To manage a missing row from System Settings, click `+`, press Command-Shift-G (⌘⇧G) or type one `~` in the file dialog to reveal the path field, paste this app or executable path, press Return, click Open, then switch the row ON.\n",
        );
        output.push_str(&format!("       {}\n", permission_target.display()));
        output.push_str(
            "Run `zanei doctor --fix` to open each missing pane with the path already on your clipboard.\n",
        );
        output.push_str(permission_list_note(bundled));
    }

    if signature_warning {
        output.push_str(
            "\nWarning: This build does not have a persistent code signature. Rebuilding it will reset previously granted macOS permissions.\n",
        );
    }

    output.push('\n');
    match (report.ok, recording) {
        (true, true) => {
            output.push_str("✓ All required permissions are granted. Recording is running.\n");
        }
        (true, false) => {
            if automation_pending {
                output.push_str("✓ Permissions are ready. Run `zanei start` to begin recording.\n");
            } else {
                output.push_str(
                    "✓ All required permissions are granted. Run `zanei start` to begin recording.\n",
                );
            }
        }
        (false, true) => {
            output.push_str(
                "After granting the permissions, restart recording with `zanei stop && zanei start` so the recorder picks them up.\n",
            );
        }
        (false, false) => {
            output.push_str("After granting the permissions, run `zanei start`.\n");
        }
    }
    output
}

// Interactive walkthrough for `doctor --fix`: one pane at a time, with the app
// or executable path already on the clipboard and its target revealed in Finder,
// so granting needs no outside knowledge of how macOS permission lists work.
pub(super) fn guide_granting(missing: &[Permission], executable: &Path) -> Result<(), CliError> {
    use std::io::{BufRead, Write};

    let permission_target = permission_target_path(executable);
    let bundled = permission_target != executable;
    copy_to_clipboard(&permission_target.display().to_string());
    reveal_in_finder(permission_target)?;

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let checker = PermissionChecker::new();
    let total = missing.len();
    println!();
    println!(
        "First run `zanei start` (`zanei stop && zanei start` if it is already running) so the recorder asks macOS for the permissions."
    );
    println!("Finder is showing the app or executable whose path is on your clipboard.");
    for (index, permission) in missing.iter().enumerate() {
        checker.open_settings(permission)?;
        println!();
        println!(
            "[{}/{}] {} — System Settings is now open on the right pane. In that window:",
            index + 1,
            total,
            pane_title(permission)
        );
        if bundled {
            println!("  1. Accessibility lists the bundled app as `Zanei`; switch that row ON.");
        } else {
            println!("  1. In Accessibility, switch the executable's row ON if it is listed.");
        }
        println!(
            "  2. Input Monitoring may omit its row after a dialog grant. The recorder-reported `zanei doctor` result is authoritative."
        );
        println!("  3. To manage a missing row from this list, click the `+` button.");
        println!(
            "  4. In the file dialog press Command-V, then Return. (The path is on your clipboard:)"
        );
        println!("       {}", permission_target.display());
        println!("  5. Click Open, then switch the new row ON.");
        println!("  Alternatively drag the revealed app or executable from Finder onto the list.");
        if index + 1 < total {
            print!("Press Return when done to open the next pane... ");
            let _ = stdout.flush();
            let mut line = String::new();
            let _ = stdin.lock().read_line(&mut line);
        }
    }
    println!();
    println!(
        "When every toggle is on, run `zanei stop && zanei start` (or `zanei start` if it is not running), then `zanei doctor` to confirm."
    );
    println!("{}", permission_list_note(bundled).trim_end());
    Ok(())
}

const fn permission_list_note(bundled: bool) -> &'static str {
    if bundled {
        BUNDLED_PERMISSION_NOTE
    } else {
        UNBUNDLED_PERMISSION_NOTE
    }
}

pub(super) fn permission_target_path(executable: &Path) -> &Path {
    let Some(macos_directory) = executable.parent() else {
        return executable;
    };
    let Some(contents_directory) = macos_directory.parent() else {
        return executable;
    };
    let Some(app_bundle) = contents_directory.parent() else {
        return executable;
    };
    if macos_directory
        .file_name()
        .is_some_and(|name| name == "MacOS")
        && contents_directory
            .file_name()
            .is_some_and(|name| name == "Contents")
        && app_bundle
            .extension()
            .is_some_and(|extension| extension == "app")
    {
        app_bundle
    } else {
        executable
    }
}

fn pane_title(permission: &Permission) -> String {
    match permission {
        Permission::Accessibility => "Accessibility".to_owned(),
        Permission::InputMonitoring => "Input Monitoring".to_owned(),
        Permission::Automation { bundle_id } => format!("Automation ({bundle_id})"),
    }
}

fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    if let Ok(mut child) = Command::new("/usr/bin/pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

fn reveal_in_finder(target: &Path) -> Result<(), CliError> {
    reveal_in_finder_with(target, |program, arguments| {
        Command::new(program).args(arguments).status()
    })
}

fn reveal_in_finder_with(
    target: &Path,
    run: impl FnOnce(&str, &[&OsStr]) -> io::Result<ExitStatus>,
) -> Result<(), CliError> {
    let arguments = [OsStr::new("-R"), target.as_os_str()];
    let status = run(OPEN, &arguments).map_err(|source| CliError::FinderRevealLaunch {
        program: OPEN,
        path: target.to_path_buf(),
        source,
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(CliError::FinderRevealFailed {
            program: OPEN,
            path: target.to_path_buf(),
            status,
        })
    }
}

fn lacks_persistent_signature(executable: &Path) -> bool {
    let Ok(output) = Command::new(CODESIGN)
        .args(["-dv"])
        .arg(executable)
        .output()
    else {
        return false;
    };
    let (Ok(stdout), Ok(stderr)) = (
        std::str::from_utf8(&output.stdout),
        std::str::from_utf8(&output.stderr),
    ) else {
        return false;
    };
    output_indicates_non_persistent_signature(stdout)
        || output_indicates_non_persistent_signature(stderr)
}

pub(super) fn output_indicates_non_persistent_signature(output: &str) -> bool {
    output.lines().any(|line| {
        let line = line.trim();
        line == "Signature=adhoc"
            || line.contains("(adhoc")
            || line.contains("code object is not signed at all")
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::ExitStatus;

    use tempfile::TempDir;

    use super::{OPEN, permission_target_path, reveal_in_finder_with};

    #[test]
    fn symlinked_executable_resolves_to_app_root_for_finder_reveal() {
        let directory = TempDir::new().expect("permission target fixture");
        let app = directory.path().join("Zanei.app");
        let executable = app.join("Contents/MacOS/zanei");
        fs::create_dir_all(executable.parent().expect("bundle executable parent"))
            .expect("create bundle executable parent");
        fs::write(&executable, b"fixture").expect("write bundle executable");
        let symlink_path = directory.path().join("bin/zanei");
        fs::create_dir_all(symlink_path.parent().expect("symlink parent"))
            .expect("create symlink parent");
        symlink(&executable, &symlink_path).expect("create executable symlink");

        let resolved = crate::executable::canonicalize_or_original(&symlink_path);
        let permission_target = permission_target_path(&resolved);
        let expected_app = fs::canonicalize(&app).expect("canonical app fixture");

        assert_eq!(permission_target, expected_app);
        reveal_in_finder_with(permission_target, |program, arguments| {
            assert_eq!(program, OPEN);
            assert_eq!(arguments, ["-R".as_ref(), expected_app.as_os_str()]);
            Ok(ExitStatus::from_raw(0))
        })
        .expect("Finder reveal command");
    }

    #[test]
    fn symlinked_unbundled_executable_resolves_to_the_executable_itself() {
        let directory = TempDir::new().expect("raw executable fixture");
        let executable = directory.path().join("libexec/zanei");
        fs::create_dir_all(executable.parent().expect("executable parent"))
            .expect("create executable parent");
        fs::write(&executable, b"fixture").expect("write executable");
        let symlink_path = directory.path().join("bin/zanei");
        fs::create_dir_all(symlink_path.parent().expect("symlink parent"))
            .expect("create symlink parent");
        symlink(&executable, &symlink_path).expect("create executable symlink");

        let resolved = crate::executable::canonicalize_or_original(&symlink_path);

        assert_eq!(permission_target_path(&resolved), resolved);
    }

    #[test]
    fn finder_reveal_surfaces_command_launch_failure() {
        let target = Path::new("/Applications/Zanei.app");

        let error = reveal_in_finder_with(target, |_, _| {
            Err(std::io::Error::other("fixture launch failure"))
        })
        .expect_err("Finder reveal must fail");

        assert!(matches!(
            error,
            crate::error::CliError::FinderRevealLaunch { .. }
        ));
    }

    #[test]
    fn finder_reveal_surfaces_unsuccessful_exit_status() {
        let target = Path::new("/Applications/Zanei.app");

        let error = reveal_in_finder_with(target, |_, _| Ok(ExitStatus::from_raw(1 << 8)))
            .expect_err("Finder reveal must fail");

        assert!(matches!(
            error,
            crate::error::CliError::FinderRevealFailed { .. }
        ));
    }
}
