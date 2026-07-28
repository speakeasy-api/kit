use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{Read, Write},
    path::Path,
};

use chacha20poly1305::{ChaCha20Poly1305, KeyInit, aead::Aead};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[path = "../eval/graders/core/mod.rs"]
mod core_grader;

use core_grader::{
    AuthenticatedChannel, Check, GradeMetadata, GraderBounds, HiddenTestManifest, SourceSnapshot,
};

const MAX_PROTOCOL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema_version: u16,
    source: Vec<(String, Vec<u8>)>,
    patch: Vec<u8>,
    checks: Vec<Check>,
    bounds: GraderBounds,
    hidden_tests: Vec<u8>,
    gold_patch: Vec<u8>,
    acceptance_rules: Vec<u8>,
    harness_config: Vec<u8>,
}

#[derive(Serialize)]
struct Response {
    schema_version: u16,
    report: GradeMetadata,
    channels: Vec<AuthenticatedChannel>,
    input_digests: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct CheckChannel<'a> {
    public: &'a [core_grader::CheckEvidence],
    hidden: &'a core_grader::HiddenCheckAggregate,
}

pub fn worker_main(arguments: &[OsString]) -> Option<std::process::ExitCode> {
    (arguments.len() == 2
        && arguments.get(1).and_then(|value| value.to_str()) == Some("__kit-core-grader"))
    .then(|| match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "{error}");
            std::process::ExitCode::FAILURE
        }
    })
}

fn run() -> Result<(), String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_PROTOCOL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_PROTOCOL_BYTES {
        return Err("grader protocol request exceeded bound".to_owned());
    }
    let request: Request =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid request: {error}"))?;
    if request.schema_version != 1 {
        return Err("unsupported grader protocol version".to_owned());
    }
    let source =
        SourceSnapshot::new(request.source, &request.bounds).map_err(|error| error.to_string())?;
    let acceptance: Vec<Check> = serde_json::from_slice(&request.acceptance_rules)
        .map_err(|error| format!("invalid acceptance rules: {error}"))?;
    if acceptance != request.checks {
        return Err("acceptance rules and declared checks differ".to_owned());
    }
    let hidden: HiddenTestManifest = serde_json::from_slice(&request.hidden_tests)
        .map_err(|error| format!("invalid hidden-test manifest: {error}"))?;
    if hidden.schema_version != 1 {
        return Err("unsupported hidden-test manifest version".to_owned());
    }
    let config: serde_json::Value = serde_json::from_slice(&request.harness_config)
        .map_err(|error| format!("invalid harness config: {error}"))?;
    for (field, actual) in [
        (
            "hidden_tests_digest",
            core_grader::sha256(&request.hidden_tests),
        ),
        (
            "acceptance_rules_digest",
            core_grader::sha256(&request.acceptance_rules),
        ),
        (
            "gold_patch_digest",
            core_grader::sha256(&request.gold_patch),
        ),
    ] {
        if config.get(field).and_then(serde_json::Value::as_str) != Some(actual.as_str()) {
            return Err(format!("harness config {field} mismatch"));
        }
    }
    let report = core_grader::grade_with_hidden(
        &source,
        &request.patch,
        &request.checks,
        &hidden.checks,
        &request.bounds,
    )
    .map_err(|error| error.to_string())?;
    let artifact_root = std::env::var_os("KIT_CORE_GRADER_ARTIFACT_ROOT")
        .ok_or_else(|| "grader artifact root is missing".to_owned())?;
    let artifact_root = Path::new(&artifact_root);
    let auth_key = std::env::var("KIT_CORE_GRADER_AUTH_KEY")
        .map_err(|_| "grader channel authentication key is missing".to_owned())?;
    let checks = serde_json::to_vec(&CheckChannel {
        public: &report.checks,
        hidden: &report.hidden,
    })
    .map_err(|error| error.to_string())?;
    let hidden_checks = encrypt_hidden(
        auth_key.as_bytes(),
        &serde_json::to_vec(&report.hidden_checks).map_err(|error| error.to_string())?,
    )?;
    let events = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "phases": ["agent", "grader"],
        "patch_digest": &report.patch_digest,
    }))
    .map_err(|error| error.to_string())?;
    let agent_output = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "patch_digest": &report.patch_digest,
    }))
    .map_err(|error| error.to_string())?;
    let usage = serde_json::to_vec(&kit::executor::trial::TrialUsage::ZERO)
        .map_err(|error| error.to_string())?;
    let channels = [
        ("diff", "applied.patch", request.patch.as_slice()),
        (
            "file",
            "final-tree.json",
            report.final_tree_artifact.as_slice(),
        ),
        ("report", "checks.json", checks.as_slice()),
        ("index", "events.json", events.as_slice()),
        ("log", "grader.log", b"core grader completed\n".as_slice()),
        ("report", "agent-output.json", agent_output.as_slice()),
        ("report", "usage.json", usage.as_slice()),
        (
            "restricted_encrypted",
            "hidden-checks.enc",
            hidden_checks.as_slice(),
        ),
    ]
    .into_iter()
    .map(|(class, handle, bytes)| write_channel(artifact_root, &auth_key, class, handle, bytes))
    .collect::<Result<Vec<_>, _>>()?;
    let output = serde_json::to_vec(&Response {
        schema_version: 1,
        report: GradeMetadata::from(&report),
        channels,
        input_digests: BTreeMap::from([
            (
                "acceptance_rules".to_owned(),
                core_grader::sha256(&request.acceptance_rules),
            ),
            (
                "gold_patch".to_owned(),
                core_grader::sha256(&request.gold_patch),
            ),
            (
                "harness_config".to_owned(),
                core_grader::sha256(&request.harness_config),
            ),
            (
                "hidden_tests".to_owned(),
                core_grader::sha256(&request.hidden_tests),
            ),
        ]),
    })
    .map_err(|error| error.to_string())?;
    if output.len() > MAX_RESULT_BYTES {
        return Err("grader protocol response exceeded bound".to_owned());
    }
    std::io::stdout()
        .write_all(&output)
        .map_err(|error| error.to_string())
}

fn encrypt_hidden(key_material: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let key = Sha256::digest(key_material);
    let cipher = ChaCha20Poly1305::new_from_slice(&key).map_err(|error| error.to_string())?;
    let mut nonce = [0_u8; 12];
    getrandom::fill(&mut nonce)
        .map_err(|_| "grader encryption randomness unavailable".to_owned())?;
    let mut encrypted = b"kit-hidden-checks-v1\0".to_vec();
    encrypted.extend_from_slice(&nonce);
    encrypted.extend_from_slice(
        &cipher
            .encrypt((&nonce).into(), plaintext)
            .map_err(|_| "hidden-check encryption failed".to_owned())?,
    );
    Ok(encrypted)
}

fn write_channel(
    root: &Path,
    auth_key: &str,
    class: &str,
    handle: &str,
    bytes: &[u8],
) -> Result<AuthenticatedChannel, String> {
    if handle.contains(['/', '\\']) {
        return Err("invalid grader channel handle".to_owned());
    }
    let path = root.join(handle);
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    let digest = core_grader::sha256(bytes);
    let length = bytes.len() as u64;
    Ok(AuthenticatedChannel {
        class: class.to_owned(),
        handle: handle.to_owned(),
        digest: digest.clone(),
        length,
        authentication: core_grader::channel_authentication(
            auth_key.as_bytes(),
            class,
            handle,
            &digest,
            length,
        ),
    })
}
