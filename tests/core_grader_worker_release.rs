use std::process::{Command, Stdio};

#[test]
fn core_grader_worker_requires_its_exact_argv_shape() {
    let exact = Command::new(env!("CARGO_BIN_EXE_kit"))
        .arg("__kit-core-grader")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!exact.status.success());
    assert!(String::from_utf8_lossy(&exact.stderr).contains("invalid request:"));

    let extra = Command::new(env!("CARGO_BIN_EXE_kit"))
        .args(["__kit-core-grader", "extra"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!extra.status.success());
    let stderr = String::from_utf8_lossy(&extra.stderr);
    assert!(stderr.contains("unrecognized subcommand '__kit-core-grader'"));
    assert!(!stderr.contains("invalid request:"));
}
