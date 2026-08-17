use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use crate::schema::App;

pub const BUILT_IN_EXCLUDED_APP_NAMES: &[&str] = &["1Password", "1Password 7", "Keychain Access"];
pub const BUILT_IN_EXCLUDED_BUNDLE_IDS: &[&str] = &[
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.apple.keychainaccess",
];

pub(crate) fn app_is_allowed(app: &App, include_only: &[String], exclude: &[String]) -> bool {
    let key = app.bundle_id.as_deref().unwrap_or(&app.name);
    if !include_only.is_empty() && !contains_case_insensitive(include_only, key) {
        return false;
    }

    !contains_case_insensitive(exclude, key) && !is_built_in_exclusion(app, key)
}

pub(crate) fn host_is_allowed(host: &str, include_only: &[String], exclude: &[String]) -> bool {
    if !include_only.is_empty()
        && !include_only
            .iter()
            .any(|domain| domain_matches(host, domain))
    {
        return false;
    }

    !exclude.iter().any(|domain| domain_matches(host, domain))
}

pub(crate) fn extract_url_host(url: &str) -> Option<String> {
    if !valid_uri_syntax(url) {
        return None;
    }

    let scheme_end = url.find(':')?;
    if !valid_scheme(&url[..scheme_end]) {
        return None;
    }

    let remainder = url[scheme_end + 1..].strip_prefix("//")?;
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    if authority.is_empty() || authority.matches('@').count() > 1 {
        return None;
    }

    let host_and_port = match authority.rsplit_once('@') {
        Some((userinfo, host_and_port)) if !userinfo.is_empty() => host_and_port,
        Some(_) => return None,
        None => authority,
    };

    parse_host_and_port(host_and_port)
}

fn contains_case_insensitive(values: &[String], expected: &str) -> bool {
    let expected = expected.to_lowercase();
    values.iter().any(|value| value.to_lowercase() == expected)
}

fn is_built_in_exclusion(app: &App, key: &str) -> bool {
    if app.bundle_id.is_some() {
        BUILT_IN_EXCLUDED_BUNDLE_IDS
            .iter()
            .any(|bundle_id| bundle_id.eq_ignore_ascii_case(key))
    } else {
        BUILT_IN_EXCLUDED_APP_NAMES
            .iter()
            .any(|name| name.to_lowercase() == key.to_lowercase())
    }
}

fn domain_matches(host: &str, configured_domain: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let domain = configured_domain.trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() || host.len() < domain.len() || !host.ends_with(&domain) {
        return false;
    }

    host.len() == domain.len() || host.as_bytes().get(host.len() - domain.len() - 1) == Some(&b'.')
}

fn valid_scheme(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

fn valid_uri_syntax(url: &str) -> bool {
    if url.is_empty() || !url.is_ascii() || url.matches('#').count() > 1 {
        return false;
    }

    let bytes = url.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == b'%' {
            if bytes
                .get(cursor + 1)
                .is_none_or(|next| !next.is_ascii_hexdigit())
                || bytes
                    .get(cursor + 2)
                    .is_none_or(|next| !next.is_ascii_hexdigit())
            {
                return false;
            }
            cursor += 3;
            continue;
        }
        if !is_uri_byte(byte) {
            return false;
        }
        cursor += 1;
    }
    true
}

fn is_uri_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b':'
                | b'/'
                | b'?'
                | b'#'
                | b'['
                | b']'
                | b'@'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
        )
}

fn parse_host_and_port(host_and_port: &str) -> Option<String> {
    if let Some(bracketed) = host_and_port.strip_prefix('[') {
        let closing = bracketed.find(']')?;
        let address = &bracketed[..closing];
        Ipv6Addr::from_str(address).ok()?;
        validate_port(&bracketed[closing + 1..])?;
        return Some(address.to_ascii_lowercase());
    }

    let (host, port) = match host_and_port.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, Some(port)),
        Some(_) => return None,
        None => (host_and_port, None),
    };
    if let Some(port) = port {
        validate_numeric_port(port)?;
    }

    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    if host
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        Ipv4Addr::from_str(host).ok()?;
    } else if !valid_domain_name(host) {
        return None;
    }

    Some(host.to_ascii_lowercase())
}

fn validate_port(remainder: &str) -> Option<()> {
    if remainder.is_empty() {
        Some(())
    } else {
        validate_numeric_port(remainder.strip_prefix(':')?)
    }
}

fn validate_numeric_port(port: &str) -> Option<()> {
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    port.parse::<u16>().ok().map(|_| ())
}

fn valid_domain_name(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, bundle_id: Option<&str>) -> App {
        App {
            name: name.to_owned(),
            bundle_id: bundle_id.map(str::to_owned),
            pid: Some(42),
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn bundle_id_is_the_only_key_when_present() {
        let safari = app("Safari", Some("com.apple.Safari"));
        assert!(!app_is_allowed(&safari, &strings(&["Safari"]), &[]));
        assert!(app_is_allowed(
            &safari,
            &strings(&["COM.APPLE.SAFARI"]),
            &[]
        ));
    }

    #[test]
    fn exclusion_wins_after_include_only_gate() {
        let safari = app("Safari", Some("com.apple.Safari"));
        let key = strings(&["com.apple.safari"]);
        assert!(!app_is_allowed(&safari, &key, &key));
    }

    #[test]
    fn built_in_exclusions_cover_names_and_bundle_ids() {
        assert!(!app_is_allowed(&app("1Password", None), &[], &[]));
        assert!(!app_is_allowed(
            &app("Anything", Some("com.apple.keychainaccess")),
            &[],
            &[]
        ));
    }

    #[test]
    fn website_match_requires_a_dot_boundary() {
        let excluded = strings(&["example.com"]);
        assert!(!host_is_allowed("example.com", &[], &excluded));
        assert!(!host_is_allowed("api.example.com", &[], &excluded));
        assert!(host_is_allowed("evil-example.com", &[], &excluded));
        assert!(host_is_allowed("example.com.evil", &[], &excluded));
    }

    #[test]
    fn website_include_only_is_checked_before_exclusion() {
        let included = strings(&["example.com"]);
        assert!(host_is_allowed("api.example.com", &included, &[]));
        assert!(!host_is_allowed("other.test", &included, &[]));
        assert!(!host_is_allowed("api.example.com", &included, &included));
    }

    #[test]
    fn extracts_valid_hierarchical_url_hosts() {
        assert_eq!(
            extract_url_host("https://user:pass@Example.COM:8443/path?q=1"),
            Some("example.com".to_owned())
        );
        assert_eq!(
            extract_url_host("chrome://settings/privacy"),
            Some("settings".to_owned())
        );
        assert_eq!(
            extract_url_host("https://[2001:db8::1]:443/"),
            Some("2001:db8::1".to_owned())
        );
    }

    #[test]
    fn rejects_malformed_or_hostless_urls() {
        for url in [
            "not a url",
            "file:///tmp/private",
            "https:///missing-host",
            "https://example.com:99999/",
            "https://bad_host.example/",
            "https://999.1.1.1/",
            "https://example.com/%zz",
            "https://example.com/<private>",
        ] {
            assert_eq!(extract_url_host(url), None, "{url}");
        }
    }
}
