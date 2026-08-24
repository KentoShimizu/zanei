use std::process::Command;

use zanei_core::{
    config::Config,
    store::{QueryFilter, StoreReader},
};

mod support;

use support::{Fixture, STORE_KEY_FILE_ENV};

const TEST_RETENTION_HOURS: u64 = 24 * 365 * 100;

#[test]
fn repeated_content_snapshot_enable_skips_non_tty_and_tty_consent() {
    let fixture = Fixture::empty();
    let mut config = Config::load(&fixture.config).expect("load fixture config");
    config.capture.content_snapshot = true;
    zanei_core::config::save(&config, &fixture.config).expect("enable content snapshots");

    let non_tty = fixture
        .command()
        .args(["config", "set", "capture.content_snapshot", "true"])
        .output()
        .expect("repeat enable without TTY");
    assert!(non_tty.status.success());
    assert_eq!(
        String::from_utf8(non_tty.stdout).expect("non-TTY stdout"),
        "No change: capture.content_snapshot\n"
    );
    assert!(non_tty.stderr.is_empty());

    let tty = Command::new("/usr/bin/script")
        .args(["-q", "/dev/null", env!("CARGO_BIN_EXE_zanei")])
        .env(STORE_KEY_FILE_ENV, &fixture.key_file)
        .arg("--config")
        .arg(&fixture.config)
        .arg("--store")
        .arg(&fixture.store)
        .args(["config", "set", "capture.content_snapshot", "true"])
        .output()
        .expect("repeat enable in a pseudo-TTY");
    assert!(tty.status.success());
    let tty_output = String::from_utf8_lossy(&tty.stdout);
    assert!(tty_output.contains("No change: capture.content_snapshot"));
    assert!(!tty_output.contains("Enable content snapshots with this scope?"));
}

#[test]
fn universal_types_prompt_but_narrow_types_do_not() {
    let fixture = Fixture::populated();
    let event_count = query(&fixture, "*").len();

    let universal = fixture
        .command()
        .args(["purge", "--types", "*"])
        .output()
        .expect("unconfirmed universal purge");
    assert!(universal.status.success());
    assert_eq!(
        String::from_utf8(universal.stdout).expect("universal purge stdout"),
        "Purge cancelled\n"
    );
    assert!(String::from_utf8_lossy(&universal.stderr).contains("Delete all stored Zanei events?"));
    assert_eq!(query(&fixture, "*").len(), event_count);

    let scoped = fixture
        .command()
        .args(["purge", "--types", "content.*"])
        .output()
        .expect("narrow type purge");
    assert!(scoped.status.success());
    assert!(scoped.stderr.is_empty());
    assert!(String::from_utf8_lossy(&scoped.stdout).starts_with("Purged "));
    assert!(query(&fixture, "content.*").is_empty());
}

fn query(fixture: &Fixture, event_type: &str) -> Vec<zanei_core::schema::Event> {
    StoreReader::open_with_key(&fixture.store, Some(&fixture.key()))
        .and_then(|reader| {
            reader.query(
                &QueryFilter {
                    types: vec![event_type.to_owned()],
                    ..QueryFilter::default()
                },
                TEST_RETENTION_HOURS,
            )
        })
        .expect("query fixture")
        .events
}
