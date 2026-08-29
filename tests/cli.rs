use std::{fs, process::Command};

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
