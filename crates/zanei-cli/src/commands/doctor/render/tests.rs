use std::collections::BTreeSet;
use std::path::Path;

use super::super::health::HealthReport;
use super::super::model::{DoctorReport, StoreKeyReport};
use super::super::report::build_report;
use zanei_collector::Capability;
use zanei_core::{CapabilityState, DaemonCapabilities, config::Config};

fn report_with_missing(missing: &[Capability]) -> DoctorReport {
    let state = |capability| {
        if missing.contains(&capability) {
            CapabilityState::ActionRequired
        } else {
            CapabilityState::Available
        }
    };
    let required = missing.iter().copied().collect::<BTreeSet<_>>();
    let snapshot = DaemonCapabilities::new(
        required.clone(),
        state(Capability::ReadAccessibilityTree),
        state(Capability::ObserveInput),
        state(Capability::AutomateBrowser),
    )
    .with_automate_safari(state(Capability::AutomateSafari));
    build_report(
        &Config::default(),
        &required,
        snapshot,
        false,
        StoreKeyReport::default(),
        HealthReport::status_missing(),
    )
    .expect("permission report fixture")
}
#[test]
fn browser_automation_denial_uses_target_toggle_without_diagnostic_path() {
    for (capability, target) in [
        (Capability::AutomateBrowser, "Google Chrome"),
        (Capability::AutomateSafari, "Safari"),
    ] {
        let rendered = super::render_human(
            &report_with_missing(&[capability]),
            Path::new("/Applications/Example.app/Contents/MacOS/recorder"),
            false,
            false,
        );
        assert!(rendered.contains(&format!("Automation ({target})")));
        assert!(!rendered.contains("/Applications/Example.app"));
        assert!(rendered.contains(&format!("switch its `{target}` toggle ON")));
        for forbidden in ["click `+`", "Command-V", "Finder", "stop && start"] {
            assert!(
                !rendered.contains(forbidden),
                "unexpected guidance: {forbidden}"
            );
        }
    }
}

#[test]
fn accessibility_denial_keeps_manual_permission_guidance() {
    let rendered = super::render_human(
        &report_with_missing(&[Capability::ReadAccessibilityTree]),
        Path::new("/tmp/recorder"),
        false,
        false,
    );

    assert!(rendered.contains("To grant Accessibility or Input Monitoring:"));
}

#[test]
fn mixed_denial_combines_targeted_automation_and_manual_guidance() {
    let rendered = super::render_human(
        &report_with_missing(&[
            Capability::AutomateSafari,
            Capability::ReadAccessibilityTree,
        ]),
        Path::new("/tmp/recorder"),
        false,
        false,
    );

    assert!(rendered.contains("switch its `Safari` toggle ON"));
    assert!(rendered.contains("click `+`"));
    assert!(rendered.contains("System Settings pane:"));
}

#[test]
fn running_recorder_with_or_without_snapshot_does_not_identify_diagnostic_path() {
    for reported_by_recorder in [false, true] {
        for (capability, target) in [
            (Capability::AutomateBrowser, "Google Chrome"),
            (Capability::AutomateSafari, "Safari"),
        ] {
            let mut report = report_with_missing(&[capability]);
            report.reported_by_recorder = reported_by_recorder;
            let rendered = super::render_human(
                &report,
                Path::new("/Applications/OtherCopy.app/Contents/MacOS/zanei"),
                false,
                true,
            );
            assert!(!rendered.contains("OtherCopy.app"));
            assert!(rendered.contains("app/executable running the recorder"));
            assert!(rendered.contains(&format!("switch its `{target}` toggle ON")));
        }
    }
}

fn screen_recording_report(
    content_snapshot: bool,
    missing: &[Capability],
    screen: Option<CapabilityState>,
) -> DoctorReport {
    let mut config = Config::default();
    config.capture.sources.clear();
    config.capture.content_snapshot = content_snapshot;
    let required = BTreeSet::from([Capability::ReadAccessibilityTree]);
    let accessibility = if missing.contains(&Capability::ReadAccessibilityTree) {
        CapabilityState::ActionRequired
    } else {
        CapabilityState::Available
    };
    let snapshot = DaemonCapabilities::new(
        required.clone(),
        accessibility,
        CapabilityState::Deferred,
        CapabilityState::Deferred,
    );
    build_report(
        &config,
        &required,
        match screen {
            Some(state) => snapshot.with_capture_screen(state),
            None => snapshot,
        },
        true,
        StoreKeyReport::default(),
        HealthReport::status_missing(),
    )
    .expect("screen recording report fixture")
}

#[test]
fn screen_recording_is_an_optional_row_with_the_reported_fallback_state() {
    // Catches the optional permission failing `doctor`, a missing or misnamed JSON row, and the
    // human output claiming a state the recorder never reported.
    for (state, json_state, row) in [
        (
            CapabilityState::Available,
            "available",
            "screen_recording granted         optional; OCR fallback on",
        ),
        (
            CapabilityState::ActionRequired,
            "action_required",
            "screen_recording denied          optional; OCR fallback off: Screen Recording not granted",
        ),
        (
            CapabilityState::Unavailable,
            "unavailable",
            "screen_recording unavailable     optional; OCR fallback unavailable: requires macOS 14",
        ),
    ] {
        let report = screen_recording_report(true, &[], Some(state));
        assert!(report.ok);
        assert_eq!(report.exit_code(), crate::commands::EXIT_SUCCESS);
        let json = serde_json::to_value(&report).expect("doctor JSON");
        let capture_screen = &json["capabilities"]["capture_screen"];
        assert_eq!(capture_screen["state"], json_state);
        assert_eq!(capture_screen["required"], false);
        assert_eq!(
            capture_screen["required_for"],
            serde_json::json!(["content.snapshot"])
        );
        assert_eq!(capture_screen["detail"]["permission"], "screen_recording");
        assert_eq!(
            capture_screen["detail"]["settings_url"],
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        );
        let rendered = super::render_human(&report, Path::new("/tmp/recorder"), false, true);
        assert!(rendered.contains(&format!("{row}\n")), "{rendered}");
        assert_eq!(
            rendered.contains("`zanei doctor --fix`"),
            state == CapabilityState::ActionRequired
        );
        assert!(rendered.contains("✓ All required permissions are granted."));
    }

    let unreported = screen_recording_report(true, &[], None);
    let json = serde_json::to_value(&unreported).expect("doctor JSON");
    assert!(json["capabilities"].get("capture_screen").is_none());
    assert!(
        super::render_human(&unreported, Path::new("/tmp/recorder"), false, true)
            .contains("optional; OCR fallback unknown")
    );

    let snapshots_off = screen_recording_report(false, &[], Some(CapabilityState::ActionRequired));
    let json = serde_json::to_value(&snapshots_off).expect("doctor JSON");
    assert!(json["capabilities"].get("capture_screen").is_none());
    assert!(
        !super::render_human(&snapshots_off, Path::new("/tmp/recorder"), false, true)
            .contains("screen_recording")
    );
}

#[test]
fn fix_opens_screen_recording_last_without_changing_the_exit_code() {
    // Catches `--fix` skipping the pane when nothing required is missing, opening it before a
    // required pane, or the optional pane turning into exit code 3.
    let only_optional = screen_recording_report(true, &[], Some(CapabilityState::ActionRequired));
    assert_eq!(
        only_optional.permissions_to_fix(true),
        Some(vec![Capability::CaptureScreen])
    );
    assert_eq!(only_optional.permissions_to_fix(false), None);
    assert_eq!(only_optional.exit_code(), crate::commands::EXIT_SUCCESS);

    let with_required = screen_recording_report(
        true,
        &[Capability::ReadAccessibilityTree],
        Some(CapabilityState::ActionRequired),
    );
    assert_eq!(
        with_required.permissions_to_fix(true),
        Some(vec![
            Capability::ReadAccessibilityTree,
            Capability::CaptureScreen
        ])
    );
    assert_eq!(
        with_required.exit_code(),
        crate::commands::EXIT_MISSING_PERMISSIONS
    );

    for screen in [
        Some(CapabilityState::Available),
        Some(CapabilityState::Unavailable),
        None,
    ] {
        assert_eq!(
            screen_recording_report(true, &[], screen).permissions_to_fix(true),
            None
        );
    }
    assert!(
        super::screen_recording_steps(true, Path::new("/Applications/Zanei.app"))
            .contains("       /Applications/Zanei.app\n")
    );
}
