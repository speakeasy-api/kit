//! Local-only ACP parent/child transport coverage. Synthetic child notices do
//! not establish provider retry accounting; provider fixtures cover that layer.
#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

#[test]
fn acp_v1_v2_forward_foreground_and_background_child_retry_notices() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/acp-retry-parent-delivery.py"
        ))
        .arg(env!("CARGO_BIN_EXE_kit"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
