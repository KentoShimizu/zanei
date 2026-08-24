use std::fmt::Display;

use super::model::{CaptureScopeReport, StatusReport, StoreWriteState};

pub(super) fn print_human(report: &StatusReport) {
    println!("STATE             {}", report.state.as_str());
    println!("PAUSED            {}", display_optional(report.paused));
    println!(
        "SINCE             {}",
        display_optional(report.since.as_deref())
    );
    println!(
        "INSTANCE          {}",
        display_optional(report.instance.as_deref())
    );
    println!(
        "MODE              {}",
        display_optional(report.mode.as_deref())
    );
    println!(
        "EVENTS CAPTURED   {}",
        display_optional(report.events_captured)
    );
    println!(
        "EVENTS DROPPED    {}",
        display_optional(report.events_dropped)
    );
    println!(
        "LAST EVENT        {}",
        display_optional(report.last_event_ts.as_deref())
    );
    println!("HEARTBEAT         {}", heartbeat_text(report));
    println!(
        "STORE WRITES      {}",
        report
            .store_write_state
            .map_or("-", StoreWriteState::as_str)
    );
    println!(
        "STORE             {}{}",
        report.store.path,
        match report.store.encryption {
            Some("sqlcipher") => " (encrypted)",
            Some(_) => " (plaintext; the recorder encrypts it on its next start)",
            None => "",
        }
    );
    for retired in &report.store.retired_plaintext {
        println!("PREVIOUS STORE    {retired} (plaintext; read until it ages out of retention)");
    }
    print_capture_setting(
        "TEXT CONTENT      ",
        report.capture.text_content,
        &report.capture.text_scope,
        "capture.text_content",
    );
    print_capture_setting(
        "CONTENT SNAPSHOT  ",
        report.capture.content_snapshot,
        &report.capture.snapshot_scope,
        "capture.content_snapshot",
    );
    println!("PERMISSIONS OK    {}", report.permissions_ok);
    if report.degraded.is_empty() {
        println!("DEGRADED          false");
    } else {
        println!("DEGRADED          true");
        for (component, reason) in &report.degraded {
            println!("  {component}: {reason}");
        }
    }
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

fn print_capture_setting(label: &str, enabled: bool, scope: &CaptureScopeReport, config_key: &str) {
    if enabled {
        println!("{label}on (apps: {}, sites: {})", scope.apps, scope.sites);
    } else {
        println!("{label}off (opt-in: zanei config set {config_key} true)");
    }
}
