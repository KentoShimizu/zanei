//! Pure policy evaluation for the accepted browser/AX/pipeline consumers.
//! This does not acquire a surface, verify its freshness, or replace Secure Input/private guards.

use crate::config::CapturePolicyConfig;
use crate::config::capture_policy::{BrowserMode, BrowserPolicy, BrowserUrlRule, PolicyAction};
use crate::schema::App;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureDeniedReason {
    ProtectedApp,
    AppNotAllowed,
    BrowserOff,
    UrlUnavailable,
    BlockedUrl,
    AuthenticationUrl,
    PaymentUrl,
    DefaultPolicy,
    FileNameUnavailable,
    EnvFile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapturePolicyDecision {
    Allow,
    Deny(CaptureDeniedReason),
}

/// Evaluates one already-bound app/window/URL observation before capture or persistence.
/// Callers must reject stale/unbound surfaces and secure/private states before this call;
/// `None` means an unavailable observation, never permission to reuse an old URL/title.
#[must_use]
pub fn evaluate_capture_policy(
    policy: &CapturePolicyConfig,
    app: &App,
    window_title: Option<&str>,
    browser_url: Option<&str>,
) -> CapturePolicyDecision {
    use CaptureDeniedReason as Reason;
    use CapturePolicyDecision::{Allow, Deny};
    if !super::matcher::app_is_allowed(app, &[], &[]) {
        return Deny(Reason::ProtectedApp);
    }
    let name = app.name.trim().to_lowercase();
    if !policy
        .allowed_apps
        .iter()
        .any(|allowed| allowed.to_lowercase() == name)
    {
        return Deny(Reason::AppNotAllowed);
    }
    if matches!(name.as_str(), "google chrome" | "safari") {
        return evaluate_browser(&policy.browser, browser_url);
    }
    if matches!(name.as_str(), "cursor" | "visual studio code" | "code")
        && policy.ide.block_env_files
    {
        match window_title.and_then(ide_file_name) {
            None if policy.ide.on_file_name_unavailable == PolicyAction::Block => {
                return Deny(Reason::FileNameUnavailable);
            }
            Some(name) if is_env_file(&name) => return Deny(Reason::EnvFile),
            _ => {}
        }
    }
    Allow
}

fn evaluate_browser(rules: &BrowserPolicy, raw_url: Option<&str>) -> CapturePolicyDecision {
    use CaptureDeniedReason as Reason;
    use CapturePolicyDecision::{Allow, Deny};
    if rules.mode == BrowserMode::Off {
        return Deny(Reason::BrowserOff);
    }
    let parsed = raw_url
        .and_then(|raw| url::Url::parse(raw).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some());
    let Some(url) = parsed else {
        return action(rules.on_url_unavailable, Reason::UrlUnavailable);
    };
    let matches = |rule: &BrowserUrlRule| {
        let host = url.host_str().expect("web URL has a host");
        (host == rule.host || rule.match_subdomains && host.ends_with(&format!(".{}", rule.host)))
            && url.path().starts_with(&rule.path_prefix)
    };
    if rules.block_list.iter().any(matches) {
        return Deny(Reason::BlockedUrl);
    }
    if let Some(reason) = preset_match(rules, url.path()) {
        return Deny(reason);
    }
    if rules.mode == BrowserMode::AllSites || rules.allow_list.iter().any(matches) {
        return Allow;
    }
    action(rules.default_policy, Reason::DefaultPolicy)
}

fn action(action: PolicyAction, reason: CaptureDeniedReason) -> CapturePolicyDecision {
    match action {
        PolicyAction::Allow => CapturePolicyDecision::Allow,
        PolicyAction::Block => CapturePolicyDecision::Deny(reason),
    }
}

fn preset_match(rules: &BrowserPolicy, path: &str) -> Option<CaptureDeniedReason> {
    // URL-only representative patterns, not detection of every auth/payment screen.
    // Exact documentation routes such as /docs/oauth also match by design.
    let mut previous = String::new();
    let mut payment = false;
    for raw in path.split('/') {
        let segment = normalized_segment(raw);
        if rules.block_auth
            && (matches!(
                segment.as_str(),
                "login"
                    | "log-in"
                    | "signin"
                    | "sign-in"
                    | "sign_in"
                    | "signup"
                    | "sign-up"
                    | "sign_up"
                    | "register"
                    | "registration"
                    | "oauth"
                    | "oauth2"
                    | "authorize"
                    | "reset-password"
                    | "reset_password"
                    | "password-reset"
                    | "password_reset"
                    | "forgot-password"
                    | "forgot_password"
            ) || previous == "password" && matches!(segment.as_str(), "reset" | "forgot"))
        {
            return Some(CaptureDeniedReason::AuthenticationUrl);
        }
        payment |= rules.block_payments
            && matches!(
                segment.as_str(),
                "checkout" | "payment" | "payments" | "billing" | "subscription" | "subscriptions"
            );
        previous = segment;
    }
    payment.then_some(CaptureDeniedReason::PaymentUrl)
}

fn normalized_segment(raw: &str) -> String {
    let mut bytes = raw.bytes();
    let mut normalized = Vec::with_capacity(raw.len());
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let mut lookahead = bytes.clone();
            let decoded = lookahead
                .next()
                .and_then(hex_digit)
                .zip(lookahead.next().and_then(hex_digit))
                .map(|(high, low)| high * 16 + low);
            if let Some(value) = decoded.filter(|value| {
                value.is_ascii_alphanumeric() || matches!(value, b'-' | b'.' | b'_' | b'~')
            }) {
                bytes = lookahead;
                normalized.push(value.to_ascii_lowercase());
                continue;
            }
        }
        normalized.push(byte.to_ascii_lowercase());
    }
    String::from_utf8(normalized).expect("ASCII decoding preserves UTF-8")
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn ide_file_name(title: &str) -> Option<String> {
    let title = title.replace(['\u{2013}', '\u{2014}'], "-");
    let mut start = 0;
    for (index, _) in title.match_indices('-') {
        if title[..index]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
            && title[index + 1..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
        {
            if let Some(name) = file_name_candidate(&title[start..index]) {
                return Some(name);
            }
            start = index + 1;
        }
    }
    file_name_candidate(&title[start..])
}

fn file_name_candidate(candidate: &str) -> Option<String> {
    let clean = candidate.replace(['●', '•'], "");
    let name = clean.trim();
    if name.encode_utf16().count() > 255 || name.contains(['/', '\\']) {
        return None;
    }
    let lower = name.to_lowercase();
    let extension = name.rsplit_once('.');
    (lower == ".env"
        || lower.starts_with(".env.")
        || extension.is_some_and(|(stem, ext)| {
            !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= 10
                && ext.bytes().all(|byte| byte.is_ascii_alphanumeric())
        }))
    .then(|| name.to_owned())
}

fn is_env_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    (lower == ".env"
        || lower
            .strip_prefix(".env.")
            .is_some_and(|suffix| !suffix.is_empty()))
        && !lower.contains(".example")
}
