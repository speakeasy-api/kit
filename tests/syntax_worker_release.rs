use std::{
    io::Write as _,
    process::{Command, Stdio},
};

fn worker(language: &str, version: &str, source: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kit"))
        .args(["--__kit-syntax-worker", language, version])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(source).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn release_worker_fully_parses_rust_and_fails_closed() {
    let valid = worker("rust", "kit-syn-rust-v1", b"fn valid() {}\n");
    assert!(valid.status.success());
    assert_eq!(valid.stdout, br#"{"contract_version":1,"status":"pass"}"#);

    let invalid = worker("rust", "kit-syn-rust-v1", b"fn invalid(\n");
    assert!(invalid.status.success());
    assert_eq!(invalid.stdout, br#"{"contract_version":1,"status":"fail"}"#);

    assert!(
        !worker("python", "kit-tree-sitter-python-v1", b"pass\n")
            .status
            .success()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_finite_memory_limit_is_enforced_or_unavailable() {
    let output = Command::new(env!("CARGO_BIN_EXE_kit"))
        .arg("--__kit-syntax-memory-probe")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    if output.status.code() == Some(78) {
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        return;
    }
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}
