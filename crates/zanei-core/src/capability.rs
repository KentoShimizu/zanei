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
    /// Automate Safari to observe its current state.
    AutomateSafari,
}

impl Capability {
    #[must_use]
    pub const fn is_browser_automation(self) -> bool {
        matches!(self, Self::AutomateBrowser | Self::AutomateSafari)
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    automate_safari: Option<CapabilityState>,
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
            automate_safari: None,
        }
    }

    #[must_use]
    pub const fn with_automate_safari(mut self, state: CapabilityState) -> Self {
        self.automate_safari = Some(state);
        self
    }

    #[must_use]
    pub const fn state(&self, capability: Capability) -> CapabilityState {
        match capability {
            Capability::ReadAccessibilityTree => self.read_accessibility_tree,
            Capability::ObserveInput => self.observe_input,
            Capability::AutomateBrowser => self.automate_browser,
            Capability::AutomateSafari => match self.automate_safari {
                Some(state) => state,
                None => CapabilityState::Deferred,
            },
        }
    }

    #[must_use]
    pub fn ready(&self) -> bool {
        self.ready_for(&self.required).unwrap_or(false)
    }

    #[must_use]
    pub fn ready_for(&self, required: &BTreeSet<Capability>) -> Option<bool> {
        (required.is_subset(&self.required)
            && (!required.contains(&Capability::AutomateSafari) || self.automate_safari.is_some()))
        .then(|| {
            required.iter().all(|capability| {
                self.state(*capability) == CapabilityState::Available
                    || (capability.is_browser_automation()
                        && self.state(*capability) == CapabilityState::Deferred)
            })
        })
    }
}
