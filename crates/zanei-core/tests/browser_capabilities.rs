use std::collections::BTreeSet;

use zanei_core::{Capability, CapabilityState, DaemonCapabilities};

fn legacy_capabilities() -> DaemonCapabilities {
    DaemonCapabilities::new(
        BTreeSet::from([
            Capability::ReadAccessibilityTree,
            Capability::AutomateBrowser,
        ]),
        CapabilityState::Available,
        CapabilityState::Available,
        CapabilityState::Deferred,
    )
}

#[test]
fn legacy_capability_json_roundtrips_without_safari() {
    let capabilities = legacy_capabilities();
    let json = serde_json::to_value(&capabilities).expect("serialize legacy capabilities");

    assert!(json.get("automate_safari").is_none());
    assert_eq!(
        serde_json::from_value::<DaemonCapabilities>(json)
            .expect("deserialize legacy capabilities"),
        capabilities
    );
}

#[test]
fn absent_safari_snapshot_is_incomplete_but_deferred_probe_is_ready() {
    let required = BTreeSet::from([Capability::AutomateSafari]);
    let absent = DaemonCapabilities::new(
        required.clone(),
        CapabilityState::Available,
        CapabilityState::Available,
        CapabilityState::Available,
    );
    assert_eq!(absent.ready_for(&required), None);

    let deferred = absent.with_automate_safari(CapabilityState::Deferred);
    assert_eq!(deferred.ready_for(&required), Some(true));
}

#[test]
fn browser_targets_keep_mixed_states_independent() {
    let required = BTreeSet::from([Capability::AutomateBrowser, Capability::AutomateSafari]);
    let capabilities = DaemonCapabilities::new(
        required.clone(),
        CapabilityState::Available,
        CapabilityState::Available,
        CapabilityState::Available,
    )
    .with_automate_safari(CapabilityState::ActionRequired);

    assert_eq!(
        capabilities.state(Capability::AutomateBrowser),
        CapabilityState::Available
    );
    assert_eq!(
        capabilities.state(Capability::AutomateSafari),
        CapabilityState::ActionRequired
    );
    assert_eq!(capabilities.ready_for(&required), Some(false));
}

#[test]
fn browser_targets_preserve_the_reverse_mixed_states() {
    let required = BTreeSet::from([Capability::AutomateBrowser, Capability::AutomateSafari]);
    let capabilities = DaemonCapabilities::new(
        required.clone(),
        CapabilityState::Available,
        CapabilityState::Available,
        CapabilityState::ActionRequired,
    )
    .with_automate_safari(CapabilityState::Available);

    assert_eq!(
        capabilities.state(Capability::AutomateBrowser),
        CapabilityState::ActionRequired
    );
    assert_eq!(
        capabilities.state(Capability::AutomateSafari),
        CapabilityState::Available
    );
    assert_eq!(capabilities.ready_for(&required), Some(false));
}
