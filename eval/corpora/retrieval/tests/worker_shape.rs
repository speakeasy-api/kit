#![cfg(target_os = "macos")]

use kit_retrieval_eval::{
    Arm, ArmConfig, ExecutorEvidence, LocalSandboxRequest, LocalWorkerSandboxRequest, RawTrial,
    RetrievalSource, SandboxOutcome, SourceStatus, TrialTerminal, WorkerArmRequest, WorkerQuery,
    run_local_sandbox, run_local_worker_sandbox,
};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[test]
fn exact_worker_shape_opens_workspace_and_preserves_isolation() {
    let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "kit-w07-worker-shape-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let source = root.join("source");
    let inputs = root.join("inputs");
    let output = root.join("output");
    let cache = root.join("cache");
    let temp = root.join("tmp");
    let oracle = root.join("oracle");
    let arbitrary_home = root.join("arbitrary-home");
    for path in [
        source.join(".git"),
        source.join("src"),
        inputs.clone(),
        output.clone(),
        cache.clone(),
        temp.clone(),
        oracle.clone(),
        arbitrary_home.clone(),
    ] {
        fs::create_dir_all(path).unwrap();
    }
    let rust = b"/// Tiny fixture.\npub fn needle() {}\n";
    fs::write(source.join("src/lib.rs"), rust).unwrap();
    fs::write(oracle.join("answer.json"), "hidden").unwrap();
    fs::write(arbitrary_home.join("secret"), "private").unwrap();

    let executable = root.join("w07-worker");
    fs::copy(env!("CARGO_BIN_EXE_w07-retrieval"), &executable).unwrap();
    let executable_digest = digest(&executable);
    let source_digest = digest_json(&BTreeMap::from([(
        "src/lib.rs",
        format!("{:x}", Sha256::digest(rust)),
    )]));
    let query = WorkerQuery {
        task_id: "tiny-task".into(),
        query: "Locate the public Rust item documented as: \"needle\"".into(),
        query_digest: digest_bytes(b"Locate the public Rust item documented as: \"needle\""),
    };
    let request = WorkerArmRequest {
        unit_id: "tiny-unit".into(),
        repository_class: kit_retrieval_eval::RepositoryClass::Small,
        source_digest: source_digest.clone(),
        admission_digest: digest_bytes(b"tiny-admission"),
        executor_evidence: ExecutorEvidence::LocalSandboxNotTrusted,
        cache_id: digest_bytes(b"tiny-cache"),
        worker_executable_digest: executable_digest.clone(),
        git_path: git_path().to_string_lossy().into_owned(),
        git_executable_digest: digest(&git_path()),
        git_version: git_version(),
        config: ArmConfig::frozen(Arm::L),
    };
    let query_path = inputs.join("query.json");
    let request_path = inputs.join("arm.json");
    fs::write(&query_path, serde_json::to_vec(&query).unwrap()).unwrap();
    fs::write(&request_path, serde_json::to_vec(&request).unwrap()).unwrap();

    let query_link = inputs.join("query-link.json");
    symlink(&query_path, &query_link).unwrap();
    assert!(
        run_worker(
            &executable,
            &executable_digest,
            &source,
            &query_link,
            &request_path,
            &output.join("query-link-raw.json"),
            &output,
            &cache,
            &temp,
            &oracle,
        )
        .is_err()
    );
    fs::remove_file(query_link).unwrap();

    let request_link = inputs.join("request-link.json");
    symlink(&request_path, &request_link).unwrap();
    assert!(
        run_worker(
            &executable,
            &executable_digest,
            &source,
            &query_path,
            &request_link,
            &output.join("request-link-raw.json"),
            &output,
            &cache,
            &temp,
            &oracle,
        )
        .is_err()
    );
    fs::remove_file(request_link).unwrap();

    let raw_path = output.join("raw.json");
    let outcome = run_worker(
        &executable,
        &executable_digest,
        &source,
        &query_path,
        &request_path,
        &raw_path,
        &output,
        &cache,
        &temp,
        &oracle,
    )
    .unwrap();
    assert!(matches!(outcome, SandboxOutcome::Exited { status, .. } if status.success()));
    let raw_bytes = fs::read(&raw_path).unwrap();
    let schema: serde_json::Value =
        serde_json::from_slice(include_bytes!("../schema/v2/raw-trial.schema.json")).unwrap();
    jsonschema::validator_for(&schema)
        .unwrap()
        .validate(&serde_json::from_slice(&raw_bytes).unwrap())
        .unwrap();
    let raw: RawTrial = serde_json::from_slice(&raw_bytes).unwrap();
    assert_eq!(raw.unit_id, "tiny-unit");
    assert_eq!(raw.arm, Arm::L);
    assert_eq!(raw.source_digest, source_digest);
    assert_eq!(raw.terminal, TrialTerminal::Complete);
    assert!(raw.worker_error.is_none());
    assert!(raw.observations.iter().any(|observation| {
        observation.source == RetrievalSource::Lexical
            && observation.status == SourceStatus::Available
            && observation.complete_candidate_count == observation.candidates.len()
    }));
    assert!(
        !String::from_utf8(raw_bytes)
            .unwrap()
            .contains(&root.to_string_lossy().to_string())
    );

    for denied in [arbitrary_home.join("secret"), oracle.join("answer.json")] {
        assert_denied(
            Path::new("/bin/cat"),
            vec![denied.into_os_string()],
            &source,
            &query_path,
            &output,
            &cache,
            &temp,
            &oracle,
        );
    }
    assert_denied(
        Path::new("/usr/bin/nc"),
        vec!["-l".into(), "127.0.0.1".into(), "54321".into()],
        &source,
        &query_path,
        &output,
        &cache,
        &temp,
        &oracle,
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn exact_worker_shape_uses_one_root_owner_and_distinct_nested_history_owner() {
    let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "kit-w07-worker-history-shape-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let executable = root.join("w07-worker");
    fs::copy(env!("CARGO_BIN_EXE_w07-retrieval"), &executable).unwrap();
    let executable_digest = digest(&executable);

    for (name, nested, arm) in [
        ("root-fp", false, Arm::FP),
        ("nested-fp", true, Arm::FP),
        ("nested-fs", true, Arm::FS),
    ] {
        run_history_worker_case(&root, &executable, &executable_digest, name, nested, arm);
    }
    fs::remove_dir_all(root).unwrap();
}

fn run_history_worker_case(
    root: &Path,
    executable: &Path,
    executable_digest: &str,
    name: &str,
    nested: bool,
    arm: Arm,
) {
    let case = root.join(name);
    let upstream = case.join("upstream");
    let repository = case.join("repository");
    let package = if nested {
        upstream.join("crate")
    } else {
        upstream.clone()
    };
    fs::create_dir_all(package.join("src")).unwrap();
    let rust = b"/// Finds alpha.\npub fn alpha() {}\n";
    fs::write(package.join("src/lib.rs"), rust).unwrap();
    git(&upstream, &["init"]);
    git(&upstream, &["add", "."]);
    git(
        &upstream,
        &[
            "-c",
            "user.name=W07 Test",
            "-c",
            "user.email=w07@example.invalid",
            "commit",
            "-m",
            "fixture",
        ],
    );
    git(
        &upstream,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            repository.to_str().unwrap(),
            "HEAD",
        ],
    );

    let source = if nested {
        repository.join("crate")
    } else {
        repository.clone()
    };
    let inputs = case.join("inputs");
    let output = case.join("output");
    let cache = case.join("cache");
    let temp = case.join("tmp");
    let oracle = case.join("oracle");
    for path in [&inputs, &output, &cache, &temp, &oracle] {
        fs::create_dir(path).unwrap();
    }
    let query_text = "Locate the public Rust item documented as: \"Finds alpha.\"";
    let query = WorkerQuery {
        task_id: format!("{name}-task"),
        query: query_text.into(),
        query_digest: digest_bytes(query_text.as_bytes()),
    };
    let config = ArmConfig::frozen(arm);
    let request = WorkerArmRequest {
        unit_id: format!("{name}-unit"),
        repository_class: kit_retrieval_eval::RepositoryClass::Small,
        source_digest: digest_json(&BTreeMap::from([(
            "src/lib.rs",
            format!("{:x}", Sha256::digest(rust)),
        )])),
        admission_digest: digest_bytes(format!("{name}-admission").as_bytes()),
        executor_evidence: ExecutorEvidence::LocalSandboxNotTrusted,
        cache_id: digest_bytes(format!("{name}-cache").as_bytes()),
        worker_executable_digest: executable_digest.into(),
        git_path: git_path().to_string_lossy().into_owned(),
        git_executable_digest: digest(&git_path()),
        git_version: git_version(),
        config: config.clone(),
    };
    let query_path = inputs.join("query.json");
    let request_path = inputs.join("arm.json");
    let raw_path = output.join("raw.json");
    fs::write(&query_path, serde_json::to_vec(&query).unwrap()).unwrap();
    fs::write(&request_path, serde_json::to_vec(&request).unwrap()).unwrap();
    let metadata = upstream.join(".git").canonicalize().unwrap();
    let mut readonly_roots = vec![metadata.clone()];
    if nested {
        readonly_roots.push(repository.clone());
    }
    let outcome = run_local_worker_sandbox(LocalWorkerSandboxRequest {
        executable: executable.into(),
        expected_executable_digest: executable_digest.into(),
        git_executable: git_path(),
        source_snapshot: source,
        git_metadata_root: metadata,
        query_path,
        request_path,
        output_path: raw_path.clone(),
        output_root: output,
        cache_root: cache.clone(),
        temporary_root: temp,
        readonly_roots,
        forbidden_paths: vec![oracle],
        max_duration: Duration::from_secs(30),
    })
    .unwrap();
    match outcome {
        SandboxOutcome::Exited { status, .. } if status.success() => {}
        other => panic!("history worker failed: {other:?}"),
    }
    let raw: RawTrial = serde_json::from_slice(&fs::read(raw_path).unwrap()).unwrap();
    assert_eq!(
        raw.observations
            .iter()
            .map(|observation| observation.source)
            .collect::<Vec<_>>(),
        config.enabled_sources
    );
    assert_eq!(
        raw.syntax_initializations,
        usize::from(config.syntax_initialization_permitted)
    );
    let history_source = if arm == Arm::FS {
        RetrievalSource::GitPathHistory
    } else {
        RetrievalSource::History
    };
    let expected_git_digest = digest(&git_path());
    assert!(raw.observations.iter().any(|observation| {
        observation.source == history_source
            && observation.status == SourceStatus::Available
            && !observation.candidates.is_empty()
            && observation.git_executable_digest.as_deref() == Some(expected_git_digest.as_str())
    }));
    let state_count = fs::read_dir(&cache)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "state")
        })
        .count();
    assert_eq!(state_count, if nested { 2 } else { 1 });
    assert_eq!(cache.join("history-revision.state").exists(), nested);
}

fn git(root: &Path, arguments: &[&str]) {
    let status = Command::new(git_path())
        .args(arguments)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "git command failed: {arguments:?}");
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    executable: &Path,
    executable_digest: &str,
    source: &Path,
    query: &Path,
    request: &Path,
    raw: &Path,
    output: &Path,
    cache: &Path,
    temp: &Path,
    oracle: &Path,
) -> Result<SandboxOutcome, Box<dyn std::error::Error>> {
    run_local_worker_sandbox(LocalWorkerSandboxRequest {
        executable: executable.to_path_buf(),
        expected_executable_digest: executable_digest.into(),
        git_executable: git_path(),
        source_snapshot: source.to_path_buf(),
        git_metadata_root: source.join(".git"),
        query_path: query.to_path_buf(),
        request_path: request.to_path_buf(),
        output_path: raw.to_path_buf(),
        output_root: output.to_path_buf(),
        cache_root: cache.to_path_buf(),
        temporary_root: temp.to_path_buf(),
        readonly_roots: vec![source.join(".git")],
        forbidden_paths: vec![oracle.to_path_buf()],
        max_duration: Duration::from_secs(10),
    })
}

#[allow(clippy::too_many_arguments)]
fn assert_denied(
    executable: &Path,
    arguments: Vec<OsString>,
    source: &Path,
    request: &Path,
    output: &Path,
    cache: &Path,
    temp: &Path,
    oracle: &Path,
) {
    let outcome = run_local_sandbox(LocalSandboxRequest {
        executable: executable.to_path_buf(),
        expected_executable_digest: digest(executable),
        allowed_executables: Vec::new(),
        arguments,
        source_snapshot: source.to_path_buf(),
        readonly_roots: Vec::new(),
        request_files: vec![request.to_path_buf()],
        writable_roots: vec![
            output.to_path_buf(),
            cache.to_path_buf(),
            temp.to_path_buf(),
        ],
        forbidden_paths: vec![oracle.to_path_buf()],
        max_duration: Duration::from_secs(2),
        capture_stderr: true,
    })
    .unwrap();
    assert!(matches!(outcome, SandboxOutcome::Exited { status, .. } if !status.success()));
}

fn digest(path: &Path) -> String {
    digest_bytes(&fs::read(path).unwrap())
}

fn git_path() -> PathBuf {
    kit::workspace::acquire::trusted_git_executable().unwrap()
}

fn git_version() -> String {
    String::from_utf8(
        Command::new(git_path())
            .arg("--version")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .into()
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn digest_json(value: &impl serde::Serialize) -> String {
    digest_bytes(&serde_json::to_vec(value).unwrap())
}
