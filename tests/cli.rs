use std::{fs, process::Command};

#[test]
fn terminal_auth_arguments_run_login_from_acp_server_invocations() {
    let home = tempfile::tempdir().unwrap();

    for command in ["acp", "serve"] {
        let output = Command::new(env!("CARGO_BIN_EXE_kit"))
            .env("HOME", home.path())
            .args([
                command,
                "--credential-store",
                "memory",
                "--terminal-auth-login",
                "openai",
            ])
            .output()
            .unwrap();

        assert!(!output.status.success(), "{command} unexpectedly started");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("provider login cannot use memory credential storage"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn sessions_rejects_missing_and_non_directory_roots() {
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("missing");
    let output = Command::new(env!("CARGO_BIN_EXE_kit"))
        .env("HOME", home.path())
        .args(["sessions", "--root"])
        .arg(&missing)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not resolve workspace root"));

    let file = home.path().join("workspace-file");
    fs::write(&file, b"not a directory").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kit"))
        .env("HOME", home.path())
        .args(["sessions", "--root"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("is not a directory"));
}
