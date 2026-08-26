use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// A platform-neutral ability that a collector requires from the recorder host.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Read application and window content through the accessibility tree.
    ReadAccessibilityTree,
    /// Observe keyboard and pointer input outside the recorder process.
    ObserveInput,
    /// Automate the supported browser to observe its current state.
    AutomateBrowser,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Available,
    ActionRequired,
    Deferred,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonCapabilities {
    required: BTreeSet<Capability>,
    read_accessibility_tree: CapabilityState,
    observe_input: CapabilityState,
    automate_browser: CapabilityState,
}

impl DaemonCapabilities {
    #[must_use]
    pub const fn new(
        required: BTreeSet<Capability>,
        read_accessibility_tree: CapabilityState,
        observe_input: CapabilityState,
        automate_browser: CapabilityState,
    ) -> Self {
        Self {
            required,
            read_accessibility_tree,
            observe_input,
            automate_browser,
        }
    }

    #[must_use]
    pub const fn state(&self, capability: Capability) -> CapabilityState {
        match capability {
            Capability::ReadAccessibilityTree => self.read_accessibility_tree,
            Capability::ObserveInput => self.observe_input,
            Capability::AutomateBrowser => self.automate_browser,
        }
    }

    #[must_use]
    pub fn ready(&self) -> bool {
        self.ready_for(&self.required).unwrap_or(false)
    }

    #[must_use]
    pub fn ready_for(&self, required: &BTreeSet<Capability>) -> Option<bool> {
        required.is_subset(&self.required).then(|| {
            required.iter().all(|capability| {
                self.state(*capability) == CapabilityState::Available
                    || (*capability == Capability::AutomateBrowser
                        && self.state(*capability) == CapabilityState::Deferred)
            })
        })
    }
}
