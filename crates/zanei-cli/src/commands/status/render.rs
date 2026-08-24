use std::fmt::Display;

use super::model::{CaptureScopeReport, StatusReport, StoreWriteState};

pub(super) fn print_human(report: &StatusReport) {
    print!("{}", render_human(report));
}

pub(super) fn render_human(report: &StatusReport) -> String {
    let mut lines = vec![
        format!("STATE             {}", report.state.as_str()),
        format!("PAUSED            {}", display_optional(report.paused)),
        format!(
            "SINCE             {}",
            display_optional(report.since.as_deref())
        ),
        format!(
            "INSTANCE          {}",
            display_optional(report.instance.as_deref())
        ),
        format!(
            "MODE              {}",
            display_optional(report.mode.as_deref())
        ),
        format!(
            "EVENTS CAPTURED   {}",
            display_optional(report.events_captured)
        ),
        format!(
            "EVENTS DROPPED    {}",
            display_optional(report.events_dropped)
        ),
        format!(
            "LAST EVENT        {}",
            display_optional(report.last_event_ts.as_deref())
        ),
        format!("HEARTBEAT         {}", heartbeat_text(report)),
        format!(
            "STORE WRITES      {}",
            report
                .store_write_state
                .map_or("-", StoreWriteState::as_str)
        ),
        format!(
            "STORE             {}{}",
            report.store.path,
            match report.store.encryption {
                Some("sqlcipher") => " (encrypted)",
                Some(_) => " (plaintext; the recorder encrypts it on its next start)",
                None => "",
            }
        ),
    ];
    lines.extend(report.store.retired_plaintext.iter().map(|retired| {
        format!("PREVIOUS STORE    {retired} (plaintext; read until it ages out of retention)")
    }));
    lines.push(capture_setting(
        "TEXT CONTENT      ",
        report.capture.text_content,
        &report.capture.text_scope,
        "capture.text_content",
    ));
    lines.push(capture_setting(
        "CONTENT SNAPSHOT  ",
        report.capture.content_snapshot,
        &report.capture.snapshot_scope,
        "capture.content_snapshot",
    ));
    lines.push(format!("PERMISSIONS OK    {}", report.permissions_ok));
    match &report.collector_failures {
        None => lines.push("COLLECTOR FAILURES -".to_owned()),
        Some(failures) if failures.is_empty() => {
            lines.push("COLLECTOR FAILURES none".to_owned());
        }
        Some(failures) => {
            lines.push("COLLECTOR FAILURES".to_owned());
            lines.extend(
                failures
                    .iter()
                    .map(|(component, count)| format!("  {component}: {count}")),
            );
        }
    }
    if report.degraded.is_empty() {
        lines.push("DEGRADED          false".to_owned());
    } else {
        lines.push("DEGRADED          true".to_owned());
        lines.extend(
            report
                .degraded
                .iter()
                .map(|(component, reason)| format!("  {component}: {reason}")),
        );
    }
    format!("{}\n", lines.join("\n"))
}

fn display_optional<T: Display>(value: Option<T>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

fn heartbeat_text(report: &StatusReport) -> String {
    report.heartbeat_freshness.map_or_else(
        || "-".to_owned(),
        |freshness| {
            format!(
                "{}{}",
                freshness.as_str(),
                report
                    .heartbeat_age_s
                    .map(|age| format!(" ({age}s old)"))
                    .unwrap_or_default()
            )
        },
    )
}

fn capture_setting(
    label: &str,
    enabled: bool,
    scope: &CaptureScopeReport,
    config_key: &str,
) -> String {
    if enabled {
        format!("{label}on (apps: {}, sites: {})", scope.apps, scope.sites)
    } else {
        format!("{label}off (opt-in: zanei config set {config_key} true)")
    }
}
