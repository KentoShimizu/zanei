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
fn browser_automation_denial_uses_recorder_path_and_target_toggle() {
    for (capability, target) in [
        (Capability::AutomateBrowser, "Chrome"),
        (Capability::AutomateSafari, "Safari"),
    ] {
        let rendered = super::render_human(
            &report_with_missing(&[capability]),
            Path::new("/Applications/Pantaray.app/Contents/MacOS/recorder"),
            false,
            false,
        );
        assert!(rendered.contains(&format!("Automation ({target})")));
        assert!(rendered.contains("recorder app/executable `/Applications/Pantaray.app"));
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
