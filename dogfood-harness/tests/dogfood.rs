use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};

const CI_JOB_TIMEOUT: Duration = Duration::from_secs(120 * 60);
const RUN_WALL_TIME: Duration = Duration::from_secs(10);
const MODEL_CALL_BOUND: Duration = Duration::from_secs(5 * 60);
const MODEL_CALLS: u32 = 7;
const READ_TOOL_BOUND: Duration = Duration::from_secs(30);
const INDEX_BUILD_BOUND: Duration = Duration::from_secs(60);
const MEASURED_EDIT_VALIDATION_WORST_CASE: Duration = Duration::from_secs(5);
const EDIT_VALIDATION_MARGIN: Duration = Duration::from_secs(15);
const EDIT_VALIDATION_TIME: Duration = Duration::from_secs(
    MEASURED_EDIT_VALIDATION_WORST_CASE.as_secs() + EDIT_VALIDATION_MARGIN.as_secs(),
);
const SCENARIO_OVERHEAD: Duration = Duration::from_secs(10 * 60);

fn checked_duration_sum(parts: impl IntoIterator<Item = Duration>) -> Duration {
    parts
        .into_iter()
        .try_fold(Duration::ZERO, Duration::checked_add)
        .unwrap()
}

fn scenario_timeout() -> Duration {
    let timeout = checked_duration_sum([
        RUN_WALL_TIME, // explicit kit_run
        MODEL_CALL_BOUND.checked_mul(MODEL_CALLS).unwrap(),
        READ_TOOL_BOUND.checked_mul(3).unwrap(), // discover, search, and read
        INDEX_BUILD_BOUND,
        EDIT_VALIDATION_TIME,
        SCENARIO_OVERHEAD,
    ]);
    assert!(
        timeout < CI_JOB_TIMEOUT,
        "scenario bound must fit the CI job"
    );
    timeout
}

struct Daemon(Child);

struct FixtureCleanup(PathBuf);

impl Drop for FixtureCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn kit_binary() -> PathBuf {
    std::env::var_os("KIT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target/debug/kit")
        })
}

fn kit_checkout() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli(state: &Path, arguments: &[&str]) -> Output {
    Command::new(kit_binary())
        .arg("--state-root")
        .arg(state)
        .arg("--format")
        .arg("json")
        .arg("--timeout-ms")
        .arg("180000")
        .args(arguments)
        .output()
        .unwrap()
}

fn cli_input(state: &Path, arguments: &[&str], input: &[u8]) -> Output {
    use std::io::Write as _;
    let mut child = Command::new(kit_binary())
        .arg("--state-root")
        .arg(state)
        .arg("--format")
        .arg("json")
        .arg("--timeout-ms")
        .arg("180000")
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn json_output(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid CLI JSON: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn wait_json(state: &Path, arguments: &[&str], predicate: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let output = cli(state, arguments);
        let last = format!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if output.status.success() {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            if predicate(&value) {
                return value;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}; last response: {last}",
            arguments.join(" "),
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_repository_result(state: &Path, id: &str) -> Value {
    wait_json(state, &["repo", "result", "--result", id], |result| {
        matches!(
            result["status"].as_str(),
            Some("completed" | "failed" | "cancelled" | "outcome_unknown" | "denied")
        )
    })
}

fn copy_checkout(source: &Path, destination: &Path) {
    let source_root = fs::canonicalize(source).unwrap();
    copy_checkout_from(&source_root, &source_root, destination);
}

fn copy_checkout_from(source_root: &Path, source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if matches!(name.to_str(), Some(".git" | "target" | ".evidence-tmp")) {
            continue;
        }
        let from = entry.path();
        let to = destination.join(name);
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            copy_checkout_from(source_root, &from, &to);
        } else if kind.is_file() {
            fs::copy(from, to).unwrap();
        } else if kind.is_symlink() {
            let target = fs::read_link(&from).unwrap();
            assert!(
                !target.is_absolute(),
                "absolute checkout symlink: {}",
                from.display()
            );
            let resolved = fs::canonicalize(from.parent().unwrap().join(&target)).unwrap();
            assert!(
                resolved.starts_with(source_root),
                "checkout symlink escapes source: {} -> {}",
                from.display(),
                target.display()
            );
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, to).unwrap();
            #[cfg(windows)]
            if resolved.is_dir() {
                std::os::windows::fs::symlink_dir(target, to).unwrap();
            } else {
                std::os::windows::fs::symlink_file(target, to).unwrap();
            }
        }
    }
}

fn canonical_tree(root: &Path) -> Vec<u8> {
    fn append(root: &Path, directory: &Path, output: &mut Vec<u8>) {
        let mut entries = fs::read_dir(directory)
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| {
                !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "target" | ".evidence-tmp")
                )
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap();
            let metadata = fs::symlink_metadata(&path).unwrap();
            output.extend_from_slice(relative.as_os_str().as_encoded_bytes());
            output.push(0);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                output.extend_from_slice(&(metadata.permissions().mode() & 0o777).to_le_bytes());
            }
            if metadata.file_type().is_dir() {
                output.push(b'd');
                append(root, &path, output);
            } else if metadata.file_type().is_file() {
                output.push(b'f');
                let bytes = fs::read(path).unwrap();
                output.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                output.extend_from_slice(&bytes);
            } else if metadata.file_type().is_symlink() {
                output.push(b'l');
                let target = fs::read_link(path).unwrap();
                let bytes = target.as_os_str().as_encoded_bytes();
                output.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                output.extend_from_slice(bytes);
            } else {
                panic!("unsupported checkout entry: {}", path.display());
            }
        }
    }

    let mut output = b"kit-canonical-tree-v1\0".to_vec();
    append(root, root, &mut output);
    output
}

fn tree_digest(root: &Path) -> String {
    format!("blake3:{}", blake3::hash(&canonical_tree(root)).to_hex())
}

fn tree_measurement(root: &Path) -> Value {
    fn measure(root: &Path, entries: &mut u64, bytes: &mut u64) {
        for entry in fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if matches!(
                entry.file_name().to_str(),
                Some(".git" | "target" | ".evidence-tmp")
            ) {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            *entries = entries.checked_add(1).unwrap();
            if metadata.file_type().is_dir() {
                measure(&entry.path(), entries, bytes);
            } else if metadata.file_type().is_file() {
                *bytes = bytes.checked_add(metadata.len()).unwrap();
            } else if metadata.file_type().is_symlink() {
                *bytes = bytes
                    .checked_add(fs::read_link(entry.path()).unwrap().as_os_str().len() as u64)
                    .unwrap();
            }
        }
    }

    let started = Instant::now();
    let mut entries = 0_u64;
    let mut bytes = 0_u64;
    measure(root, &mut entries, &mut bytes);
    json!({
        "entries": entries,
        "bytes": bytes,
        "scan_elapsed_millis": u64::try_from(started.elapsed().as_millis()).unwrap(),
    })
}

fn materialize_unsupported_git_symlink(root: &Path, relative: &Path) {
    let path = root.join(relative);
    let target = fs::read_link(&path).unwrap();
    assert!(!target.is_absolute());
    assert!(
        fs::canonicalize(path.parent().unwrap().join(&target))
            .unwrap()
            .starts_with(fs::canonicalize(root).unwrap())
    );
    fs::remove_file(&path).unwrap();
    fs::write(path, target.as_os_str().as_encoded_bytes()).unwrap();
}

#[derive(Clone)]
struct ProductionPins {
    run_image: String,
    helper_digest: String,
}

fn pinned_digest(name: &str) -> String {
    let value = std::env::var(name).unwrap_or_else(|_| panic!("trusted dogfood requires {name}"));
    assert!(
        value.len() == 71
            && value.starts_with("sha256:")
            && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
            && value[8..].bytes().any(|byte| byte != value.as_bytes()[7]),
        "{name} must be a pinned sha256 digest"
    );
    value
}

fn production_pins() -> ProductionPins {
    let run_image = std::env::var("KIT_NATIVE_CONTAINER_IMAGE")
        .expect("trusted dogfood requires KIT_NATIVE_CONTAINER_IMAGE");
    for (name, image) in [("KIT_NATIVE_CONTAINER_IMAGE", &run_image)] {
        assert!(
            !image.contains('<') && !image.contains('>') && !image.contains("example.invalid"),
            "{name} must not be a placeholder"
        );
        let digest = image
            .rsplit_once('@')
            .map(|(_, digest)| digest)
            .unwrap_or_else(|| panic!("{name} must be an immutable image@sha256 digest"));
        assert_eq!(digest, pinned_digest_value(name, digest));
    }
    ProductionPins {
        run_image,
        helper_digest: pinned_digest("KIT_CONTAINER_HELPER_SHA256"),
    }
}

fn pinned_digest_value(name: &str, value: &str) -> String {
    assert!(
        value.len() == 71
            && value.starts_with("sha256:")
            && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
            && value[8..].bytes().any(|byte| byte != value.as_bytes()[7]),
        "{name} must contain a pinned sha256 digest"
    );
    value.to_owned()
}

fn fixture(pins: Option<&ProductionPins>) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "kit-dogfood-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&root);
    let project = root.join("kit-checkout");
    copy_checkout(kit_checkout(), &project);
    materialize_unsupported_git_symlink(
        &project,
        Path::new("vendor/agentkit/book/src/assets/logo.png"),
    );
    fs::create_dir_all(project.join(".kit")).unwrap();
    let _ = pins;
    fs::write(
        project.join(".kit/native.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "edit_validation_wall_time_millis": EDIT_VALIDATION_TIME.as_millis(),
        }))
        .unwrap(),
    )
    .unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&project)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Kit",
        "-c",
        "user.email=kit@example.invalid",
        "commit",
        "-qm",
        "fixture",
    ]);
    git(&["gc", "--prune=now", "--quiet"]);
    let base = Command::new("git")
        .args(["cat-file", "-e", "HEAD^{commit}"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        base.status.success(),
        "{}",
        String::from_utf8_lossy(&base.stderr)
    );
    (root.join("state"), fs::canonicalize(project).unwrap())
}

fn start_daemon(
    state: &Path,
    project: &Path,
    provider_url: &str,
    fake_syntax: bool,
    pins: Option<&ProductionPins>,
) -> Daemon {
    let mut command = Command::new(kit_binary());
    command
        .arg("daemon")
        .arg("--state-root")
        .arg(state)
        .env("KIT_PROVIDER", "openai")
        .env("OPENAI_API_KEY", "dogfood-http-mock")
        .env("OPENAI_MODEL", "dogfood-model")
        .env("OPENAI_BASE_URL", provider_url)
        .env("KIT_PROJECT_ROOT", project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(
            if std::env::var("KIT_WORKSPACE_SCAN_PROFILE").as_deref() == Ok("1") {
                Stdio::inherit()
            } else {
                Stdio::piped()
            },
        );
    if fake_syntax {
        command.env("KIT_FAKE_SYNTAX", "pass");
    } else {
        command.env_remove("KIT_FAKE_SYNTAX");
    }
    if let Some(pins) = pins {
        command.env("KIT_NATIVE_CONTAINER_IMAGE", &pins.run_image);
    }
    Daemon(command.spawn().unwrap())
}

fn start_real_provider_daemon(state: &Path, project: &Path) -> Daemon {
    assert!(std::env::var_os("KIT_PROVIDER").is_some());
    Daemon(
        Command::new(kit_binary())
            .arg("daemon")
            .arg("--state-root")
            .arg(state)
            .env("KIT_PROJECT_ROOT", project)
            .env_remove("KIT_FAKE_SYNTAX")
            .env_remove("KIT_FAKE_NATIVE_AUTO_APPROVE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(
                if std::env::var("KIT_WORKSPACE_SCAN_PROFILE").as_deref() == Ok("1") {
                    Stdio::inherit()
                } else {
                    Stdio::piped()
                },
            )
            .spawn()
            .unwrap(),
    )
}

fn wait_for_repository(state: &Path) -> Value {
    wait_json(state, &["repo", "status"], |status| {
        status["available"] == true
    })
}

fn revision_in(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => text.as_bytes().windows(66).find_map(|candidate| {
            let candidate = std::str::from_utf8(candidate).ok()?;
            (candidate.starts_with("r:")
                && candidate[2..].bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| candidate.to_owned())
        }),
        Value::Array(values) => values.iter().find_map(revision_in),
        Value::Object(values) => values.values().find_map(revision_in),
        _ => None,
    }
}

fn read_result_bytes(value: &Value) -> Option<Vec<u8>> {
    if let Some(content) = value.pointer("/data/content").and_then(Value::as_array) {
        return content
            .iter()
            .map(|byte| u8::try_from(byte.as_u64()?).ok())
            .collect();
    }
    match value {
        Value::String(text) => serde_json::from_str(text)
            .ok()
            .and_then(|value| read_result_bytes(&value)),
        Value::Array(values) => values.iter().find_map(read_result_bytes),
        Value::Object(values) => values.values().find_map(read_result_bytes),
        _ => None,
    }
}

fn provider_stream(name: Option<&str>, input: Value, step: usize) -> String {
    let value = match name {
        Some(name) => json!({
            "id":format!("chatcmpl-dogfood-{step}"),
            "model":"dogfood-model",
            "choices":[{"index":0,"delta":{"tool_calls":[{
                "index":0,"id":format!("dogfood-call-{step}"),"type":"function",
                "function":{"name":name,"arguments":input.to_string()}
            }]},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}
        }),
        None => json!({
            "id":"chatcmpl-dogfood-complete","model":"dogfood-model",
            "choices":[{"index":0,"delta":{"content":"dogfood complete"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
        }),
    };
    format!("data: {value}\n\ndata: [DONE]\n\n")
}

fn start_provider_mock(project: &Path) -> (String, thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let project = project.to_owned();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for step in 0..7 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 8192];
            let header_end = loop {
                let read = stream.read(&mut buffer).unwrap();
                assert_ne!(read, 0);
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while bytes.len() - header_end < length {
                let read = stream.read(&mut buffer).unwrap();
                assert_ne!(read, 0);
                bytes.extend_from_slice(&buffer[..read]);
            }
            let request: Value =
                serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
            let tools = request["tools"].as_array().unwrap();
            assert_eq!(tools.len(), 5);
            let revision =
                revision_in(&request).expect("public prompt must expose current revision");
            let (name, input) = match step {
                0 => (
                    Some("kit_read"),
                    json!({"malformed_provider_argument":true}),
                ),
                1 => (
                    Some("kit_discover"),
                    json!({"expected_revision":revision,"terms":["NativeCatalog"],"roots":["src"],"languages":["rust"]}),
                ),
                2 => (
                    Some("kit_search"),
                    json!({"expected_revision":revision,"text":"pub mod","mode":"content","path_prefixes":["src"],"languages":["rust"]}),
                ),
                3 => (
                    Some("kit_read"),
                    json!({"expected_revision":revision,"path":"src/lib.rs","range":{"kind":"full"}}),
                ),
                4 => {
                    let original = read_result_bytes(&request).unwrap_or_else(|| {
                        panic!("edit must use returned read content: {request}")
                    });
                    assert_eq!(original, fs::read(project.join("src/lib.rs")).unwrap());
                    let text = String::from_utf8(original.clone()).unwrap();
                    // DR-0008 hunk edit built from the read result: the whole
                    // current file is the (trivially unique) anchor and the
                    // marker constant rides at the end of the new lines.
                    let old: Vec<&str> =
                        text.strip_suffix('\n').unwrap_or(&text).split('\n').collect();
                    let mut new = old.clone();
                    new.push("");
                    new.push("pub const DOGFOOD_NATIVE_PATH: &str = \"provider-kernel-native\";");
                    (
                        Some("kit_edit"),
                        json!({"version":2,"operations":[{
                            "op":"edit","path":"src/lib.rs",
                            "hunks":[{"context_before":[],"old":old,"new":new,"context_after":[]}]
                        }]}),
                    )
                }
                5 => (
                    Some("kit_run"),
                    json!({
                        "argv":["cargo","metadata","--no-deps","--format-version","1"],
                        "working_directory":".","mounts":{"source":"read_only","build":"read_write","temp":"read_write"},
                        "environment":{},"network":"deny","host_compatibility":false,"background":"foreground",
                        "limits":{"cpu_millis":1000,"memory_bytes":268435456,"pids":64,"file_bytes":16777216,"disk_bytes":268435456,"io_bytes":67108864,"output_bytes":65536,"wall_time_millis":10000}
                    }),
                ),
                _ => (None, Value::Null),
            };
            let response = provider_stream(name, input, step);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
            requests.push(request);
        }
        requests
    });
    (endpoint, handle)
}

fn wait_run_with_approvals(state: &Path, project_id: &str, run_id: &str) -> Value {
    let deadline = Instant::now().checked_add(scenario_timeout()).unwrap();
    let mut resolved = BTreeSet::new();
    loop {
        let run = json_output(cli(state, &["run", "show", run_id]));
        if matches!(
            run["state"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {
            return run;
        }
        for arguments in [
            &["repo", "status"][..],
            &["run", "show", run_id][..],
            &["run", "transcript", run_id][..],
        ] {
            let started = Instant::now();
            let response = cli(state, arguments);
            assert!(
                response.status.success(),
                "responsive read failed: {arguments:?}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "async status/read blocked behind validation: {arguments:?}"
            );
        }
        if run["state"] == "waiting_for_approval" {
            let approvals = json_output(cli(state, &["approval", "list", "--project", project_id]));
            for approval in approvals["items"].as_array().unwrap() {
                let id = approval["id"].as_str().unwrap();
                if resolved.insert(id.to_owned()) {
                    let version = approval["version"].as_u64().unwrap().to_string();
                    let key = format!("dogfood-approve-{id}");
                    json_output(cli(
                        state,
                        &[
                            "approval",
                            "resolve",
                            "--approval",
                            id,
                            "--decision",
                            "approved",
                            "--version",
                            &version,
                            "--idempotency-key",
                            &key,
                        ],
                    ));
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for run {run_id}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn artifact_references(value: &Value, found: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if let Some(text) = value.as_str()
                    && text.starts_with("blake3:")
                    && text.len() == 71
                    && (key == "reference" || key.ends_with("artifact"))
                {
                    found.insert(text.to_owned());
                }
                artifact_references(value, found);
            }
        }
        Value::Array(values) => {
            for value in values {
                artifact_references(value, found);
            }
        }
        Value::String(text) if text.starts_with("artifact-ref:") && text.len() == 77 => {
            found.insert(text.to_owned());
        }
        _ => {}
    }
}

#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct SourceProvenance {
    operation_id: Option<String>,
    call_id: Option<String>,
    run_id: Option<String>,
}

fn object_string(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
}

fn collect_source_provenance(
    value: &Value,
    inherited: &SourceProvenance,
    found: &mut BTreeSet<(String, SourceProvenance)>,
) {
    match value {
        Value::Object(object) => {
            let metadata = object.get("metadata").and_then(Value::as_object);
            let descriptor = object.get("provenance").and_then(Value::as_object);
            let mut context = inherited.clone();
            context.operation_id = object_string(
                object,
                &[
                    "operation_id",
                    "native_operation_id",
                    "feedback_operation_id",
                ],
            )
            .or_else(|| {
                metadata.and_then(|value| {
                    object_string(value, &["kit.native_operation_id", "operation_id"])
                })
            })
            .or_else(|| descriptor.and_then(|value| object_string(value, &["operation_id"])))
            .or(context.operation_id);
            context.call_id = object_string(object, &["call_id", "tool_call_id"])
                .or_else(|| {
                    object
                        .get("ToolCall")
                        .and_then(Value::as_object)
                        .and_then(|value| object_string(value, &["id"]))
                })
                .or(context.call_id);
            context.run_id = object_string(object, &["run_id"]).or(context.run_id);

            for (key, child) in object {
                if let Some(reference) = child.as_str()
                    && ((reference.starts_with("artifact-ref:") && reference.len() == 77)
                        || (reference.starts_with("blake3:")
                            && reference.len() == 71
                            && (key == "reference" || key.ends_with("artifact"))))
                {
                    found.insert((reference.to_owned(), context.clone()));
                }
                collect_source_provenance(child, &context, found);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_source_provenance(value, inherited, found);
            }
        }
        _ => {}
    }
}

fn artifact_source_provenance(
    state: &Path,
    references: &BTreeSet<String>,
    roots: &[Value],
    run_id: &str,
) -> BTreeMap<String, BTreeSet<SourceProvenance>> {
    let root = SourceProvenance {
        run_id: Some(run_id.to_owned()),
        ..SourceProvenance::default()
    };
    let mut pending = BTreeSet::new();
    for value in roots {
        collect_source_provenance(value, &root, &mut pending);
    }
    let mut provenance = BTreeMap::<String, BTreeSet<SourceProvenance>>::new();
    let mut opened = BTreeSet::new();
    while let Some((reference, context)) = pending.pop_first() {
        provenance
            .entry(reference.clone())
            .or_default()
            .insert(context.clone());
        if opened.insert((reference.clone(), context.clone()))
            && let Ok(value) = serde_json::from_slice::<Value>(&artifact_bytes(state, &reference))
        {
            collect_source_provenance(&value, &context, &mut pending);
        }
    }
    for reference in references {
        provenance
            .entry(reference.clone())
            .or_default()
            .insert(root.clone());
    }
    provenance
}

fn artifact_bytes(state: &Path, reference: &str) -> Vec<u8> {
    let artifact = json_output(cli(
        state,
        &["repo", "artifact", "--artifact-ref", reference],
    ));
    for field in [
        "digest",
        "media_type",
        "class",
        "principal_id",
        "project_id",
    ] {
        assert!(
            artifact[field]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
    }
    let bytes = artifact["bytes"]
        .as_array()
        .unwrap_or_else(|| panic!("artifact has no bytes: {artifact}"))
        .iter()
        .map(|byte| u8::try_from(byte.as_u64().unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        artifact["digest"],
        format!("blake3:{}", blake3::hash(&bytes).to_hex())
    );
    bytes
}

#[test]
fn scenario_timeout_is_checked_and_fits_ci() {
    assert_eq!(scenario_timeout(), Duration::from_secs(2_880));
    assert!(scenario_timeout() < CI_JOB_TIMEOUT);
}

#[test]
fn current_kit_tree_measurement_is_recorded() {
    let measurement = tree_measurement(kit_checkout());
    assert!(
        measurement["entries"]
            .as_u64()
            .is_some_and(|value| value > 0)
    );
    assert!(measurement["bytes"].as_u64().is_some_and(|value| value > 0));
    println!("KIT_TREE_MEASUREMENT={measurement}");
}

#[test]
fn artifact_provenance_keeps_source_operation_call_and_run() {
    let reference = format!("artifact-ref:{}", "a".repeat(64));
    let source = json!({
        "run_id": "run-source",
        "content": {
            "call_id": "call-source",
            "metadata": {"kit.native_operation_id": "operation-source"},
            "output": {"diff_artifact": {"reference": reference}},
        },
    });
    let mut found = BTreeSet::new();
    collect_source_provenance(&source, &SourceProvenance::default(), &mut found);
    assert_eq!(
        found,
        BTreeSet::from([(
            reference,
            SourceProvenance {
                operation_id: Some("operation-source".to_owned()),
                call_id: Some("call-source".to_owned()),
                run_id: Some("run-source".to_owned()),
            },
        )])
    );
}

fn command_text(program: &str, arguments: &[&str], directory: &Path) -> String {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(directory)
        .output()
        .unwrap_or_else(|error| panic!("cannot run {program}: {error}"));
    assert!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[cfg(target_os = "linux")]
fn trusted_linux_platform_preflight() -> Value {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").expect("EXT-01 mountinfo");
    let mount_line = mountinfo
        .lines()
        .find(|line| line.contains(" - cgroup2 "))
        .expect("EXT-01 requires a cgroup v2 mount");
    let mount_point = PathBuf::from(
        mount_line
            .split_whitespace()
            .nth(4)
            .expect("cgroup v2 mountpoint"),
    );
    let process_cgroup = fs::read_to_string("/proc/self/cgroup").expect("EXT-01 process cgroup");
    let relative = process_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .expect("EXT-01 requires unified process cgroup");
    let delegated = mount_point.join(relative.trim_start_matches('/'));
    let controllers = fs::read_to_string(delegated.join("cgroup.controllers"))
        .expect("EXT-01 delegated cgroup controllers");
    assert!(
        !controllers
            .split_whitespace()
            .collect::<Vec<_>>()
            .is_empty(),
        "EXT-01 delegated cgroup has no controllers"
    );
    let subtree_control =
        fs::read_to_string(delegated.join("cgroup.subtree_control")).unwrap_or_default();
    fs::OpenOptions::new()
        .write(true)
        .open(delegated.join("cgroup.procs"))
        .expect("EXT-01 delegated cgroup.procs must be writable");
    let probe = delegated.join(format!("kit-ext01-probe-{}", std::process::id()));
    fs::create_dir(&probe).expect("EXT-01 requires writable delegated cgroup");
    let kill = probe.join("cgroup.kill");
    let kill_present = kill.is_file();
    let child_controllers =
        fs::read_to_string(probe.join("cgroup.controllers")).unwrap_or_default();
    fs::remove_dir(&probe).expect("remove EXT-01 cgroup probe");
    assert!(kill_present, "EXT-01 delegated cgroup lacks cgroup.kill");

    let landlock = Command::new(std::env::current_exe().unwrap())
        .args([
            "landlock_policy_probe_child",
            "--ignored",
            "--exact",
            "--nocapture",
        ])
        .env("KIT_LANDLOCK_POLICY_PROBE_CHILD", "1")
        .output()
        .expect("run Landlock policy probe");
    assert!(
        landlock.status.success(),
        "EXT-01 Landlock policy probe failed: {}",
        String::from_utf8_lossy(&landlock.stderr)
    );
    let output = String::from_utf8(landlock.stdout).unwrap();
    let landlock_probe = output
        .lines()
        .find_map(|line| {
            line.find("KIT_LANDLOCK_PROBE=")
                .map(|at| &line[at + "KIT_LANDLOCK_PROBE=".len()..])
        })
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .expect("exact Landlock probe output");

    json!({
        "kernel": command_text("uname", &["-a"], kit_checkout()),
        "ext_contract": "EXT-01",
        "cgroup_v2": {
            "mountinfo": mount_line,
            "mount_point": mount_point,
            "controllers": controllers.split_whitespace().collect::<Vec<_>>(),
            "process_cgroup": process_cgroup.trim(),
            "delegated_path": delegated,
            "subtree_control": subtree_control.split_whitespace().collect::<Vec<_>>(),
            "probe_path": probe,
            "probe_controllers": child_controllers.split_whitespace().collect::<Vec<_>>(),
            "cgroup_procs_writable": true,
            "cgroup_kill": kill_present,
        },
        "landlock": landlock_probe,
        "process_status": fs::read_to_string("/proc/self/status").expect("process status"),
    })
}

#[cfg(not(target_os = "linux"))]
fn trusted_linux_platform_preflight() -> Value {
    panic!("EXT-01 trusted preflight requires Linux")
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "internal child for the trusted Linux Landlock policy probe"]
fn landlock_policy_probe_child() {
    assert_eq!(
        std::env::var("KIT_LANDLOCK_POLICY_PROBE_CHILD").as_deref(),
        Ok("1")
    );
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }
    unsafe extern "C" {
        fn syscall(number: isize, ...) -> isize;
        fn prctl(option: i32, ...) -> i32;
        fn close(fd: i32) -> i32;
    }
    const LANDLOCK_CREATE_RULESET: isize = 444;
    const LANDLOCK_RESTRICT_SELF: isize = 446;
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1;
    const PR_SET_NO_NEW_PRIVS: i32 = 38;

    // The child is disposable because applying a Landlock ruleset is irreversible.
    let abi = unsafe {
        syscall(
            LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0_usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    assert!(
        abi > 0,
        "Landlock ABI query failed: {}",
        std::io::Error::last_os_error()
    );
    let attr = RulesetAttr {
        handled_access_fs: LANDLOCK_ACCESS_FS_EXECUTE,
    };
    let ruleset = unsafe {
        syscall(
            LANDLOCK_CREATE_RULESET,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0_u32,
        )
    };
    assert!(
        ruleset >= 0,
        "Landlock ruleset creation failed: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(unsafe { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }, 0);
    assert_eq!(
        unsafe { syscall(LANDLOCK_RESTRICT_SELF, ruleset, 0_u32) },
        0
    );
    assert_eq!(unsafe { close(ruleset as i32) }, 0);
    println!(
        "KIT_LANDLOCK_PROBE={}",
        json!({
            "abi": abi,
            "handled_access_fs": LANDLOCK_ACCESS_FS_EXECUTE,
            "no_new_privs": true,
            "policy_restricted": true,
        })
    );
}

struct EvidenceExport<'a> {
    state: &'a Path,
    directory: &'a Path,
    references: &'a BTreeSet<String>,
    source_documents: &'a [Value],
    run_id: &'a str,
    pins: &'a ProductionPins,
    baseline_tree_digest: &'a str,
    materialized_tree_digest: &'a str,
    source_tree_measurement: &'a Value,
    platform_preflight: &'a Value,
}

fn export_evidence_bundle(export: EvidenceExport<'_>) {
    let EvidenceExport {
        state,
        directory,
        references,
        source_documents,
        run_id,
        pins,
        baseline_tree_digest,
        materialized_tree_digest,
        source_tree_measurement,
        platform_preflight,
    } = export;
    fs::create_dir_all(directory.join("artifacts")).unwrap();
    let source_provenance = artifact_source_provenance(state, references, source_documents, run_id);
    let mut artifacts = Vec::new();
    for reference in references {
        let response = json_output(cli(
            state,
            &["repo", "artifact", "--artifact-ref", reference],
        ));
        let source_manifest = json!({
            "digest": response["digest"],
            "media_type": response["media_type"],
            "class": response["class"],
            "principal_id": response["principal_id"],
            "project_id": response["project_id"],
        });
        for field in [
            "digest",
            "media_type",
            "class",
            "principal_id",
            "project_id",
        ] {
            assert!(
                source_manifest[field]
                    .as_str()
                    .is_some_and(|value| !value.is_empty()),
                "artifact source manifest is missing {field}: {response}"
            );
        }
        let bytes = response["bytes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|byte| u8::try_from(byte.as_u64().unwrap()).unwrap())
            .collect::<Vec<_>>();
        let digest = format!("blake3:{}", blake3::hash(&bytes).to_hex());
        assert_eq!(source_manifest["digest"], digest);
        if reference.starts_with("blake3:") {
            assert_eq!(
                reference, &digest,
                "artifact digest must bind exported bytes"
            );
        }
        let name = format!("{}.bin", digest.strip_prefix("blake3:").unwrap());
        let path = directory.join("artifacts").join(&name);
        fs::write(&path, &bytes).unwrap();
        assert_eq!(fs::read(path).unwrap(), bytes);
        artifacts.push(json!({
            "reference": reference,
            "size": bytes.len(),
            "path": format!("artifacts/{name}"),
            "source_manifest": source_manifest,
            "source_provenance": source_provenance[reference].iter().map(|source| json!({
                "operation_id": source.operation_id,
                "call_id": source.call_id,
                "run_id": source.run_id,
            })).collect::<Vec<_>>(),
        }));
    }

    let helper_path = Path::new("/usr/libexec/kit-container-helper");
    let helper_digest = format!(
        "sha256:{}",
        command_text(
            "sha256sum",
            &[helper_path.to_str().unwrap()],
            kit_checkout()
        )
        .split_whitespace()
        .next()
        .unwrap()
    );
    assert_eq!(helper_digest, pins.helper_digest);
    let cargo_lock = fs::read(kit_checkout().join("Cargo.lock")).unwrap();
    let manifest = json!({
        "schema_version": 1,
        "scenario_id": "trusted_linux_production_dogfood_required_for_g04",
        "source": {
            "commit": command_text("git", &["rev-parse", "HEAD"], kit_checkout()),
            "tree_digest": tree_digest(kit_checkout()),
            "baseline_tree_digest": baseline_tree_digest,
            "materialized_tree_digest": materialized_tree_digest,
            "cargo_lock_digest": format!("blake3:{}", blake3::hash(&cargo_lock).to_hex()),
            "measured_tree": source_tree_measurement,
        },
        "toolchain": {
            "rustc": command_text("rustc", &["--version", "--verbose"], kit_checkout()),
            "cargo": command_text("cargo", &["--version", "--verbose"], kit_checkout()),
        },
        "platform": platform_preflight,
        "runner": {
            "name": std::env::var("RUNNER_NAME").ok(),
            "environment": std::env::var("RUNNER_ENVIRONMENT").ok(),
            "workflow": std::env::var("GITHUB_WORKFLOW_REF").ok(),
            "run_id": std::env::var("GITHUB_RUN_ID").ok(),
            "run_attempt": std::env::var("GITHUB_RUN_ATTEMPT").ok(),
        },
        "helper": {
            "path": helper_path,
            "digest": helper_digest,
        },
        "commands": [
            "cargo build --locked --bin kit",
            "cargo test --locked --manifest-path dogfood-harness/Cargo.toml trusted_linux_production_dogfood_required_for_g04 -- --ignored --exact",
        ],
        "pins": {
            "run_image": pins.run_image,
            "helper_digest": pins.helper_digest,
        },
        "artifacts": artifacts,
    });
    fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn assert_fields(value: &Value, expected: &[&str]) {
    let actual = value
        .as_object()
        .unwrap_or_else(|| panic!("expected object: {value}"))
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected.iter().copied().collect(), "{value}");
}

fn tool_call<'a>(transcript: &'a Value, name: &str) -> &'a Value {
    let calls = transcript["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "model_tool_call")
        .map(|item| &item["content"]["ToolCall"])
        .filter(|call| call["name"] == name && call["id"] != "dogfood-call-0")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1, "expected one {name} call: {transcript}");
    calls[0]
}

fn tool_result<'a>(transcript: &'a Value, call_id: &str) -> &'a Value {
    transcript["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "tool_result")
        .map(|item| &item["content"])
        .find(|result| result["call_id"] == call_id)
        .unwrap_or_else(|| panic!("missing terminal result for {call_id}: {transcript}"))
}

fn structured_output(result: &Value) -> &Value {
    result["output"]["Structured"]
        .as_object()
        .map(|_| &result["output"]["Structured"])
        .unwrap_or_else(|| panic!("expected structured tool output: {result}"))
}

fn output_data(output: &Value) -> &Value {
    output.get("data").unwrap_or(output)
}

fn assert_registered_tools(requests: Vec<Value>) {
    assert_eq!(requests.len(), 7);
    for request in requests {
        let names = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            names,
            BTreeSet::from([
                "kit_discover",
                "kit_search",
                "kit_read",
                "kit_edit",
                "kit_run",
            ])
        );
    }
}

fn open_artifact_closure(state: &Path, root: &Value) -> BTreeSet<String> {
    let mut pending = BTreeSet::new();
    let mut opened = BTreeSet::new();
    artifact_references(root, &mut pending);
    while let Some(reference) = pending.pop_first() {
        if !opened.insert(reference.clone()) {
            continue;
        }
        let bytes = artifact_bytes(state, &reference);
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            artifact_references(&value, &mut pending);
        }
    }
    opened
}

fn assert_provider_edit_evidence(
    state: &Path,
    project: &Path,
    pristine: &Path,
    transcript: &Value,
) -> (String, BTreeSet<String>) {
    let edit_call = tool_call(transcript, "kit_edit");
    assert_eq!(edit_call["id"], "dogfood-call-4");
    let call_id = edit_call["id"].as_str().unwrap();
    let result = tool_result(transcript, call_id);
    assert_eq!(result["is_error"], false, "{result}");
    let operation_id = result["metadata"]["kit.native_operation_id"]
        .as_str()
        .expect("provider edit result must carry its kernel operation id")
        .to_owned();
    assert_eq!(result["metadata"]["kit.native_result_id"], operation_id);
    let output = structured_output(result);
    let data = output_data(output);
    assert_eq!(data["outcome"], "committed", "{data}");
    assert!(
        data["revision"]["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("r:"))
    );
    assert_eq!(
        data["trace"],
        json!([
            "edit.normalize.v1",
            "edit.ir.new.v1",
            "edit.validate.v1",
            "edit.stage.v1",
            "edit.recovery.v1"
        ])
    );

    let diff_reference = data["diff_artifact"]["reference"].as_str().unwrap();
    let actual_diff = artifact_bytes(state, diff_reference);
    let patch = actual_diff
        .splitn(2, |byte| *byte == b'\n')
        .nth(1)
        .and_then(|bytes| {
            bytes
                .windows(2)
                .position(|part| part == b"\n\n")
                .map(|at| &bytes[at + 2..])
        })
        .expect("authenticated actual diff must contain Git patch bytes");
    let mut apply = Command::new("git")
        .args(["apply", "--binary", "-"])
        .current_dir(pristine)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    apply.stdin.take().unwrap().write_all(patch).unwrap();
    let apply = apply.wait_with_output().unwrap();
    assert!(
        apply.status.success(),
        "actual diff cannot reconstruct the pristine baseline: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    assert_eq!(
        canonical_tree(pristine),
        canonical_tree(project),
        "actual diff must reconstruct the complete materialized checkout"
    );

    let references = open_artifact_closure(state, data);
    assert!(
        !references.is_empty(),
        "expected the diff artifact in the closure: {references:?}"
    );
    (operation_id, references)
}

fn provider_scenario(production: Option<&ProductionPins>) {
    let mut source_tree_measurement = tree_measurement(kit_checkout());
    eprintln!("current Kit tree measurement: {source_tree_measurement}");
    let platform_preflight = production.map(|_| trusted_linux_platform_preflight());
    let (state, project) = fixture(production);
    let _cleanup = FixtureCleanup(state.parent().unwrap().to_owned());
    let pristine = state.parent().unwrap().join("pristine-baseline");
    copy_checkout(&project, &pristine);
    let baseline_tree_digest = tree_digest(&pristine);
    assert_ne!(project, kit_checkout());
    assert!(project.join("Cargo.toml").is_file());
    let (provider_url, provider) = start_provider_mock(&project);
    let fake_syntax = production.is_none();
    let daemon = start_daemon(&state, &project, &provider_url, fake_syntax, production);
    let repository_status = wait_for_repository(&state);
    let project_id = repository_status["project_id"].as_str().unwrap();
    json_output(cli(&state, &["project", "create", "--id", project_id]));
    let initial_revision = json_output(cli(&state, &["repo", "revision", "--project", project_id]));
    assert_eq!(
        json_output(cli(&state, &["repo", "status"]))["project_id"],
        project_id
    );
    let thread = json_output(cli(&state, &["thread", "create", "--project", project_id]));
    let thread_id = thread["resource"]["id"].as_str().unwrap();
    let enqueue_started = Instant::now();
    let prompt = json_output(cli(
        &state,
        &[
            "prompt",
            "--thread",
            thread_id,
            "Make the deterministic safe Kit source change.",
        ],
    ));
    assert!(
        enqueue_started.elapsed() < Duration::from_secs(5),
        "async run enqueue exceeded responsiveness bound"
    );
    let run_id = prompt["resource"]["id"].as_str().unwrap().to_owned();

    let run_started = Instant::now();
    let completed = wait_run_with_approvals(&state, project_id, &run_id);
    let run_elapsed_millis = u64::try_from(run_started.elapsed().as_millis()).unwrap();
    source_tree_measurement["provider_run_elapsed_millis"] = run_elapsed_millis.into();
    eprintln!("provider run elapsed: {run_elapsed_millis} ms");
    assert_eq!(completed["state"], "completed", "{completed}");
    assert_registered_tools(provider.join().unwrap());
    let transcript = json_output(cli(&state, &["run", "transcript", &run_id]));
    let malformed = tool_result(&transcript, "dogfood-call-0");
    assert_eq!(malformed["is_error"], true);
    assert_eq!(malformed["metadata"]["agentkit.tool.not_started"], true);
    assert!(
        malformed["metadata"]
            .get("kit.native_operation_id")
            .is_none()
    );
    assert!(malformed["metadata"].get("kit.native_result_id").is_none());
    assert!(malformed["metadata"].get("kit.intent_event_id").is_none());
    let expected_transport_errors = [
        ("kit_discover", false),
        ("kit_search", false),
        ("kit_read", false),
        ("kit_edit", false),
        ("kit_run", production.is_none()),
    ];
    for (name, local_error) in expected_transport_errors {
        let call = tool_call(&transcript, name);
        let result = tool_result(&transcript, call["id"].as_str().unwrap());
        assert_eq!(
            result["is_error"], local_error,
            "terminal outcome for {name}: {result}"
        );
    }
    if let Some(pins) = production {
        let run = output_data(structured_output(tool_result(
            &transcript,
            "dogfood-call-5",
        )));
        assert_eq!(run["outcome"]["status"], "success");
        let run_process: Value = serde_json::from_slice(&artifact_bytes(
            &state,
            run["process_artifact"].as_str().unwrap(),
        ))
        .unwrap();
        assert_eq!(run_process["helper_identity"], pins.helper_digest);
        assert_eq!(
            run_process["image_digest"],
            pins.run_image.rsplit_once('@').unwrap().1
        );
        assert_eq!(run_process["survivors"], 0);
        assert_eq!(run_process["quiescent"], true);
    }
    let (provider_edit_id, mut references) =
        assert_provider_edit_evidence(&state, &project, &pristine, &transcript);
    references.extend(open_artifact_closure(&state, &transcript));
    let revision = wait_json(
        &state,
        &["repo", "revision", "--project", project_id],
        |value| value["revision"] != initial_revision["revision"],
    );
    let cost = json_output(cli(&state, &["run", "cost", &run_id]));
    references.extend(open_artifact_closure(&state, &cost));
    assert!(
        cost["usage"]["attempts"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert!(
        revision["revision"]
            .as_str()
            .is_some_and(|value| value.starts_with("r:"))
    );
    drop(daemon);
    let restarted = start_daemon(&state, &project, &provider_url, fake_syntax, production);
    wait_for_repository(&state);
    wait_json(&state, &["run", "show", &run_id], |run| {
        run["state"] == "completed"
    });
    let restarted_transcript = json_output(cli(&state, &["run", "transcript", &run_id]));
    assert_eq!(
        tool_result(&restarted_transcript, "dogfood-call-4")["metadata"]["kit.native_operation_id"],
        provider_edit_id
    );
    for reference in &references {
        let _ = artifact_bytes(&state, reference);
    }
    assert_eq!(json_output(cli(&state, &["run", "cost", &run_id])), cost);
    if let (Some(pins), Ok(path)) = (production, std::env::var("KIT_DOGFOOD_EVIDENCE_BUNDLE")) {
        export_evidence_bundle(EvidenceExport {
            state: &state,
            directory: Path::new(&path),
            references: &references,
            source_documents: &[transcript.clone(), cost.clone()],
            run_id: &run_id,
            pins,
            baseline_tree_digest: &baseline_tree_digest,
            materialized_tree_digest: &tree_digest(&project),
            source_tree_measurement: &source_tree_measurement,
            platform_preflight: platform_preflight.as_ref().unwrap(),
        });
    }
    drop(restarted);
    fs::remove_dir_all(state.parent().unwrap()).unwrap();
}

#[test]
fn local_mechanical_provider_conformance_uses_public_cli_and_http() {
    provider_scenario(None);
}

#[cfg(unix)]
#[test]
fn checkout_copy_preserves_contained_symlinks_and_rejects_escapes() {
    let root = std::env::temp_dir().join(format!("kit-dogfood-symlinks-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let source = root.join("source");
    fs::create_dir_all(source.join("inside")).unwrap();
    fs::write(source.join("inside/file"), b"inside").unwrap();
    fs::write(root.join("outside"), b"outside").unwrap();
    std::os::unix::fs::symlink("inside/file", source.join("safe")).unwrap();
    copy_checkout(&source, &root.join("safe-copy"));
    assert_eq!(
        fs::read_link(root.join("safe-copy/safe")).unwrap(),
        Path::new("inside/file")
    );
    std::os::unix::fs::symlink("../outside", source.join("escape")).unwrap();
    assert!(
        std::panic::catch_unwind(|| copy_checkout(&source, &root.join("rejected-copy"))).is_err()
    );
    assert!(!root.join("rejected-copy/escape").exists());
    fs::remove_dir_all(root).unwrap();
}

fn read_edit_input(state: &Path, project_id: &str, revision: &Value, marker: &str) -> Vec<u8> {
    let read = json_output(cli_input(
        state,
        &["repo", "read", "--project", project_id],
        serde_json::to_string(&json!({
            "expected_revision":revision,"path":"src/lib.rs","range":{"kind":"full"}
        }))
        .unwrap()
        .as_bytes(),
    ));
    let read = wait_repository_result(state, read["id"].as_str().unwrap());
    let source = read["output"]["data"]["content"]
        .as_array()
        .unwrap_or_else(|| panic!("repository read returned no content: {read}"))
        .iter()
        .map(|byte| u8::try_from(byte.as_u64().unwrap()).unwrap())
        .collect::<Vec<_>>();
    let source_text = String::from_utf8(source).unwrap();
    let source_text = source_text.strip_suffix('\n').unwrap_or(&source_text);
    // DR-0008 hunk edit: the whole current file anchors the append.
    let old: Vec<&str> = source_text.split('\n').collect();
    let mut new = old.clone();
    let marker_line = format!("pub const {marker}: &str = \"public-api\";");
    new.push("");
    new.push(&marker_line);
    serde_json::to_vec(&json!({
        "version":2,"operations":[{
            "op":"edit","path":"src/lib.rs",
            "hunks":[{"context_before":[],"old":old,"new":new,"context_after":[]}]
        }]
    }))
    .unwrap()
}

fn submit_edit(state: &Path, project_id: &str, key: &str, input: &[u8]) -> String {
    let pending = json_output(cli_input(
        state,
        &[
            "repo",
            "edit",
            "--project",
            project_id,
            "--idempotency-key",
            key,
        ],
        input,
    ));
    assert_eq!(pending["status"], "waiting_approval");
    pending["id"].as_str().unwrap().to_owned()
}

fn approve_edit(state: &Path, id: &str, key: &str) -> Value {
    json_output(cli(
        state,
        &[
            "repo",
            "approval",
            "--result",
            id,
            "--decision",
            "approved",
            "--idempotency-key",
            key,
        ],
    ));
    wait_repository_result(state, id)
}

#[test]
fn direct_public_edit_failure_approval_and_artifact_contracts() {
    let (state, project) = fixture(None);
    let _cleanup = FixtureCleanup(state.parent().unwrap().to_owned());
    let daemon = start_daemon(&state, &project, "http://127.0.0.1:1", true, None);
    let status = wait_for_repository(&state);
    let project_id = status["project_id"].as_str().unwrap();
    json_output(cli(&state, &["project", "create", "--id", project_id]));
    let initial = json_output(cli(&state, &["repo", "revision", "--project", project_id]));
    let input = read_edit_input(
        &state,
        project_id,
        &initial["revision"],
        "DOGFOOD_PUBLIC_PATH",
    );
    let mut invalid_input: Value = serde_json::from_slice(&input).unwrap();
    // DR-0008: a hunk built from an outdated view of the file cannot anchor.
    invalid_input["operations"][0]["hunks"][0]["old"][0] =
        Value::String("this line no longer exists in src/lib.rs".to_owned());
    let invalid_id = submit_edit(
        &state,
        project_id,
        "dogfood-invalid-edit",
        &serde_json::to_vec(&invalid_input).unwrap(),
    );
    let invalid = approve_edit(&state, &invalid_id, "dogfood-invalid-edit-approval");
    assert_eq!(invalid["status"], "failed", "{invalid}");
    assert_eq!(invalid["error"]["code"], "edit_anchor_not_found");
    assert!(
        invalid["error"]["detail"]
            .as_str()
            .unwrap()
            .contains("outdated"),
        "{invalid}"
    );
    let invalid_events = json_output(cli(&state, &["repo", "events", "--result", &invalid_id]));
    assert!(
        invalid_events["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| {
                event["type"] == "repository.operation_terminal"
                    && event["payload"]["result"]["error"]["code"] == "edit_anchor_not_found"
            })
    );
    let edit_id = submit_edit(&state, project_id, "dogfood-public-edit", &input);
    let edit = approve_edit(&state, &edit_id, "dogfood-public-edit-approval");
    assert_eq!(edit["status"], "completed", "{edit}");
    assert_eq!(edit["native_operation_id"], edit_id);
    assert_eq!(edit["native_result_id"], edit_id);

    let mut references = BTreeSet::new();
    for key in ["actual_diff", "edit_events", "cost"] {
        let descriptor = &edit["artifacts"][key];
        assert_fields(
            descriptor,
            &["reference", "digest", "media_type", "size", "provenance"],
        );
        let provenance_fields = if key == "actual_diff" {
            &[
                "kind",
                "operation_id",
                "native_result_id",
                "principal_id",
                "project_id",
                "revision_id",
                "transaction_id",
            ][..]
        } else {
            &[
                "kind",
                "operation_id",
                "native_result_id",
                "principal_id",
                "project_id",
            ][..]
        };
        assert_fields(&descriptor["provenance"], provenance_fields);
        assert_eq!(descriptor["provenance"]["kind"], key);
        assert_eq!(descriptor["provenance"]["operation_id"], edit_id);
        assert_eq!(descriptor["provenance"]["native_result_id"], edit_id);
        let reference = descriptor["reference"].as_str().unwrap();
        let bytes = artifact_bytes(&state, reference);
        assert_eq!(
            format!("blake3:{}", blake3::hash(&bytes).to_hex()),
            descriptor["digest"]
        );
        assert_eq!(bytes.len() as u64, descriptor["size"].as_u64().unwrap());
        references.insert(reference.to_owned());
    }
    assert_eq!(references.len(), 3);
    let events = json_output(cli(&state, &["repo", "events", "--result", &edit_id]));
    let event_types = events["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<BTreeSet<_>>();
    assert!(event_types.contains("capability.invocation_intent"));
    assert!(event_types.contains("capability.invocation_outcome"));
    assert!(event_types.contains("repository.operation_terminal"));

    let current = json_output(cli(&state, &["repo", "revision", "--project", project_id]));
    let failed_input = read_edit_input(
        &state,
        project_id,
        &current["revision"],
        "DOGFOOD_ABORTED_PATH",
    );
    let before_failed = fs::read(project.join("src/lib.rs")).unwrap();

    let denied_id = submit_edit(&state, project_id, "dogfood-denied-edit", &failed_input);
    json_output(cli(
        &state,
        &[
            "repo",
            "approval",
            "--result",
            &denied_id,
            "--decision",
            "denied",
            "--idempotency-key",
            "dogfood-denied-resolution",
        ],
    ));
    let denied = wait_repository_result(&state, &denied_id);
    assert_eq!(denied["status"], "denied");
    assert_eq!(denied["approval"]["state"], "denied");
    assert_eq!(denied["error"]["code"], "approval_denied");
    assert_eq!(denied["error"]["effect_state"], "none");
    assert_eq!(denied["cost"]["charged"], false);
    assert_eq!(denied["cost"]["released"]["tools"], 1);

    let cancelled_id = submit_edit(&state, project_id, "dogfood-cancelled-edit", &failed_input);
    json_output(cli(
        &state,
        &[
            "repo",
            "cancel",
            "--result",
            &cancelled_id,
            "--idempotency-key",
            "dogfood-cancelled-resolution",
        ],
    ));
    let cancelled = wait_repository_result(&state, &cancelled_id);
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["error"]["effect_state"], "none");
    assert_eq!(cancelled["cost"]["charged"], false);
    assert_eq!(fs::read(project.join("src/lib.rs")).unwrap(), before_failed);

    let revision_before_restart =
        json_output(cli(&state, &["repo", "revision", "--project", project_id]));
    let tree_before_restart = tree_digest(&project);
    let before_restart = [&invalid_id, &edit_id, &denied_id, &cancelled_id]
        .into_iter()
        .map(|id| {
            let result = json_output(cli(&state, &["repo", "result", "--result", id]));
            let events = json_output(cli(&state, &["repo", "events", "--result", id]));
            assert!(events["items"].as_array().unwrap().iter().any(|event| {
                event["type"] == "repository.operation_terminal"
                    && event["payload"]["result"]["status"] == result["status"]
            }));
            (id.to_owned(), result, events)
        })
        .collect::<Vec<_>>();
    let mut all_references = references;
    for (_, result, events) in &before_restart {
        all_references.extend(open_artifact_closure(&state, result));
        all_references.extend(open_artifact_closure(&state, events));
    }

    drop(daemon);
    let restarted = start_daemon(&state, &project, "http://127.0.0.1:1", true, None);
    wait_for_repository(&state);
    let revision_after_restart =
        json_output(cli(&state, &["repo", "revision", "--project", project_id]));
    assert!(
        revision_before_restart["revision"]
            .as_str()
            .is_some_and(|revision| revision.starts_with("r:"))
    );
    assert!(
        revision_after_restart["revision"]
            .as_str()
            .is_some_and(|revision| revision.starts_with("r:"))
    );
    assert_eq!(
        revision_after_restart["digest"],
        revision_before_restart["digest"]
    );
    assert_eq!(tree_digest(&project), tree_before_restart);
    for reference in all_references {
        let bytes = artifact_bytes(&state, &reference);
        if reference.starts_with("blake3:") {
            assert_eq!(
                reference,
                format!("blake3:{}", blake3::hash(&bytes).to_hex())
            );
        }
    }
    for (id, result, events) in before_restart {
        assert_eq!(
            json_output(cli(&state, &["repo", "result", "--result", &id])),
            result
        );
        assert_eq!(
            json_output(cli(&state, &["repo", "events", "--result", &id])),
            events
        );
    }
    drop(restarted);
    fs::remove_dir_all(state.parent().unwrap()).unwrap();
}

#[test]
fn real_provider_preflight_without_network_or_billing() {
    if let Ok(provider) = std::env::var("KIT_PROVIDER") {
        assert!(["openai", "anthropic", "openrouter", "ollama"].contains(&provider.as_str()));
    }
}

#[test]
#[ignore = "explicitly billed real-provider smoke"]
fn real_provider_billing_smoke() {
    assert_eq!(std::env::var("KIT_ALLOW_BILLING").as_deref(), Ok("1"));
    let pins = production_pins();
    let (state, project) = fixture(Some(&pins));
    let _cleanup = FixtureCleanup(state.parent().unwrap().to_owned());
    let daemon = start_real_provider_daemon(&state, &project);
    let status = wait_for_repository(&state);
    let project_id = status["project_id"].as_str().unwrap();
    json_output(cli(&state, &["project", "create", "--id", project_id]));
    let thread = json_output(cli(&state, &["thread", "create", "--project", project_id]));
    let thread_id = thread["resource"]["id"].as_str().unwrap();
    let prompt = json_output(cli(
        &state,
        &[
            "prompt",
            "--thread",
            thread_id,
            "Use repository tools to inspect Kit, make one minimal valid Rust source edit, and report evidence.",
        ],
    ));
    let run_id = prompt["resource"]["id"].as_str().unwrap();
    let completed = wait_run_with_approvals(&state, project_id, run_id);
    assert_eq!(completed["state"], "completed", "{completed}");
    let transcript = json_output(cli(&state, &["run", "transcript", run_id]));
    assert!(transcript.to_string().contains("kit.native_operation_id"));
    let cost = json_output(cli(&state, &["run", "cost", run_id]));
    assert!(cost.to_string().contains("provider_reported"));
    let changed = Command::new("git")
        .args(["status", "--short"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(changed.status.success() && !changed.stdout.is_empty());
    drop(daemon);
    fs::remove_dir_all(state.parent().unwrap()).unwrap();
}

#[test]
#[ignore = "trusted Linux production pin preflight; not release evidence"]
fn trusted_linux_production_preflight() {
    let _ = production_pins();
    assert_eq!(std::env::consts::OS, "linux");
    assert_eq!(
        std::env::var("KIT_TRUSTED_PRODUCTION_DOGFOOD").as_deref(),
        Ok("1")
    );
    println!("{}", trusted_linux_platform_preflight());
}

#[test]
#[ignore = "trusted Linux production dogfood; requires isolation/helper/syntax images"]
fn trusted_linux_production_dogfood_required_for_g04() {
    assert_eq!(std::env::consts::OS, "linux");
    assert_eq!(
        std::env::var("KIT_TRUSTED_PRODUCTION_DOGFOOD").as_deref(),
        Ok("1")
    );
    let pins = production_pins();
    provider_scenario(Some(&pins));
}
