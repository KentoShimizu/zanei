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
    /// Capture a window's pixels for text recognition. Optional: never required, so it
    /// never affects readiness.
    CaptureScreen,
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
    /// This OS version lacks the capability. Only the optional screen capture reports it.
    Unavailable,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capture_screen: Option<CapabilityState>,
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
            capture_screen: None,
        }
    }

    #[must_use]
    pub const fn with_automate_safari(mut self, state: CapabilityState) -> Self {
        self.automate_safari = Some(state);
        self
    }

    /// Records the optional screen-capture state, which only a recorder with content
    /// snapshots on reports.
    #[must_use]
    pub const fn with_capture_screen(mut self, state: CapabilityState) -> Self {
        self.capture_screen = Some(state);
        self
    }

    /// `None` when this snapshot does not report screen capture.
    #[must_use]
    pub const fn capture_screen(&self) -> Option<CapabilityState> {
        self.capture_screen
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
            Capability::CaptureScreen => match self.capture_screen {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_screen_capture_never_affects_readiness_and_round_trips() {
        // Catches the optional permission leaking into `permissions_ok` / `start`'s exit
        // code, and a heartbeat losing or inventing the reported state.
        let required = BTreeSet::from([Capability::ReadAccessibilityTree]);
        for (state, name) in [
            (CapabilityState::ActionRequired, "action_required"),
            (CapabilityState::Unavailable, "unavailable"),
        ] {
            let capabilities = DaemonCapabilities::new(
                required.clone(),
                CapabilityState::Available,
                CapabilityState::Deferred,
                CapabilityState::Deferred,
            )
            .with_capture_screen(state);

            assert!(capabilities.ready());
            assert_eq!(capabilities.ready_for(&required), Some(true));
            let json = serde_json::to_value(&capabilities).expect("serialize capabilities");
            assert_eq!(json["capture_screen"], name);
            let read: DaemonCapabilities =
                serde_json::from_value(json).expect("deserialize capabilities");
            assert_eq!(read.capture_screen(), Some(state));
        }

        let unreported: DaemonCapabilities = serde_json::from_value(serde_json::json!({
            "required": ["read_accessibility_tree"],
            "read_accessibility_tree": "available",
            "observe_input": "deferred",
            "automate_browser": "deferred"
        }))
        .expect("heartbeat without screen capture");
        assert_eq!(unreported.capture_screen(), None);
        assert!(
            serde_json::to_value(&unreported)
                .expect("serialize")
                .get("capture_screen")
                .is_none()
        );
    }
}
