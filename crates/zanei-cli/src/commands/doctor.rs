use std::path::Path;

use zanei_core::config::Config;
use zanei_core::store::{LockedReason, StoreError, StoreFormat, StoreStatus};

use super::{EXIT_MISSING_PERMISSIONS, EXIT_SUCCESS};
use crate::daemon::StoreOwnership;
use crate::error::CliError;
use crate::permissions::probe_permissions;
use crate::store_access::{self, KeyAccess, KeyPrompt};

mod health;
mod model;
mod render;
mod report;
mod requirements;

use model::{DoctorReport, StoreKeyReport};
use render::{guide_granting, print_human};
use report::{build_report, capabilities_for_status};
use requirements::required_capabilities;
const STARTED_WITH_MISSING_PERMISSIONS: &str = "Zanei recording started with missing permissions — grant them, then run `zanei stop && zanei start`.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StartPermissionState {
    PendingSnapshot,
    Ready,
    Missing,
}

pub fn run(config_path: &Path, store_path: &Path, fix: bool, json: bool) -> Result<u8, CliError> {
    let config = Config::load(config_path)?;
    let owner = StoreOwnership::probe(store_path)?;
    let status_read = store_status(store_path);
    let report = evaluate(
        &config,
        status_read.status(),
        store_key_report(store_path),
        status_read.health_report(owner.as_ref()),
    )?;
    let executable = crate::executable::current().map_err(CliError::Input)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report, &executable, true, report.health.is_running());
    }
    if let Some(missing_permissions) = report.permissions_to_fix(fix) {
        guide_granting(missing_permissions, &executable)?;
    }
    Ok(report.exit_code())
}

pub(crate) fn permissions_ok(config: &Config) -> Result<bool, CliError> {
    let required = crate::daemon::required_capabilities_for(config);
    Ok(probe_permissions(&required)?.ready())
}

pub(crate) fn require_recorder_for_start(
    config: &Config,
    store_path: &Path,
    executable: &Path,
) -> Result<StartPermissionState, CliError> {
    let status = store_status(store_path).into_status()?.ok_or_else(|| {
        CliError::InvalidValue("running recorder did not publish a heartbeat".to_owned())
    })?;
    let owner = StoreOwnership::probe(store_path)?;
    evaluate_recorder_for_start(config, &status, owner.as_ref(), executable)
}

fn evaluate_recorder_for_start(
    config: &Config,
    status: &StoreStatus,
    owner: Option<&crate::daemon::StoreOwner>,
    executable: &Path,
) -> Result<StartPermissionState, CliError> {
    if !status.running {
        return Err(CliError::InvalidValue(
            "running recorder did not publish a fresh heartbeat".to_owned(),
        ));
    }
    let Some(snapshot) = status.reported_capabilities().cloned() else {
        return Ok(StartPermissionState::PendingSnapshot);
    };
    let required = crate::daemon::required_capabilities_for(config);
    if snapshot.ready_for(&required).is_none() {
        return Ok(StartPermissionState::PendingSnapshot);
    }
    let report = build_report(
        config,
        &required,
        snapshot,
        true,
        StoreKeyReport::default(),
        health::HealthReport::from_status(status, owner),
    )?;
    if !report.ok {
        println!("{STARTED_WITH_MISSING_PERMISSIONS}");
        print_human(&report, executable, false, report.health.is_running());
    }
    Ok(if report.ok {
        StartPermissionState::Ready
    } else {
        StartPermissionState::Missing
    })
}

fn store_status(store_path: &Path) -> health::StatusRead {
    match store_path.try_exists() {
        Ok(false) => health::StatusRead::missing(),
        Err(error) => health::StatusRead::unreadable(CliError::io(store_path, error)),
        Ok(true) => match store_access::open_reader(store_path, KeyPrompt::Allowed)
            .and_then(|reader| reader.status())
        {
            Ok(status) => health::StatusRead::readable(status),
            Err(error) => health::StatusRead::unreadable(error.into()),
        },
    }
}

fn store_key_report(store_path: &Path) -> StoreKeyReport {
    match StoreFormat::probe(store_path) {
        Ok(StoreFormat::Encrypted) => {
            match store_access::load_store_key(KeyAccess::Existing, KeyPrompt::Allowed) {
                Ok(Some(_)) => match store_access::key_store() {
                    Ok(store) => StoreKeyReport::new("key_store", Some(store.location())),
                    Err(StoreError::Locked(reason)) => StoreKeyReport::from_locked(&reason),
                    Err(error) => StoreKeyReport::new("unavailable", Some(error.to_string())),
                },
                Ok(None) => StoreKeyReport::from_locked(&LockedReason::KeyMissing),
                Err(StoreError::Locked(reason)) => StoreKeyReport::from_locked(&reason),
                Err(error) => StoreKeyReport::new("unavailable", Some(error.to_string())),
            }
        }
        Ok(StoreFormat::Plaintext | StoreFormat::Missing | StoreFormat::Unrecognized) => {
            StoreKeyReport::new("not_needed", None)
        }
        Err(error) => StoreKeyReport::new("unavailable", Some(error.to_string())),
    }
}

fn evaluate(
    config: &Config,
    status: Option<&StoreStatus>,
    store_key: StoreKeyReport,
    health: health::HealthReport,
) -> Result<DoctorReport, CliError> {
    let required = required_capabilities(config);
    let (snapshot, reported_by_recorder) = capabilities_for_status(status, || {
        probe_permissions(&required).map_err(CliError::from)
    })?;
    build_report(
        config,
        &required,
        snapshot,
        reported_by_recorder,
        store_key,
        health,
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeSet;
    use std::path::Path;

    use zanei_collector::Capability;
    use zanei_core::config::Config;
    use zanei_core::store::StoreStatus;
    use zanei_core::{CapabilityState, DaemonCapabilities};

    use super::health::HealthReport;
    use super::render::{output_indicates_non_persistent_signature, render_human};
    use super::{DoctorReport, build_report, capabilities_for_status};

    #[test]
    fn denied_report_asks_for_a_recorder_start_before_the_manual_add_fallback() {
        let config =
            Config::from_toml("[capture]\nsources = [\"input\"]\n").expect("input capture config");
        let required = crate::daemon::required_capabilities_for(&config);
        let report = permission_report(
            &config,
            &required,
            DaemonCapabilities::new(
                required.clone(),
                CapabilityState::Available,
                CapabilityState::ActionRequired,
                CapabilityState::Deferred,
            ),
            false,
        );

        let rendered = render_human(&report, Path::new("/tmp/zanei test"), false, false);

        let start_step = rendered
            .find("Run `zanei start`")
            .expect("the recorder start is offered first");
        let manual_step = rendered
            .find("To manage a missing row")
            .expect("manual `+` is offered for list management");
        assert!(start_step < manual_step);
        assert!(
            rendered.contains("In Accessibility, switch the executable's row ON if it is listed")
        );
        assert!(rendered.contains("Input Monitoring may omit its row"));
        assert!(rendered.contains("recorder-reported `zanei doctor` result is authoritative"));
        assert!(rendered.contains("`zanei doctor --fix`"));
        assert!(rendered.contains("click `+`"));
        assert!(rendered.contains("Command-Shift-G"));
        assert!(rendered.contains("one `~`"));
        assert!(rendered.contains("/tmp/zanei test"));
        assert!(rendered.contains("manual `+` entry may not persist"));
        assert!(rendered.contains("Bundle-ID `tccutil` resets do not apply"));
        assert!(rendered.contains("use the bundled distribution"));
        assert!(
            rendered
                .trim_end()
                .ends_with("After granting the permissions, run `zanei start`.")
        );
    }

    #[test]
    fn granted_report_explains_how_to_remove_a_bundled_app_permission() {
        let rendered = render_human(
            &granted_report(),
            Path::new("/Applications/Zanei.app/Contents/MacOS/zanei"),
            false,
            false,
        );

        assert!(rendered.contains("Accessibility lists the bundled app as `Zanei`"));
        assert!(rendered.contains("Input Monitoring may omit its row"));
        assert!(rendered.contains("click `+` and add `Zanei.app`"));
        assert!(rendered.contains("the bundled entry persists"));
        assert!(rendered.contains("tccutil reset Accessibility dev.zanei.recorder"));
        assert!(rendered.contains("tccutil reset ListenEvent dev.zanei.recorder"));
    }

    #[test]
    fn successful_report_warns_about_signature_and_ends_with_start_instruction() {
        let report = granted_report();

        let rendered = render_human(&report, Path::new("/tmp/zanei"), true, false);

        assert!(!rendered.contains("zanei doctor --fix"));
        assert!(rendered.contains("does not have a persistent code signature"));
        assert!(rendered.trim_end().ends_with(
            "✓ All required permissions are granted. Run `zanei start` to begin recording."
        ));
    }

    #[test]
    fn not_determined_automation_explains_the_first_chrome_prompt() {
        let mut config = Config::default();
        config.capture.sources.clear();
        config.capture.content_snapshot = true;
        let required = crate::daemon::required_capabilities_for(&config);
        let report = permission_report(
            &config,
            &required,
            DaemonCapabilities::new(
                required.clone(),
                CapabilityState::Available,
                CapabilityState::Available,
                CapabilityState::Deferred,
            ),
            false,
        );

        let rendered = render_human(&report, Path::new("/tmp/zanei"), false, false);

        assert!(rendered.contains("the first time Zanei contacts Chrome"));
        assert!(rendered.contains("no setup is needed in advance"));
        assert!(
            rendered
                .trim_end()
                .ends_with("✓ Permissions are ready. Run `zanei start` to begin recording.")
        );
    }

    #[test]
    fn granted_report_while_recording_says_recording_is_running() {
        let report = granted_report();

        let rendered = render_human(&report, Path::new("/tmp/zanei"), false, true);

        assert!(
            rendered
                .trim_end()
                .ends_with("✓ All required permissions are granted. Recording is running.")
        );
    }

    #[test]
    fn denied_report_while_recording_says_restart_recording() {
        let mut report = granted_report();
        report.ok = false;
        report.missing_permissions = vec![Capability::ObserveInput];

        let rendered = render_human(&report, Path::new("/tmp/zanei"), false, true);

        assert!(rendered.trim_end().ends_with(
            "restart recording with `zanei stop && zanei start` so the recorder picks them up."
        ));
    }

    #[test]
    fn running_recorder_snapshot_has_priority_and_stopped_status_uses_fallback() {
        let reported = permission_snapshot(false);
        let running = StoreStatus {
            running: true,
            retention_hours: Some(48),
            capabilities: Some(reported.clone()),
            ..StoreStatus::default()
        };
        let fallback_called = Cell::new(false);

        let (selected, from_recorder) = capabilities_for_status(Some(&running), || {
            fallback_called.set(true);
            Ok::<_, ()>(permission_snapshot(true))
        })
        .expect("select recorder permissions");

        assert_eq!(selected, reported);
        assert!(from_recorder);
        assert!(!fallback_called.get());

        let stopped = StoreStatus {
            running: false,
            capabilities: Some(permission_snapshot(false)),
            ..StoreStatus::default()
        };
        let fallback_called = Cell::new(false);
        let (selected, from_recorder) = capabilities_for_status(Some(&stopped), || {
            fallback_called.set(true);
            Ok::<_, ()>(permission_snapshot(true))
        })
        .expect("select local permissions");

        assert!(selected.ready());
        assert!(!from_recorder);
        assert!(fallback_called.get());
    }

    #[test]
    fn human_report_identifies_running_recorder_as_permission_source() {
        let mut report = granted_report();
        report.reported_by_recorder = true;

        let rendered = render_human(&report, Path::new("/tmp/zanei"), false, true);

        assert!(rendered.contains("Permission status as reported by the running recorder."));
    }

    #[test]
    fn human_report_identifies_local_permission_fallback() {
        let rendered = render_human(&granted_report(), Path::new("/tmp/zanei"), false, false);

        assert!(rendered.contains(
            "(probed from this process — start the recorder to see its own permissions)"
        ));
    }

    #[test]
    fn doctor_recomputes_overall_result_for_the_current_capture_sources() {
        let config = Config::from_toml("[capture]\nsources = [\"window\"]\n")
            .expect("window capture config");
        let required = BTreeSet::from([Capability::ReadAccessibilityTree]);
        let snapshot = DaemonCapabilities::new(
            required.clone(),
            CapabilityState::ActionRequired,
            CapabilityState::Available,
            CapabilityState::Deferred,
        );

        let report = permission_report(&config, &required, snapshot, true);

        assert!(!report.ok);
        assert_eq!(
            report.missing_permissions,
            [Capability::ReadAccessibilityTree]
        );
    }

    #[test]
    fn ui_capture_requires_input_monitoring_for_clicks() {
        let config =
            Config::from_toml("[capture]\nsources = [\"ui\"]\n").expect("ui capture config");
        let required = crate::daemon::required_capabilities_for(&config);
        let snapshot = DaemonCapabilities::new(
            required.clone(),
            CapabilityState::Available,
            CapabilityState::ActionRequired,
            CapabilityState::Deferred,
        );

        let report = permission_report(&config, &required, snapshot, false);

        assert!(!report.ok);
        assert_eq!(report.missing_permissions, [Capability::ObserveInput]);
        assert_eq!(report.capabilities.observe_input.required_for, ["ui.click"]);
    }

    #[test]
    fn content_snapshot_permissions_and_required_for_match_collector_selection() {
        super::requirements::assert_estimate_matches_collector_matrix();

        let mut content = Config::default();
        content.capture.sources.clear();
        content.capture.content_snapshot = true;
        let required = crate::daemon::required_capabilities_for(&content);
        let snapshot = DaemonCapabilities::new(
            required.clone(),
            CapabilityState::ActionRequired,
            CapabilityState::Available,
            CapabilityState::Deferred,
        );
        let report = permission_report(&content, &required, snapshot, false);

        assert_eq!(
            report.capabilities.read_accessibility_tree.required_for,
            ["content.snapshot"]
        );
        assert_eq!(
            report.missing_permissions,
            [Capability::ReadAccessibilityTree]
        );
        assert_eq!(
            report
                .capabilities
                .automate_browser
                .as_ref()
                .expect("browser capability")
                .detail
                .status,
            "not_determined"
        );
    }

    #[test]
    fn signature_parser_warns_only_for_explicit_unsigned_or_ad_hoc_markers() {
        assert!(output_indicates_non_persistent_signature(
            "Signature=adhoc\n"
        ));
        assert!(output_indicates_non_persistent_signature(
            "CodeDirectory flags=0x2(adhoc,linker-signed)\n"
        ));
        assert!(output_indicates_non_persistent_signature(
            "/tmp/zanei: code object is not signed at all\n"
        ));
        assert!(!output_indicates_non_persistent_signature(
            "Authority=Zanei Local Development\nTeamIdentifier=not set\n"
        ));
        assert!(!output_indicates_non_persistent_signature(
            "codesign returned an unexpected diagnostic\n"
        ));
    }

    fn granted_report() -> DoctorReport {
        let config = Config::default();
        let required = crate::daemon::required_capabilities_for(&config);
        permission_report(
            &config,
            &required,
            DaemonCapabilities::new(
                required.clone(),
                CapabilityState::Available,
                CapabilityState::Available,
                CapabilityState::Available,
            ),
            false,
        )
    }

    fn permission_snapshot(permissions_ok: bool) -> DaemonCapabilities {
        DaemonCapabilities::new(
            BTreeSet::from([Capability::ReadAccessibilityTree]),
            if permissions_ok {
                CapabilityState::Available
            } else {
                CapabilityState::ActionRequired
            },
            CapabilityState::Available,
            CapabilityState::Deferred,
        )
    }

    fn permission_report(
        config: &Config,
        required: &BTreeSet<Capability>,
        snapshot: DaemonCapabilities,
        reported_by_recorder: bool,
    ) -> DoctorReport {
        build_report(
            config,
            required,
            snapshot,
            reported_by_recorder,
            super::StoreKeyReport::default(),
            HealthReport::status_missing(),
        )
        .expect("doctor report")
    }
}
