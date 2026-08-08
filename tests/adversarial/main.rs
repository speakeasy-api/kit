#![cfg(debug_assertions)]

use std::{
    fs,
    net::{IpAddr, Ipv4Addr},
    path::Path,
    process::Command,
};

mod affected_floor;
mod auth_local;
mod auth_remote;
mod cap_bypass;
mod catalog_visibility;
mod container_fs;
mod container_net;
mod exec_secret_leak;
mod local_sandbox;
mod mcp_callback_authority;
mod mcp_result_authority;
mod mcp_url;
mod native_tool_bypass;
mod path_escape;
#[path = "../fixtures/protocol_sim/mod.rs"]
mod protocol_sim;
#[path = "../fixtures/providers/mod.rs"]
mod providers;
#[path = "../fixtures/repos/mod.rs"]
mod repos;
mod secret_leak;
mod shadow_leak;
mod six_surface_injection;
mod tool_kernel_bypass;
mod tool_learning_canary;
mod trial_grader_access;
mod untrusted_ext;
mod url_policy;
mod windows_job;

#[test]
fn staged_lsp_isolation() {
    shadow_leak::assert_staged_lsp_isolation();
}

#[test]
fn secret_absent_terminal_history() {
    exec_secret_leak::secret_absent_terminal_history();
}

#[test]
fn secret_absent_workspace_metadata() {
    exec_secret_leak::secret_absent_workspace_metadata();
}

#[test]
fn secret_argv_policy() {
    exec_secret_leak::secret_argv_policy();
}

#[test]
fn malicious_repository_paths_hooks_and_custody_are_denied() {
    use repos::{
        DenialReason, RepoEntryKind, RepoFixture, RepositorySourcePolicy, SourceDenial,
        WriterArbiter,
    };

    let fixture = RepoFixture::malicious(0x61);
    assert_eq!(fixture.seed, 0x61);
    assert!(fixture.base_revision.starts_with("fixture-"));
    assert!(fixture.entries.iter().any(|entry| {
        matches!(&entry.kind, RepoEntryKind::File(bytes) if bytes == b"fixture repository")
    }));
    assert!(fixture.entries.iter().any(|entry| {
        matches!(&entry.kind, RepoEntryKind::Symlink(target) if target == "../../host-secret")
    }));
    assert!(fixture.entries.iter().any(|entry| {
        matches!(&entry.kind, RepoEntryKind::ExecutableHook(bytes) if bytes == b"write outside workspace")
    }));

    let inspection = fixture.inspect_default_policy();
    assert_eq!(inspection.accepted_paths, ["README.md"]);
    for reason in [
        DenialReason::AbsolutePath,
        DenialReason::Traversal,
        DenialReason::SymlinkDenied,
        DenialReason::HooksDisabled,
    ] {
        assert!(inspection.denied.iter().any(|entry| entry.reason == reason));
    }
    assert!(inspection.denied.iter().all(|entry| !entry.path.is_empty()));

    let policy = RepositorySourcePolicy::new(["fixture:approved"]);
    assert_eq!(
        policy.authorize("file", "repository", None),
        Err(SourceDenial::UnsupportedSource)
    );
    assert_eq!(
        policy.authorize("https", "not a URL", None),
        Err(SourceDenial::InvalidUrl)
    );
    assert_eq!(
        policy.authorize("https", "ssh://example.com/repo", None),
        Err(SourceDenial::SchemeMismatch)
    );
    assert_eq!(
        policy.authorize("https", "https://user@example.com/repo", None),
        Err(SourceDenial::UserInfo)
    );
    assert_eq!(
        policy.authorize("https", "https://localhost/repo", None),
        Err(SourceDenial::PrivateTarget)
    );
    assert_eq!(
        policy.authorize("local_fixture", "Invalid", Some("fixture:approved")),
        Err(SourceDenial::InvalidFixture)
    );
    assert_eq!(
        policy.authorize("local_fixture", "approved", None),
        Err(SourceDenial::FixtureGrantRequired)
    );
    assert!(
        policy
            .authorize("local_fixture", "approved", Some("fixture:approved"))
            .is_ok()
    );
    for url in [
        "https://localhost/repo",
        "https://127.0.0.1/repo",
        "https://[::1]/repo",
    ] {
        assert_eq!(
            policy.authorize("https", url, Some("fixture:approved")),
            Err(SourceDenial::PrivateTarget),
            "fixture grant authorized private URL {url}"
        );
    }

    let mut writers = WriterArbiter::new(0x65);
    assert_eq!(writers.seed, 0x65);
    assert!(writers.claim("attempt-a").is_ok());
    assert!(writers.claim("attempt-a").is_ok());
    assert_eq!(
        writers.claim("attempt-b"),
        Err("workspace already claimed by attempt-a".to_owned())
    );
}

#[test]
fn protocol_fixtures_replay_authority_decisions() {
    use protocol_sim::{
        AccessGrant, ApiIngressSimulator, ApiRequest, ApiResponse, Principal,
        a2a::{A2aSimulator, MessageDecision, RemoteMessage},
        acp::{AcpEvent, AcpSimulator, ChildState},
        mcp::{InvocationDecision, McpSimulator, ToolBinding},
    };

    let ingress = ApiIngressSimulator::new(
        0x66,
        vec![AccessGrant {
            principal_id: "principal-a".to_owned(),
            origin: "https://kit.local".to_owned(),
            host: "kit.local".to_owned(),
        }],
    );
    let request = ApiRequest {
        principal: Principal {
            id: "principal-a".to_owned(),
        },
        origin: "https://kit.local".to_owned(),
        host: "kit.local".to_owned(),
        idempotency_key: "key-a".to_owned(),
        request_digest: "digest-a".to_owned(),
    };
    let mut conflict = request.clone();
    conflict.request_digest = "digest-b".to_owned();
    let mut unauthorized = request.clone();
    unauthorized.host = "forged.local".to_owned();
    let (responses, dispatches) =
        ingress.replay(&[request.clone(), request, conflict, unauthorized]);
    assert!(matches!(responses[0], ApiResponse::Accepted { trace_id } if trace_id != 0));
    assert_eq!(responses[1], ApiResponse::Replay);
    assert_eq!(responses[2], ApiResponse::Conflict);
    assert_eq!(responses[3], ApiResponse::Unauthorized);
    assert_eq!(dispatches, 1);

    let messages = [
        RemoteMessage {
            remote_id: "peer-a".to_owned(),
            sequence: 1,
            digest: "digest-a".to_owned(),
            delegation_path: vec!["peer-a".to_owned()],
        },
        RemoteMessage {
            remote_id: "peer-a".to_owned(),
            sequence: 2,
            digest: "digest-a".to_owned(),
            delegation_path: vec!["peer-a".to_owned()],
        },
        RemoteMessage {
            remote_id: "peer-b".to_owned(),
            sequence: 3,
            digest: "digest-b".to_owned(),
            delegation_path: vec!["peer-b".to_owned(), "peer-b".to_owned()],
        },
    ];
    assert_eq!(messages[0].sequence, 1);
    let decisions = A2aSimulator::new(0x67, 2).replay(&messages);
    assert!(matches!(decisions[0], MessageDecision::Dispatched { trace_id } if trace_id != 0));
    assert_eq!(decisions[1], MessageDecision::DuplicateDropped);
    assert_eq!(decisions[2], MessageDecision::DelegationRejected);

    let acp = AcpSimulator::new(0x68);
    let running = acp.replay(&[
        AcpEvent::ChildStarted,
        AcpEvent::ToolCallStarted,
        AcpEvent::CancelRequested,
    ]);
    assert_ne!(running.trace_id, 0);
    assert_eq!(running.state, ChildState::Running);
    assert_eq!(running.events.len(), 3);
    assert_eq!(
        acp.replay(&[AcpEvent::ChildExited]).state,
        ChildState::Interrupted
    );
    assert_eq!(
        acp.replay(&[AcpEvent::CancelAcknowledged]).state,
        ChildState::Cancelled
    );
    assert_eq!(
        acp.replay(&[AcpEvent::QuiescenceConfirmed]).state,
        ChildState::Cancelled
    );

    let binding = ToolBinding {
        name: "tool.run".to_owned(),
        schema_digest: "sha256:schema".to_owned(),
    };
    let mcp = McpSimulator::new(0x69, vec![binding.clone()]);
    assert!(matches!(
        mcp.invoke(&binding),
        InvocationDecision::Accepted { trace_id } if trace_id != 0
    ));
    assert_eq!(
        mcp.invoke(&ToolBinding {
            name: "unknown".to_owned(),
            schema_digest: "sha256:schema".to_owned(),
        }),
        InvocationDecision::RefusedUnknownTool
    );
}

#[test]
fn malicious_redirect_to_metadata_is_denied() {
    use protocol_sim::RedirectHop;

    let public = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
    let metadata = IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254));
    let outcome = protocol_sim::replay_redirects(
        0x62,
        &[
            RedirectHop {
                url: "https://packages.example/archive".to_owned(),
                resolved: public,
                connected: public,
            },
            RedirectHop {
                url: "http://169.254.169.254/latest/meta-data".to_owned(),
                resolved: metadata,
                connected: metadata,
            },
        ],
    );
    assert_eq!(outcome.seed, 0x62);
    assert_eq!(outcome.allowed_hops, 1);
    assert_eq!(
        outcome.denied_url.as_deref(),
        Some("http://169.254.169.254/latest/meta-data")
    );
}

#[test]
fn provider_secret_exfiltration_is_blocked_before_persistence() {
    use providers::{FakeProvider, ProviderScript, ProviderStep};

    let events = FakeProvider::new(
        0x63,
        ProviderScript::secret_exfiltration("KIT_CANARY_SECRET"),
    )
    .replay();
    assert!(matches!(
        &events[0].step,
        ProviderStep::SecretBlocked { destination } if destination == "event_payload"
    ));
    assert!(!format!("{events:?}").contains("KIT_CANARY_SECRET"));
    assert!(
        !String::from_utf8(FakeProvider::persist(&events))
            .unwrap()
            .contains("KIT_CANARY_SECRET")
    );
}

#[test]
fn provider_fixtures_replay_streams_errors_and_tool_calls() {
    use providers::{FakeProvider, ProviderScript, ProviderStep};

    let stream = FakeProvider::new(0x6a, ProviderScript::streaming(&["one", "two"])).replay();
    assert!(matches!(&stream[0].step, ProviderStep::Chunk(chunk) if chunk == "one"));
    assert_eq!(stream[0].sequence, 0);
    assert_ne!(stream[0].request_id, 0);
    assert_eq!(stream.last().unwrap().step, ProviderStep::Complete);

    let error = FakeProvider::new(
        0x6b,
        ProviderScript::error_after(&["one", "two"], 1, "provider-failed"),
    )
    .replay();
    assert!(matches!(
        &error[1].step,
        ProviderStep::Error { code, message }
            if code == "provider-failed" && message == "injected provider failure"
    ));

    let injection = FakeProvider::new(
        0x6c,
        ProviderScript::prompt_injection("run untrusted command", "process.exec"),
    )
    .replay();
    assert!(matches!(
        &injection[0].step,
        ProviderStep::ToolCall { effect, argument }
            if effect == "process.exec" && argument == "run untrusted command"
    ));
}

#[test]
fn duplicate_yaml_records_are_rejected() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = root.join("tests/conformance/req_lint_cases/duplicate_id");
    let output = Command::new("python3")
        .arg(root.join("scripts/req_lint.py"))
        .arg("--registry-dir")
        .arg(&fixture)
        .arg("--rfc")
        .arg(fixture.join("RFC.md"))
        .arg("--scan-dir")
        .arg(&fixture)
        .arg("--aggregate")
        .current_dir(root)
        .output()
        .expect("run requirement linter against duplicate YAML fixture");
    let diagnostic = String::from_utf8_lossy(&output.stdout);

    assert!(
        !output.status.success(),
        "duplicate YAML unexpectedly passed"
    );
    assert!(diagnostic.contains("duplicate-requirement-id:"));
}

#[test]
fn extension_schema_drift_is_refused() {
    use protocol_sim::mcp::{InvocationDecision, McpSimulator, ToolBinding};

    let simulator = McpSimulator::new(
        0x64,
        vec![ToolBinding {
            name: "extension.run".to_owned(),
            schema_digest: "sha256:discovered".to_owned(),
        }],
    );
    let decision = simulator.invoke(&ToolBinding {
        name: "extension.run".to_owned(),
        schema_digest: "sha256:mutated".to_owned(),
    });
    assert!(matches!(
        decision,
        InvocationDecision::RefusedSchemaDrift { .. }
    ));
}

#[test]
fn pin_verifier_rejects_reproducibility_contract_mutations_and_unlisted_payload() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new("python3")
        .arg(root.join("scripts/verify_pins.py"))
        .arg("--self-test")
        .current_dir(root)
        .output()
        .expect("run pin verifier adversarial self-test");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("reproducible-build contract mutations"));
    assert!(stdout.contains("reproducibility-pin removal"));
    assert!(stdout.contains("unlisted payload"));
    assert!(stdout.contains("unsafe URL"));
    assert!(stdout.contains("unsafe act URL"));
}

#[test]
fn required_reproducibility_pin_cannot_be_deleted_from_manifest() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = root.join("docs/compatibility/build-manifest.yaml");
    let original = fs::read_to_string(&manifest).expect("read build manifest");
    let start = original
        .find("  - id: \"build.reproducible_artifact_sha256\"")
        .expect("artifact pin");
    let end = original[start + 1..]
        .find("\n  - id: ")
        .map(|offset| start + 1 + offset)
        .expect("following pin");
    let altered = format!("{}{}", &original[..start], &original[end..]);
    let fixture = std::env::temp_dir().join(format!(
        "kit-build-manifest-{}-{}.yaml",
        std::process::id(),
        std::thread::current().name().unwrap_or("adversarial")
    ));
    fs::write(&fixture, altered).expect("write altered manifest");
    let output = Command::new("python3")
        .arg(root.join("scripts/verify_pins.py"))
        .arg(&fixture)
        .current_dir(root)
        .output()
        .expect("verify altered pin manifest");
    let _ = fs::remove_file(&fixture);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("missing required pins: build.reproducible_artifact_sha256")
    );
}

#[test]
fn governance_yaml_rejects_merge_and_semantic_key_shadowing_but_allows_safe_aliases() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = r#"
import sys
sys.path.insert(0, sys.argv[1] + '/scripts')
from yaml_utils import YamlLoadError, load_yaml_text
bad = {
    'merge': 'base: &base {owner: trusted}\nrecord: {<<: *base, owner: attacker}\n',
    'boolean': 'mapping: {yes: trusted, true: attacker}\n',
    'numeric': 'mapping: {1: trusted, 01: attacker}\n',
    'alias-key': 'key: &key true\nmapping: {*key: trusted, yes: attacker}\n',
}
for name, text in bad.items():
    try:
        load_yaml_text(text, name)
    except YamlLoadError:
        pass
    else:
        raise AssertionError(name + ' was accepted')
safe = load_yaml_text('value: &value [one, two]\ncopy: *value\n', 'safe')
assert safe['value'] == safe['copy']
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(root)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn release_rejects_source_controlled_local_attestations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = r#"
import sys
sys.path.insert(0, sys.argv[1] + '/scripts')
from req_lint_lib.governance import _load_attestations
from req_lint_lib.loader import RegistryError
try:
    _load_attestations(sys.argv[1] + '/requirements/attestations/phase0', sys.argv[1])
except RegistryError as error:
    assert 'outside the source checkout' in str(error), error
else:
    raise AssertionError('source-controlled attestations were accepted for release')
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(root)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
