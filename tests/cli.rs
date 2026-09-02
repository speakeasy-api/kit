use std::{fs, path::Path, process::Command};

use agentkit_core::{Item, ItemKind};

fn write_session(home: &Path, root: &Path, id: &str) -> std::path::PathBuf {
    let root = root.canonicalize().unwrap();
    let identity = blake3::hash(root.as_os_str().as_encoded_bytes());
    let directory = home
        .join(".kit/sessions")
        .join(format!("w-{}", identity.to_hex()));
    fs::create_dir_all(&directory).unwrap();
    let record = serde_json::json!({
        "schema_version": 3,
        "session_id": id,
        "generation": 1,
        "workspace_root": root,
        "item": Item::text(ItemKind::User, "Generated title"),
    });
    fs::write(
        directory.join(format!("{id}.jsonl")),
        format!("{}\n", serde_json::to_string(&record).unwrap()),
    )
    .unwrap();
    directory.join(format!("{id}.metadata.json"))
}

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

#[test]
fn sessions_rename_sets_replaces_lists_and_clears_a_display_name() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("project");
    fs::create_dir(&root).unwrap();
    let metadata = write_session(home.path(), &root, "s-abc123");

    let renamed = Command::new(env!("CARGO_BIN_EXE_kit"))
        .env("HOME", home.path())
        .args([
            "sessions",
            "rename",
            "s-abc123",
            "  OAuth token bug  ",
            "--root",
        ])
        .arg(&root)
        .output()
        .unwrap();
    assert!(renamed.status.success(), "{:?}", renamed);
    assert_eq!(
        String::from_utf8(renamed.stdout).unwrap(),
        "Renamed session s-abc123 to \"OAuth token bug\"\n"
    );
    assert_eq!(
        fs::read_to_string(&metadata).unwrap(),
        "{\"display_name\":\"OAuth token bug\"}\n"
    );

    let listed = Command::new(env!("CARGO_BIN_EXE_kit"))
        .env("HOME", home.path())
        .args(["sessions", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listing = String::from_utf8(listed.stdout).unwrap();
    assert!(
        listing.contains("s-abc123\tOAuth token bug\tGenerated title"),
        "{listing}"
    );

    let replaced = Command::new(env!("CARGO_BIN_EXE_kit"))
        .env("HOME", home.path())
        .args(["sessions", "--root"])
        .arg(&root)
        .args(["rename", "s-abc123", "Auth refresh"])
        .output()
        .unwrap();
    assert!(replaced.status.success(), "{:?}", replaced);
    assert_eq!(
        fs::read_to_string(&metadata).unwrap(),
        "{\"display_name\":\"Auth refresh\"}\n"
    );

    let cleared = Command::new(env!("CARGO_BIN_EXE_kit"))
        .env("HOME", home.path())
        .args(["sessions", "rename", "s-abc123", "--clear", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(cleared.status.success(), "{:?}", cleared);
    assert_eq!(
        String::from_utf8(cleared.stdout).unwrap(),
        "Cleared name for session s-abc123\n"
    );
    assert_eq!(fs::read_to_string(metadata).unwrap(), "{}\n");
}
