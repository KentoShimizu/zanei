use std::path::Path;

use zanei_collector::Capability;
use zanei_core::config::Config;
use zanei_core::store::{
    DaemonPermissions, LockedReason, PermissionState, StoreError, StoreFormat, StoreStatus,
};

use super::{EXIT_MISSING_PERMISSIONS, EXIT_SUCCESS};
use crate::daemon::StoreOwnership;
use crate::error::CliError;
use crate::permissions::probe_permissions;
use crate::store_access::{self, KeyAccess, KeyPrompt};

mod health;
mod model;
mod render;
mod requirements;

#[cfg(test)]
use model::AutomationDetail;
use model::{DoctorReport, PermissionReport, StatusDetail, StoreKeyReport};
use render::{guide_granting, print_human};
use requirements::{
    accessibility_events, estimated_permissions, input_events, permission_name_and_pane,
    snapshot_status, status_name,
};
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
    Ok(probe_permissions(&required)?.permissions_ok)
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
    let Some(snapshot) = status.reported_permissions().cloned() else {
        return Ok(StartPermissionState::PendingSnapshot);
    };
    let required = crate::daemon::required_capabilities_for(config);
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
                Ok(Some(_)) => {
                    StoreKeyReport::new("key_store", Some(store_access::key_store().location()))
                }
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
    let required = estimated_permissions(config);
    let (snapshot, reported_by_recorder) = permissions_for_status(status, || {
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

fn permissions_for_status<E>(
    status: Option<&StoreStatus>,
    fallback: impl FnOnce() -> Result<DaemonPermissions, E>,
) -> Result<(DaemonPermissions, bool), E> {
    match status.and_then(StoreStatus::reported_permissions) {
        Some(permissions) => Ok((permissions.clone(), true)),
        None => fallback().map(|permissions| (permissions, false)),
    }
}

fn build_report(
    config: &Config,
    required: &std::collections::BTreeSet<Capability>,
    snapshot: DaemonPermissions,
    reported_by_recorder: bool,
    store_key: StoreKeyReport,
    health: health::HealthReport,
) -> Result<DoctorReport, CliError> {
    let mut checked = required.clone();
    checked.insert(Capability::ReadAccessibilityTree);
    checked.insert(Capability::ObserveInput);
    let mut permissions = PermissionReport::default();
    let mut missing_required = Vec::new();
    let mut missing_permissions = Vec::new();
    let mut settings_pane = None;

    for permission in checked {
        let status = snapshot_status(&snapshot, &permission).ok_or_else(|| {
            CliError::InvalidValue(format!(
                "recorder permission snapshot is missing {}",
                permission_name_and_pane(&permission).0
            ))
        })?;
        let is_required = required.contains(&permission);
        match &permission {
            Capability::ReadAccessibilityTree => {
                permissions.accessibility = StatusDetail {
                    status: status_name(status),
                    required_for: accessibility_events(
                        &config.capture.sources,
                        config.capture.content_snapshot,
                    ),
                };
            }
            Capability::ObserveInput => {
                permissions.input_monitoring = StatusDetail {
                    status: status_name(status),
                    required_for: input_events(&config.capture.sources),
                };
            }
            Capability::AutomateBrowser => {
                permissions.automation.per_app.insert(
                    zanei_core::privacy::CHROME_BUNDLE_ID.to_owned(),
                    status_name(status),
                );
            }
        }
        // Automation can be indeterminate while the target app is not running. The public
        // doctor example reports that state but does not classify it as a missing permission.
        let missing = status != PermissionState::Granted
            && !matches!(
                (&permission, status),
                (Capability::AutomateBrowser, PermissionState::NotDetermined)
            );
        if is_required && missing {
            let (name, pane) = permission_name_and_pane(&permission);
            missing_required.push(name);
            missing_permissions.push(permission);
            settings_pane.get_or_insert(pane);
        }
    }

    Ok(DoctorReport {
        ok: missing_required.is_empty(),
        capture_sources: config
            .capture
            .sources
            .iter()
            .map(|source| source.as_str())
            .collect(),
        permissions,
        missing_required,
        settings_pane,
        missing_permissions,
        reported_by_recorder,
        store_key,
        health,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use zanei_collector::Capability;
    use zanei_core::config::Config;
    use zanei_core::store::{DaemonPermissions, PermissionState, StoreStatus};

    use super::health::HealthReport;
    use super::render::{output_indicates_non_persistent_signature, render_human};
    use super::{
        AutomationDetail, DoctorReport, PermissionReport, StatusDetail, build_report,
        permissions_for_status,
    };

    #[test]
    fn denied_report_asks_for_a_recorder_start_before_the_manual_add_fallback() {
        let report = DoctorReport {
            store_key: super::StoreKeyReport::default(),
            ok: false,
            capture_sources: vec!["input"],
            permissions: PermissionReport {
                accessibility: detail("granted", &[]),
                input_monitoring: detail("denied", &["input.key"]),
                automation: AutomationDetail::default(),
            },
            missing_required: vec!["input_monitoring"],
            settings_pane: Some("input-pane"),
            missing_permissions: vec![Capability::ObserveInput],
            reported_by_recorder: false,
            health: HealthReport::status_missing(),
        };

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
        let mut report = granted_report();
        report
            .permissions
            .automation
            .per_app
            .insert("com.google.Chrome".to_owned(), "not_determined");

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
        report.missing_required = vec!["input_monitoring"];

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
            permissions: Some(reported.clone()),
            ..StoreStatus::default()
        };
        let fallback_called = Cell::new(false);

        let (selected, from_recorder) = permissions_for_status(Some(&running), || {
            fallback_called.set(true);
            Ok::<_, ()>(permission_snapshot(true))
        })
        .expect("select recorder permissions");

        assert_eq!(selected, reported);
        assert!(from_recorder);
        assert!(!fallback_called.get());

        let stopped = StoreStatus {
            running: false,
            permissions: Some(permission_snapshot(false)),
            ..StoreStatus::default()
        };
        let fallback_called = Cell::new(false);
        let (selected, from_recorder) = permissions_for_status(Some(&stopped), || {
            fallback_called.set(true);
            Ok::<_, ()>(permission_snapshot(true))
        })
        .expect("select local permissions");

        assert!(selected.permissions_ok);
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
        let snapshot = DaemonPermissions {
            permissions_ok: true,
            accessibility: PermissionState::Denied,
            input_monitoring: PermissionState::Granted,
            automation: BTreeMap::new(),
        };

        let report = permission_report(&config, &required, snapshot, true);

        assert!(!report.ok);
        assert_eq!(report.missing_required, ["accessibility"]);
    }

    #[test]
    fn ui_capture_requires_input_monitoring_for_clicks() {
        let config =
            Config::from_toml("[capture]\nsources = [\"ui\"]\n").expect("ui capture config");
        let required = crate::daemon::required_capabilities_for(&config);
        let snapshot = DaemonPermissions {
            permissions_ok: false,
            accessibility: PermissionState::Granted,
            input_monitoring: PermissionState::Denied,
            automation: BTreeMap::new(),
        };

        let report = permission_report(&config, &required, snapshot, false);

        assert!(!report.ok);
        assert_eq!(report.missing_required, ["input_monitoring"]);
        assert_eq!(
            report.permissions.input_monitoring.required_for,
            ["ui.click"]
        );
    }

    #[test]
    fn content_snapshot_permissions_and_required_for_match_collector_selection() {
        super::requirements::assert_estimate_matches_collector_matrix();

        let mut content = Config::default();
        content.capture.sources.clear();
        content.capture.content_snapshot = true;
        let required = crate::daemon::required_capabilities_for(&content);
        let snapshot = DaemonPermissions {
            permissions_ok: false,
            accessibility: PermissionState::Denied,
            input_monitoring: PermissionState::Granted,
            automation: BTreeMap::from([(
                "com.google.Chrome".to_owned(),
                PermissionState::NotDetermined,
            )]),
        };
        let report = permission_report(&content, &required, snapshot, false);

        assert_eq!(
            report.permissions.accessibility.required_for,
            ["content.snapshot"]
        );
        assert_eq!(report.missing_required, ["accessibility"]);
        assert_eq!(
            report.permissions.automation.per_app["com.google.Chrome"],
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
        DoctorReport {
            store_key: super::StoreKeyReport::default(),
            ok: true,
            capture_sources: vec!["app"],
            permissions: PermissionReport {
                accessibility: detail("granted", &[]),
                input_monitoring: detail("granted", &[]),
                automation: AutomationDetail::default(),
            },
            missing_required: Vec::new(),
            settings_pane: None,
            missing_permissions: Vec::new(),
            reported_by_recorder: false,
            health: HealthReport::status_missing(),
        }
    }

    fn permission_snapshot(permissions_ok: bool) -> DaemonPermissions {
        DaemonPermissions {
            permissions_ok,
            accessibility: PermissionState::Granted,
            input_monitoring: PermissionState::Granted,
            automation: BTreeMap::new(),
        }
    }

    fn permission_report(
        config: &Config,
        required: &BTreeSet<Capability>,
        snapshot: DaemonPermissions,
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

    fn detail(status: &'static str, required_for: &[&'static str]) -> StatusDetail {
        StatusDetail {
            status,
            required_for: required_for.to_vec(),
        }
    }
}
