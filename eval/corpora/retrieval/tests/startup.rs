#![cfg(target_os = "macos")]

use kit_retrieval_eval::{LocalSandboxRequest, SandboxOutcome, run_local_sandbox};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[test]
fn built_worker_reaches_main_without_exposing_home_oracle_or_network() {
    let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "kit-w07-startup-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let source = root.join("source");
    let requests = root.join("requests");
    let output = root.join("output");
    let arbitrary_home = root.join("arbitrary-home");
    let oracle = root.join("oracle");
    for path in [&source, &requests, &output, &arbitrary_home, &oracle] {
        fs::create_dir_all(path).unwrap();
    }
    let arbitrary_home_file = arbitrary_home.join("secret");
    let oracle_file = oracle.join("oracle.json");
    fs::write(&arbitrary_home_file, "private").unwrap();
    fs::write(&oracle_file, "hidden").unwrap();
    let request_file = requests.join("startup-probe.json");
    fs::write(
        &request_file,
        serde_json::to_vec(&serde_json::json!({
            "arbitrary_home_file": arbitrary_home_file,
            "oracle_file": oracle_file,
        }))
        .unwrap(),
    )
    .unwrap();

    let executable = PathBuf::from(env!("CARGO_BIN_EXE_w07-retrieval"));
    let outcome = run_local_sandbox(LocalSandboxRequest {
        executable: executable.clone(),
        expected_executable_digest: digest(&executable),
        allowed_executables: Vec::new(),
        arguments: vec![
            "worker-startup-probe".into(),
            requests.clone().into_os_string(),
            output.clone().into_os_string(),
        ],
        source_snapshot: source,
        readonly_roots: Vec::new(),
        request_files: vec![request_file],
        writable_roots: vec![output.clone()],
        forbidden_paths: vec![oracle],
        max_duration: Duration::from_secs(5),
        capture_stderr: true,
    })
    .unwrap();
    match outcome {
        SandboxOutcome::Exited {
            status,
            stderr_first_line,
        } => assert!(status.success(), "startup failed: {stderr_first_line:?}"),
        SandboxOutcome::TimedOut { stderr_first_line } => {
            panic!("startup timed out: {stderr_first_line:?}")
        }
    }
    assert_eq!(
        fs::read(output.join("startup-probe.json")).unwrap(),
        b"{\"arbitrary_home_denied\":true,\"network_denied\":true,\"oracle_denied\":true,\"reached_main\":true}\n"
    );
    fs::remove_dir_all(root).unwrap();
}

fn digest(path: &Path) -> String {
    format!("sha256:{:x}", Sha256::digest(fs::read(path).unwrap()))
}
