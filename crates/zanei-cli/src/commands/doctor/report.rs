use std::collections::BTreeSet;

use zanei_collector::Capability;
use zanei_core::config::Config;
use zanei_core::store::StoreStatus;
use zanei_core::{CapabilityState, DaemonCapabilities};

use super::health::HealthReport;
use super::model::{CapabilityDetail, CapabilityReport, DoctorReport, StoreKeyReport};
use super::requirements::{accessibility_events, input_events};
use crate::error::CliError;

pub(super) fn capabilities_for_status<E>(
    status: Option<&StoreStatus>,
    fallback: impl FnOnce() -> Result<DaemonCapabilities, E>,
) -> Result<(DaemonCapabilities, bool), E> {
    match status.and_then(StoreStatus::reported_capabilities) {
        Some(capabilities) => Ok((capabilities.clone(), true)),
        None => fallback().map(|capabilities| (capabilities, false)),
    }
}

pub(super) fn build_report(
    config: &Config,
    required: &BTreeSet<Capability>,
    snapshot: DaemonCapabilities,
    reported_by_recorder: bool,
    store_key: StoreKeyReport,
    health: HealthReport,
) -> Result<DoctorReport, CliError> {
    let ok = snapshot.ready_for(required).ok_or_else(|| {
        CliError::InvalidValue(
            "recorder capability snapshot does not cover current requirements".to_owned(),
        )
    })?;
    let missing_permissions = required
        .iter()
        .copied()
        .filter(|capability| requires_action(*capability, snapshot.state(*capability)))
        .collect();
    let capabilities = CapabilityReport {
        read_accessibility_tree: CapabilityDetail::new(
            Capability::ReadAccessibilityTree,
            snapshot.state(Capability::ReadAccessibilityTree),
            required.contains(&Capability::ReadAccessibilityTree),
            accessibility_events(&config.capture.sources, config.capture.content_snapshot),
        ),
        observe_input: CapabilityDetail::new(
            Capability::ObserveInput,
            snapshot.state(Capability::ObserveInput),
            required.contains(&Capability::ObserveInput),
            input_events(&config.capture.sources),
        ),
        automate_browser: required.contains(&Capability::AutomateBrowser).then(|| {
            CapabilityDetail::new(
                Capability::AutomateBrowser,
                snapshot.state(Capability::AutomateBrowser),
                true,
                Vec::new(),
            )
        }),
    };

    Ok(DoctorReport {
        ok,
        capture_sources: config
            .capture
            .sources
            .iter()
            .map(|source| source.as_str())
            .collect(),
        capabilities,
        missing_permissions,
        reported_by_recorder,
        store_key,
        health,
    })
}

fn requires_action(capability: Capability, state: CapabilityState) -> bool {
    state != CapabilityState::Available
        && !(capability == Capability::AutomateBrowser && state == CapabilityState::Deferred)
}
