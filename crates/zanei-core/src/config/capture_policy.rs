//! Generated app-owned policy. Absence preserves standalone filter semantics.

use serde::{Deserialize, Serialize};

use super::ConfigError;

/// Consumers generate every field explicitly; missing rules never become broad defaults.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapturePolicyConfig {
    /// Display names, as used by Pantaray's allow-only settings. Empty denies all apps.
    pub allowed_apps: Vec<String>,
    pub browser: BrowserPolicy,
    pub ide: IdePolicy,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    Allow,
    Block,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserMode {
    Off,
    AllSites,
    Rules,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserPolicy {
    pub mode: BrowserMode,
    pub default_policy: PolicyAction,
    pub on_url_unavailable: PolicyAction,
    pub block_auth: bool,
    pub block_payments: bool,
    pub allow_list: Vec<BrowserUrlRule>,
    pub block_list: Vec<BrowserUrlRule>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserUrlRule {
    pub host: String,
    pub path_prefix: String,
    pub match_subdomains: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdePolicy {
    pub block_env_files: bool,
    pub on_file_name_unavailable: PolicyAction,
}

impl CapturePolicyConfig {
    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        super::validation::nonempty_unique(
            "filter.capture_policy.allowed_apps",
            &self.allowed_apps,
        )?;
        for rule in self
            .browser
            .allow_list
            .iter()
            .chain(&self.browser.block_list)
        {
            // The generator supplies URL.hostname (ASCII/IDNA, including IPv6 brackets).
            // Reject rather than reinterpret a rule that could change its allow scope.
            let canonical = url::Host::parse(&rule.host)
                .ok()
                .is_some_and(|host| host.to_string() == rule.host);
            if !canonical {
                return Err(ConfigError::InvalidListValue {
                    field: "filter.capture_policy.browser.host",
                    value: rule.host.clone(),
                });
            }
            if rule.path_prefix.trim() != rule.path_prefix {
                return Err(ConfigError::InvalidListValue {
                    field: "filter.capture_policy.browser.path_prefix",
                    value: rule.path_prefix.clone(),
                });
            }
        }
        Ok(())
    }
}
