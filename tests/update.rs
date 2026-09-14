//! Update discovery must not invoke a package manager during a dry run.
#![allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

use std::{fs, process::Command};

#[test]
fn cargo_dry_run_reads_provenance_without_running_tools_or_writing() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let kit = bin.join("kit");
    fs::copy(env!("CARGO_BIN_EXE_kit"), &kit).unwrap();
    let metadata =
        "[v1]\n'kit 0.2.1 (git+https://example.invalid/kit?branch=stable#abc)' = ['kit']\n";
    fs::write(root.path().join(".crates.toml"), metadata).unwrap();
    let output = Command::new(&kit)
        .args(["update", "--dry-run"])
        .env("HOME", root.path().join("nonexistent-home"))
        .env("PATH", "")
        .output()
        .unwrap();
    // Containers intentionally refuse any executable mutation plan.
    if std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
    {
        assert!(!output.status.success());
        return;
    }
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Method: Cargo"));
    assert!(stdout.contains("--branch"));
    assert!(stdout.contains("stable"));
    assert!(stdout.contains("Dry run"));
    assert!(!root.path().join("nonexistent-home").exists());
    assert_eq!(
        fs::read_to_string(root.path().join(".crates.toml")).unwrap(),
        metadata
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
}

#[test]
fn update_help_and_unknown_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_kit"))
        .args(["update", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("--dry-run"));
    let output = Command::new(env!("CARGO_BIN_EXE_kit"))
        .args(["update", "--force"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn mise_dry_run_refuses_with_upgrade_guidance_and_no_writes() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("custom data");
    let bin = data.join("installs/github-speakeasy-api-kit/0.2.1/bin");
    fs::create_dir_all(&bin).unwrap();
    let kit = bin.join("kit");
    fs::copy(env!("CARGO_BIN_EXE_kit"), &kit).unwrap();
    let output = Command::new(&kit)
        .args(["update", "--dry-run"])
        .env("HOME", root.path().join("missing-home"))
        .env("MISE_DATA_DIR", &data)
        .env_remove("MISE_INSTALLS_DIR")
        .env("KIT_INSTALL_DIR", &bin)
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("managed by mise"), "{stderr}");
    assert!(stderr.contains("mise upgrade github:speakeasy-api/kit"));
    assert!(!root.path().join("missing-home").exists());
    assert_eq!(fs::read_dir(&bin).unwrap().count(), 1);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}
