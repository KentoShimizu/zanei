#![cfg(target_os = "macos")]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const APPLE_EVENTS_USAGE_DESCRIPTION: &str =
    "Zanei reads Chrome URLs and window types to record browser activity.";

#[test]
fn make_app_creates_the_signed_bundle_contract() {
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root");
    let temporary_directory = TempDir::new().expect("app bundle fixture");
    let app_path = temporary_directory.path().join("Zanei.app");
    let status = Command::new(repository_root.join("packaging/make-app.sh"))
        .arg("-")
        .arg(env!("CARGO_BIN_EXE_zanei"))
        .arg(&app_path)
        .status()
        .expect("run make-app.sh");

    assert!(status.success(), "make-app.sh failed with {status}");
    let plist_path = app_path.join("Contents/Info.plist");
    let executable_path = app_path.join("Contents/MacOS/zanei");
    let executable_metadata = executable_path.metadata().expect("bundle executable");
    assert!(executable_metadata.is_file());
    assert_ne!(executable_metadata.permissions().mode() & 0o111, 0);
    assert_eq!(plist_value(&plist_path, "CFBundleExecutable"), "zanei");
    assert_eq!(
        plist_value(&plist_path, "CFBundleIdentifier"),
        "dev.zanei.recorder"
    );
    assert_eq!(plist_value(&plist_path, "CFBundleName"), "Zanei");
    assert_eq!(plist_value(&plist_path, "CFBundlePackageType"), "APPL");
    assert_eq!(
        plist_value(&plist_path, "CFBundleShortVersionString"),
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        plist_value(&plist_path, "CFBundleVersion"),
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(plist_value(&plist_path, "LSUIElement"), "true");
    assert_eq!(
        plist_value(&plist_path, "NSAppleEventsUsageDescription"),
        APPLE_EVENTS_USAGE_DESCRIPTION
    );
}

fn plist_value(plist_path: &Path, key: &str) -> String {
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", key, "raw", "-o", "-"])
        .arg(plist_path)
        .output()
        .expect("run plutil");
    command_stdout(output, "plutil")
}

fn command_stdout(output: Output, command: &str) -> String {
    assert!(
        output.status.success(),
        "{command} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("command output is UTF-8")
        .trim()
        .to_owned()
}
