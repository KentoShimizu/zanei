use std::process::Command;

const SERVICE_ENV: &str = "ZANEI_KEYCHAIN_SERVICE";
const LABEL_ENV: &str = "ZANEI_KEYCHAIN_LABEL";
const NO_PROMPT_ENV: &str = "ZANEI_KEYCHAIN_NO_PROMPT";

fn zanei() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zanei"));
    command
        .env_remove(SERVICE_ENV)
        .env_remove(LABEL_ENV)
        .env_remove(NO_PROMPT_ENV);
    command
}

#[test]
fn custom_keychain_identity_rejects_launchd_entrypoints() {
    for command in ["start", "__daemon"] {
        let output = zanei()
            .arg(command)
            .env(SERVICE_ENV, "dev.example.subject.store")
            .env(LABEL_ENV, "Example subject context store key")
            .output()
            .expect("run zanei");

        assert_eq!(output.status.code(), Some(2), "command: {command}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("custom Keychain identity requires `zanei start --foreground`"),
            "command: {command}"
        );
    }
}

#[test]
fn partial_keychain_identity_is_an_error() {
    let output = zanei()
        .arg("status")
        .env(SERVICE_ENV, "dev.example.subject.store")
        .output()
        .expect("run zanei");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("ZANEI_KEYCHAIN_SERVICE and ZANEI_KEYCHAIN_LABEL must be set together")
    );
}
