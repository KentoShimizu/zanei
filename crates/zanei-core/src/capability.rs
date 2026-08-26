/// A platform-neutral ability that a collector requires from the recorder host.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Capability {
    /// Read application and window content through the accessibility tree.
    ReadAccessibilityTree,
    /// Observe keyboard and pointer input outside the recorder process.
    ObserveInput,
    /// Automate the supported browser to observe its current state.
    AutomateBrowser,
}
