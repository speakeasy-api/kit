use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::Arc,
    time::Duration,
};

use kit::{
    agent::driver::restart::{EffectIntentPayload, EffectJournalRecord},
    agent::executor::{
        FakeProvider, FakeResponse, FakeScenario, RunExecutor, RunExecutorConfig,
        SelectedModelAdapter,
    },
    agent::prompt::{PromptInput, TaskContract},
    api::{
        auth::{
            contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot, ScopedAuthorizer},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        http::exec::ManagerExecService,
        service::{
            Command, PromptCommand, PromptInput as ServicePromptInput, Query, QueryProjection,
            RequestContext, ServiceStore, WorkerStore,
        },
    },
    domain::{
        config::{
            Grant, Provider as ConfigProvider, RunConfigSnapshot, StaticRunConfigMaterializer,
        },
        events::{RunState, SchemaVersion, TraceId},
        ids::{AttemptId, EventId, PrincipalId, ProjectId, RunId, ThreadId},
        lifecycle::{AttemptOwnership, FencingToken, ProcessOwnership},
        projections::PersistedCommand,
        secret::{REDACTED, SecretCustody, SecretLease},
    },
    executor::{
        cancel::SqliteCancellationCoordinator,
        process::own::{ProcessRegistrationContext, ProcessRegistry, ProcessRegistryRegistration},
        profile::ResourceLimits,
        terminal::{
            NativePtyDriver, OutputRetention, SqliteTerminalSnapshotStore, TerminalManager,
            TerminalSize,
        },
    },
    protocols::mcp::features::{PayloadLimits, RawPayload},
    runtime::scheduler::DurableScheduler,
    store::{
        artifacts::{
            ArtifactClass, ArtifactDigest, ArtifactMetadata, ArtifactRetention, ArtifactStore,
            now_unix_micros,
        },
        sqlite::idempotency::IdempotencyKey,
    },
    telemetry::{
        otel::{
            Adapter, AttributeValue, DropPolicy, DurableLocalExporter, Resource, Span, SpanEvent,
            SpanKind, SpanName, SpanStatus, TelemetryItem,
        },
        redact::{CaptureBoundary, CapturePersistencePolicy},
    },
    test_support,
    workspace::{
        index::meta::{IndexOptions, MetadataIndex},
        revision::{ManagedWorkspace, RevisionOptions},
        search::lexical::{SearchMode, SearchOptions, SearchQuery, search_projected},
    },
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};

const SURFACES: [&str; 6] = [
    "prompts",
    "events",
    "traces",
    "composition inputs",
    "terminal history",
    "workspace metadata",
];

const SURFACE_INGRESS: [(&str, &str); 6] = [
    ("prompts", "tool_output"),
    ("events", "provider_event_payload"),
    ("traces", "mcp_resource_response"),
    ("composition inputs", "nested_composition_input"),
    ("terminal history", "terminal_output_history"),
    ("workspace metadata", "workspace_filename_metadata"),
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u16,
    corpus_id: String,
    version: String,
    license: String,
    provenance: String,
    corpus_file: String,
    corpus_sha256: String,
    surfaces: Vec<String>,
    canaries: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    schema_version: u16,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    ingress: String,
    target: String,
    payload: String,
}

struct LoadedCorpus {
    manifest: Manifest,
    corpus: Corpus,
}

impl LoadedCorpus {
    fn case(&self, ingress: &str) -> &Case {
        self.corpus
            .cases
            .iter()
            .find(|case| case.ingress == ingress)
            .unwrap_or_else(|| panic!("missing corpus ingress {ingress}"))
    }

    fn surface_case(&self, surface: &str) -> &Case {
        let ingress = SURFACE_INGRESS
            .iter()
            .find_map(|(name, ingress)| (*name == surface).then_some(*ingress))
            .unwrap_or_else(|| panic!("unknown surface {surface}"));
        self.case(ingress)
    }
}

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "kit-six-surface-{label}-{}",
            EventId::generate().unwrap()
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn load_corpus() -> LoadedCorpus {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("eval/corpora/injection");
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(root.join("manifest.json")).unwrap()).unwrap();
    let corpus_bytes = fs::read(root.join(&manifest.corpus_file)).unwrap();
    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.corpus_id, "kit-m006-six-surface-injection");
    assert_eq!(manifest.version, "1.0.0");
    assert_eq!(manifest.license, "CC0-1.0");
    assert!(manifest.provenance.contains("no real credentials"));
    assert_eq!(
        manifest.corpus_sha256,
        format!("{:x}", Sha256::digest(&corpus_bytes))
    );
    let corpus: Corpus = serde_json::from_slice(&corpus_bytes).unwrap();
    assert_eq!(corpus.schema_version, 1);
    assert_eq!(corpus.cases.len(), 7);
    assert!(corpus.cases.iter().all(|case| {
        !case.id.is_empty()
            && matches!(
                case.target.as_str(),
                "authority_expansion" | "tool_invocation" | "exfiltration"
            )
    }));
    LoadedCorpus { manifest, corpus }
}

fn validate_exact_whitelist<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Result<(), &'static str> {
    let values = values.into_iter().collect::<Vec<_>>();
    let actual = values.iter().copied().collect::<BTreeSet<_>>();
    let expected = SURFACES.into_iter().collect::<BTreeSet<_>>();
    (values.len() == SURFACES.len() && actual == expected)
        .then_some(())
        .ok_or("surface whitelist must have exactly one of each required surface")
}

fn custody(corpus: &LoadedCorpus) -> SecretCustody {
    SecretCustody::new(
        corpus
            .manifest
            .canaries
            .iter()
            .map(|value| Arc::new(SecretLease::new(value.as_bytes().to_vec()))),
    )
}

fn encoded_ingress(corpus: &LoadedCorpus, ingress: &str, vector: usize) -> String {
    let payload = &corpus.case(ingress).payload;
    let canary = corpus
        .manifest
        .canaries
        .iter()
        .find(|canary| payload.contains(canary.as_str()))
        .unwrap();
    let encoded = String::from_utf8(encoded_vectors(canary.as_bytes())[vector].clone()).unwrap();
    payload.replace(canary, &encoded)
}

fn prompt_export(corpus: &LoadedCorpus, custody: &SecretCustody) -> Vec<u8> {
    let root = TempDirectory::new("prompt");
    let database = root.path().join("state.sqlite3");
    let artifact_root = root.path().join("artifacts");
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let thread_id = ThreadId::generate().unwrap();
    let principal = authenticate(
        principal_id,
        project_id,
        [
            Grant::WorkspaceRead,
            Grant::WorkspaceWrite,
            Grant::ModelCall,
        ],
    );
    let artifacts = ArtifactStore::open(&artifact_root).unwrap();
    let mut service = test_support::project_service_with_runtime(
        test_support::open_project_service_store(&database, custody.clone()).unwrap(),
        ScopedAuthorizer,
        artifacts,
        custody.clone(),
    );
    for (key, command) in [
        (
            "prompt-project",
            Command::CreateProject {
                schema_version: SchemaVersion::CURRENT,
                project_id,
            },
        ),
        (
            "prompt-thread",
            Command::CreateThread {
                schema_version: SchemaVersion::CURRENT,
                thread_id,
                project_id,
            },
        ),
    ] {
        service
            .execute(
                &RequestContext::authenticated(
                    Ok(principal.clone()),
                    Some(IdempotencyKey::parse(key).unwrap()),
                    TraceId::parse(key).unwrap(),
                )
                .unwrap(),
                command,
            )
            .unwrap();
    }
    let receipt = service
        .prompt(
            &RequestContext::authenticated(
                Ok(principal),
                Some(IdempotencyKey::parse("prompt-message").unwrap()),
                TraceId::parse("prompt-message").unwrap(),
            )
            .unwrap(),
            PromptCommand {
                thread_id,
                run_id: None,
                input: ServicePromptInput::Message(format!(
                    "surface-pointer:prompts {}",
                    encoded_ingress(corpus, "tool_output", 1)
                )),
                run_config: None,
                experiment_config: None,
            },
        )
        .unwrap();
    let QueryProjection::Run(run) = service
        .store_mut()
        .query(&Query::GetRun {
            run_id: receipt.run_id,
        })
        .unwrap()
    else {
        panic!("prompt run projection missing")
    };
    ArtifactStore::open(artifact_root)
        .unwrap()
        .open_bytes(ArtifactDigest::parse(run.input.as_str()).unwrap())
        .unwrap()
}

fn event_export(corpus: &LoadedCorpus, custody: &SecretCustody) -> Vec<u8> {
    let root = TempDirectory::new("event");
    let database = root.path().join("events.sqlite3");
    let artifact_root = root.path().join("artifacts");
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let thread_id = ThreadId::generate().unwrap();
    let run_id = RunId::generate().unwrap();
    let input = ArtifactStore::open(&artifact_root)
        .unwrap()
        .put(
            b"event producer input",
            ArtifactMetadata::new(
                "text/plain; charset=utf-8",
                ArtifactClass::File,
                principal_id.to_string(),
                project_id.to_string(),
                ArtifactRetention::Forever,
                now_unix_micros().unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let principal = authenticate(
        principal_id,
        project_id,
        [
            Grant::WorkspaceRead,
            Grant::WorkspaceWrite,
            Grant::ModelCall,
        ],
    );
    let mut service = test_support::service_with_runtime_and_config(
        test_support::open_project_service_store(&database, custody.clone()).unwrap(),
        ScopedAuthorizer,
        ArtifactStore::open(&artifact_root).unwrap(),
        StaticRunConfigMaterializer::for_provider(ConfigProvider::OpenAi),
    );
    for (key, command) in [
        (
            "event-project",
            Command::CreateProject {
                schema_version: SchemaVersion::CURRENT,
                project_id,
            },
        ),
        (
            "event-thread",
            Command::CreateThread {
                schema_version: SchemaVersion::CURRENT,
                thread_id,
                project_id,
            },
        ),
        (
            "event-run",
            Command::StartRun {
                schema_version: SchemaVersion::CURRENT,
                run_id,
                thread_id,
                input: input.digest().to_string().parse().unwrap(),
                run_config: None,
                experiment_config: None,
                effective_config: None,
            },
        ),
    ] {
        service
            .execute(
                &RequestContext::authenticated(
                    Ok(principal.clone()),
                    Some(IdempotencyKey::parse(key).unwrap()),
                    TraceId::parse(key).unwrap(),
                )
                .unwrap(),
                command,
            )
            .unwrap();
    }
    let mut store = service.into_store();
    let job = store
        .claim_queued_run(Duration::from_secs(30))
        .unwrap()
        .unwrap();
    store
        .publish_run_progress(
            run_id,
            job.claim,
            kit::api::service::RunProgressRecord {
                sequence: 1,
                model_call_id: None,
                kind: "provider_event_payload".to_owned(),
                content: json!({
                    "pointer": "surface-pointer:events",
                    "provider": encoded_ingress(corpus, "provider_event_payload", 3),
                }),
            },
        )
        .unwrap();
    let mut service = test_support::service_with_runtime_and_config(
        store,
        ScopedAuthorizer,
        ArtifactStore::open(&artifact_root).unwrap(),
        StaticRunConfigMaterializer::for_provider(ConfigProvider::OpenAi),
    );
    let QueryProjection::Events(readback) = service
        .query(
            &RequestContext::authenticated(
                Ok(principal),
                None,
                TraceId::parse("event-readback").unwrap(),
            )
            .unwrap(),
            Query::RunTimeline {
                run_id,
                after: kit::api::service::EventCursor::START,
                limit: 100,
                opaque_cursor: None,
            },
        )
        .unwrap()
    else {
        panic!("event API did not return a timeline")
    };
    readback
        .events
        .into_iter()
        .find(|event| event.operation == "run.progress")
        .unwrap()
        .envelope
}

fn trace_export(corpus: &LoadedCorpus, custody: &SecretCustody) -> Vec<u8> {
    let root = TempDirectory::new("trace");
    let path = root.path().join("telemetry.bin");
    let payload = encoded_ingress(corpus, "mcp_resource_response", 6);
    let mut adapter = Adapter::with_custody(
        Resource {
            attributes: BTreeMap::from([(
                "surface".to_owned(),
                AttributeValue::String(format!("surface-pointer:traces {payload}")),
            )]),
        },
        custody,
        4,
        DropPolicy::DropNewest,
    )
    .unwrap();
    adapter
        .enqueue(TelemetryItem::Span(Span {
            trace_id: "11111111111111111111111111111111".to_owned(),
            span_id: "2222222222222222".to_owned(),
            parent_span_id: None,
            span_name: SpanName::ApiCommand,
            kind: SpanKind::Server,
            start_unix_nanos: 1,
            end_unix_nanos: 2,
            attributes: BTreeMap::from([(
                "mcp.resource".to_owned(),
                AttributeValue::String(payload.clone()),
            )]),
            events: vec![SpanEvent {
                name: payload.clone(),
                timestamp_unix_nanos: 1,
                attributes: BTreeMap::new(),
            }],
            status: SpanStatus::Error(Some(payload.clone())),
        }))
        .unwrap();
    let key = [0x71; 32];
    let mut exporter = DurableLocalExporter::open(&path, &key, 1024 * 1024).unwrap();
    assert_eq!(adapter.flush(&mut exporter).unwrap(), 1);
    let encrypted = fs::read(&path).unwrap();
    assert!(
        !encrypted
            .windows(payload.len())
            .any(|window| window == payload.as_bytes())
    );
    exporter.read_batches().unwrap()[0]
        .to_canonical_json()
        .unwrap()
}

fn composition_export(corpus: &LoadedCorpus, custody: &SecretCustody) -> Vec<u8> {
    let payload = encoded_ingress(corpus, "nested_composition_input", 7);
    let input = PromptInput {
        task: TaskContract {
            goal: format!("surface-pointer:composition inputs {payload}"),
            ..TaskContract::default()
        },
        retrieved_evidence: BTreeMap::from([(
            "composition-artifact".to_owned(),
            format!("surface-pointer:composition-artifact {payload}"),
        )]),
        ..PromptInput::default()
    };
    serde_json::to_vec(&test_support::project_composition_input(custody, &input).unwrap()).unwrap()
}

fn terminal_export(corpus: &LoadedCorpus, custody: &SecretCustody) -> Vec<u8> {
    let root = TempDirectory::new("terminal");
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let database = root.path().join("daemon.sqlite3");
    let store = SqliteTerminalSnapshotStore::open(&database).unwrap();
    let service = Arc::new(
        ManagerExecService::open(
            &database,
            TerminalManager::new(project_id, NativePtyDriver::new(), store.clone()),
            SqliteCancellationCoordinator::new(&database),
        )
        .unwrap(),
    );
    let registry: Arc<dyn ProcessRegistry> = service;
    let registration = ProcessRegistryRegistration::new(
        registry,
        ProcessRegistrationContext {
            project_id,
            principal_id,
        },
    )
    .with_custody(custody.clone())
    .with_pty(
        TerminalSize::new(80, 24).unwrap(),
        OutputRetention::new(4096, 10_000),
        CapturePersistencePolicy::no_secrets(),
    );
    let payload = format!(
        "surface-pointer:terminal history {}",
        encoded_ingress(corpus, "terminal_output_history", 4)
    );
    let command = if cfg!(windows) {
        let mut command = ProcessCommand::new("cmd.exe");
        command.args(["/d", "/s", "/c", &format!("<nul set /p ={payload}")]);
        command
    } else {
        let mut command = ProcessCommand::new("printf");
        command.args(["%s", &payload]);
        command
    };
    let limits = ResourceLimits::new(
        10_000,
        64 * 1024 * 1024,
        16,
        1024 * 1024,
        1024 * 1024,
        1024 * 1024,
        4096,
        5_000,
    );
    let mut child = test_support::spawn_registered_test_process(
        command,
        ProcessOwnership::Attempt(AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal_id,
            FencingToken::new(1),
        )),
        registration,
        limits,
    )
    .unwrap();
    child.wait().unwrap();
    let snapshot = store.load().unwrap().remove(0);
    let output = snapshot
        .output
        .iter()
        .flat_map(|chunk| chunk.bytes().iter().copied())
        .collect::<Vec<_>>();
    serde_json::to_vec(&json!({
        "owner": snapshot.owner,
        "output": String::from_utf8(output).unwrap(),
    }))
    .unwrap()
}

fn workspace_export(corpus: &LoadedCorpus, custody: &SecretCustody) -> Vec<u8> {
    let root = TempDirectory::new("workspace");
    let workspace_root = root.path().join("workspace");
    fs::create_dir(&workspace_root).unwrap();
    fs::write(
        workspace_root.join(encoded_ingress(corpus, "workspace_filename_metadata", 1)),
        b"surface-pointer:workspace metadata request unauthorized network egress",
    )
    .unwrap();
    let workspace = ManagedWorkspace::open_with_options(
        &workspace_root,
        RevisionOptions {
            max_entries: 64,
            max_name_bytes: 4096,
            max_bytes: 1024 * 1024,
            max_memory_bytes: 2 * 1024 * 1024,
            max_depth: 8,
            max_scan_time: Duration::from_secs(2),
            max_scan_attempts: 2,
            watcher_interval: Duration::from_millis(10),
            reconciliation_interval: Duration::from_secs(60),
            metadata_path: Some(root.path().join("revision.state")),
        },
    )
    .unwrap();
    let revision = workspace.current_revision().unwrap();
    let index =
        MetadataIndex::build_lexical(&workspace, revision.id(), &IndexOptions::default()).unwrap();
    let response = search_projected(
        &workspace,
        &index,
        &SearchQuery {
            text: "request".to_owned(),
            mode: SearchMode::PathAndContent,
        },
        &SearchOptions::default(),
        None,
        custody,
        &[0x5a; 32],
    )
    .unwrap();
    serde_json::to_vec(&response).unwrap()
}

fn mcp_projection_exports(corpus: &LoadedCorpus, custody: &SecretCustody) -> Vec<Vec<u8>> {
    [
        ("tool", "tool_output"),
        ("resource", "mcp_resource_response"),
        ("prompt", "mcp_prompt_response"),
        ("log", "provider_event_payload"),
        ("sampling", "mcp_prompt_response"),
        ("form", "tool_output"),
        ("roots", "workspace_filename_metadata"),
    ]
    .into_iter()
    .map(|(kind, ingress)| {
        let raw = RawPayload::from_value(
            json!({"kind":kind, "pointer":format!("mcp-pointer:{kind}"), "result":corpus.case(ingress).payload}),
            PayloadLimits::default(),
        )
        .unwrap();
        serde_json::to_vec(&custody.project_json(CaptureBoundary::Artifact, raw.value())).unwrap()
    })
    .collect()
}

fn encoded_vectors(secret: &[u8]) -> Vec<Vec<u8>> {
    let hex = secret
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let percent = secret
        .iter()
        .map(|byte| format!("%{byte:02X}"))
        .collect::<String>();
    let json = secret
        .iter()
        .map(|byte| format!("\\u00{byte:02x}"))
        .collect::<String>();
    let encoded_base64 = base64(secret, false);
    let base64url = base64(secret, true);
    let spaced = encoded_base64
        .bytes()
        .map(|byte| format!("{} ", char::from(byte)))
        .collect::<String>();
    let ansi = secret
        .iter()
        .map(|byte| format!("{}\u{1b}[31m", char::from(*byte)))
        .collect::<String>();
    vec![
        secret.to_vec(),
        hex.into_bytes(),
        format!(
            "0x{}",
            secret
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        )
        .into_bytes(),
        encoded_base64.into_bytes(),
        base64url.into_bytes(),
        spaced.into_bytes(),
        percent.into_bytes(),
        json.into_bytes(),
        ansi.into_bytes(),
    ]
}

fn base64(source: &[u8], url: bool) -> String {
    let alphabet = if url {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let mut output = Vec::new();
    for chunk in source.chunks(3) {
        let bits = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(alphabet[((bits >> 18) & 63) as usize]);
        output.push(alphabet[((bits >> 12) & 63) as usize]);
        if chunk.len() > 1 {
            output.push(alphabet[((bits >> 6) & 63) as usize]);
        }
        if chunk.len() > 2 {
            output.push(alphabet[(bits & 63) as usize]);
        }
    }
    String::from_utf8(output).unwrap()
}

fn authenticate(
    principal_id: PrincipalId,
    project_id: ProjectId,
    grants: impl IntoIterator<Item = Grant>,
) -> AuthenticatedPrincipal {
    LocalPeerAuthenticator::new(BTreeMap::from([(
        714,
        GrantSnapshot::new(principal_id, project_id, grants),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(714, 1, 714))
    .unwrap()
}

#[derive(Debug)]
struct AuthorityReceipt {
    state: RunState,
    persisted_tool_request: bool,
    authority_before: AuthoritySnapshot,
    authority_after: AuthoritySnapshot,
    durable_effect_intents: usize,
    kernel_intents: usize,
    kernel_denials: usize,
    kernel_dispatches: usize,
    broker_denials: usize,
    effect_dispatches: usize,
}

#[derive(Debug, Eq, PartialEq)]
struct AuthoritySnapshot {
    authenticated_grants: BTreeSet<Grant>,
    authenticated_grant_digest: String,
    effective_grants: BTreeSet<Grant>,
    effective_config_digest: String,
    effective_config_revision: u32,
    broker_granted: Option<bool>,
    broker_request_revisions: BTreeMap<RunId, u64>,
}

fn authority_snapshot<S>(
    store: &mut S,
    principal: &AuthenticatedPrincipal,
    project_id: ProjectId,
    run_id: RunId,
) -> AuthoritySnapshot
where
    S: ServiceStore + WorkerStore,
{
    let QueryProjection::Run(run) = store.query(&Query::GetRun { run_id }).unwrap() else {
        panic!("unexpected run projection")
    };
    let QueryProjection::AuthRequests(requests) = store
        .query(&Query::PendingAuthRequests { project_id })
        .unwrap()
    else {
        panic!("unexpected auth request projection")
    };
    let config = store
        .worker_append_store()
        .unwrap()
        .events()
        .unwrap()
        .into_iter()
        .filter(|event| event.event.event_type.as_str() == "run.start")
        .find_map(|event| {
            let persisted =
                serde_json::from_slice::<PersistedCommand>(&event.event.payload).ok()?;
            match persisted.command {
                Command::StartRun {
                    run_id: found,
                    effective_config: Some(config),
                    ..
                } if found == run_id => Some(config),
                _ => None,
            }
        })
        .unwrap();
    let config = RunConfigSnapshot::from_canonical_bytes(&config).unwrap();
    assert_eq!(config.reference(), run.effective_config);
    let authenticated_grants = principal.grant_snapshot().grants().clone();
    let authenticated_grant_digest = format!(
        "sha256:{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!({
                "principal_id": principal.principal_id(),
                "project_id": principal.grant_snapshot().project_id(),
                "grants": &authenticated_grants,
            }))
            .unwrap()
        )
    );
    AuthoritySnapshot {
        authenticated_grants,
        authenticated_grant_digest,
        effective_grants: config.effective_authority().clone(),
        effective_config_digest: format!("sha256:{}", config.digest_hex()),
        effective_config_revision: config.version(),
        broker_granted: run.auth_granted,
        broker_request_revisions: requests
            .into_iter()
            .map(|request| (request.run_id, request.version))
            .collect(),
    }
}

fn assert_authority_unchanged(label: &str, before: &AuthoritySnapshot, after: &AuthoritySnapshot) {
    let added = after
        .authenticated_grants
        .difference(&before.authenticated_grants)
        .copied()
        .collect::<BTreeSet<_>>();
    let revoked = before
        .authenticated_grants
        .difference(&after.authenticated_grants)
        .copied()
        .collect::<BTreeSet<_>>();
    assert!(
        added.is_empty(),
        "{label}: grants added or expanded: {added:?}"
    );
    assert!(revoked.is_empty(), "{label}: grants revoked: {revoked:?}");
    assert_eq!(
        after.authenticated_grants, before.authenticated_grants,
        "{label}: grant set replaced"
    );
    assert_eq!(
        after.authenticated_grant_digest, before.authenticated_grant_digest,
        "{label}: grant digest changed"
    );

    let expanded = after
        .effective_grants
        .difference(&before.effective_grants)
        .copied()
        .collect::<BTreeSet<_>>();
    let contracted = before
        .effective_grants
        .difference(&after.effective_grants)
        .copied()
        .collect::<BTreeSet<_>>();
    assert!(
        expanded.is_empty(),
        "{label}: effective grants expanded: {expanded:?}"
    );
    assert!(
        contracted.is_empty(),
        "{label}: effective grants revoked: {contracted:?}"
    );
    assert_eq!(
        after.effective_grants, before.effective_grants,
        "{label}: effective grant set replaced"
    );
    assert_eq!(
        after.effective_config_digest, before.effective_config_digest,
        "{label}: effective config digest changed"
    );
    assert_eq!(
        after.effective_config_revision, before.effective_config_revision,
        "{label}: effective config revision changed"
    );
    assert_eq!(
        after.broker_granted, before.broker_granted,
        "{label}: broker grant state changed"
    );
    assert_eq!(
        after.broker_request_revisions, before.broker_request_revisions,
        "{label}: broker request set or revision changed"
    );
}

async fn run_authority_conversation(label: &str, input: &[u8]) -> AuthorityReceipt {
    let key_label = label.replace(' ', "-");
    let root = TempDirectory::new(&format!("authority-{label}"));
    let project_root = root.path().join("project");
    fs::create_dir(&project_root).unwrap();
    fs::write(
        project_root.join("README.md"),
        "six-surface authority fixture\n",
    )
    .unwrap();
    init_git(&project_root);

    let database = root.path().join("state.sqlite3");
    let artifact_root = root.path().join("artifacts");
    let artifacts = Arc::new(ArtifactStore::open(&artifact_root).unwrap());
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let thread_id = ThreadId::generate().unwrap();
    let run_id = RunId::generate().unwrap();
    let authenticator = LocalPeerAuthenticator::new(BTreeMap::from([(
        714,
        GrantSnapshot::new(
            principal_id,
            project_id,
            [
                Grant::ModelCall,
                Grant::WorkspaceRead,
                Grant::WorkspaceWrite,
            ],
        ),
    )]));
    let current_authority = || {
        authenticator
            .authenticate(&LocalPeerObservation::from_transport(714, 1, 714))
            .unwrap()
    };
    let principal = current_authority();
    let input = artifacts
        .put(
            input,
            ArtifactMetadata::new(
                "application/json",
                ArtifactClass::File,
                principal_id.to_string(),
                project_id.to_string(),
                ArtifactRetention::Forever,
                now_unix_micros().unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let mut service = test_support::service_with_runtime_and_config(
        test_support::open_service_store(&database).unwrap(),
        ScopedAuthorizer,
        ArtifactStore::open(&artifact_root).unwrap(),
        StaticRunConfigMaterializer::for_provider(ConfigProvider::OpenAi),
    );
    for (key, command) in [
        (
            "project",
            Command::CreateProject {
                schema_version: SchemaVersion::CURRENT,
                project_id,
            },
        ),
        (
            "thread",
            Command::CreateThread {
                schema_version: SchemaVersion::CURRENT,
                thread_id,
                project_id,
            },
        ),
        (
            "run",
            Command::StartRun {
                schema_version: SchemaVersion::CURRENT,
                run_id,
                thread_id,
                input: input.digest().to_string().parse().unwrap(),
                run_config: None,
                experiment_config: None,
                effective_config: None,
            },
        ),
    ] {
        let context = RequestContext::authenticated(
            Ok(principal.clone()),
            Some(IdempotencyKey::parse(&format!("{key_label}-{key}")).unwrap()),
            TraceId::parse(&format!("{key_label}-{key}")).unwrap(),
        )
        .unwrap();
        service.execute(&context, command).unwrap();
    }
    let authority_before = authority_snapshot(
        service.store_mut(),
        &current_authority(),
        project_id,
        run_id,
    );
    let store = Arc::new(std::sync::Mutex::new(service.into_store()));
    let scheduler = DurableScheduler::open(&database).unwrap();
    let provider = Arc::new(FakeProvider::with_scenario(
        FakeResponse::completed("authority check complete"),
        FakeScenario::ReactiveInjection,
    ));
    let mut config = RunExecutorConfig::new(
        &database,
        Arc::clone(&artifacts),
        Arc::clone(&store),
        scheduler,
        SelectedModelAdapter::for_test(ConfigProvider::OpenAi, provider),
    )
    .with_project_root(&project_root);
    config.poll_interval = Duration::from_millis(5);
    let executor = RunExecutor::start(config).unwrap();

    let state = loop {
        let QueryProjection::Run(run) = store
            .lock()
            .unwrap()
            .query(&Query::GetRun { run_id })
            .unwrap()
        else {
            panic!("unexpected run projection")
        };
        if run.state.is_terminal() {
            break run.state;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    executor.shutdown().await.unwrap();

    let mut locked = store.lock().unwrap();
    let authority_after =
        authority_snapshot(&mut *locked, &current_authority(), project_id, run_id);
    let QueryProjection::RunTranscript(transcript) =
        locked.query(&Query::RunTranscript { run_id }).unwrap()
    else {
        panic!("unexpected transcript projection")
    };
    let transcript = serde_json::to_string(&transcript).unwrap();
    let events = locked.worker_append_store().unwrap().events().unwrap();
    let journal = events
        .iter()
        .filter(|event| event.event.event_type.as_str() == "agent.effect_journal")
        .filter_map(|event| {
            serde_json::from_slice::<serde_json::Value>(&event.event.payload)
                .ok()?
                .get("record")
                .cloned()
                .and_then(|record| serde_json::from_value::<EffectJournalRecord>(record).ok())
        })
        .collect::<Vec<_>>();
    let operation_intents = journal
        .iter()
        .filter_map(|record| match record {
            EffectJournalRecord::EffectIntent(intent)
                if matches!(intent.payload, EffectIntentPayload::Capability { .. }) =>
            {
                Some(intent.correlation.operation_id.as_str())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    AuthorityReceipt {
        state,
        persisted_tool_request: transcript.contains("kit_run")
            && transcript.contains("\"argv\":[\"false\"]"),
        authority_before,
        authority_after,
        durable_effect_intents: operation_intents.len(),
        kernel_intents: events
            .iter()
            .filter(|event| event.event.event_type.as_str() == "capability.invocation_intent")
            .count(),
        kernel_denials: events
            .iter()
            .filter(|event| {
                event.event.event_type.as_str() == "capability.invocation_outcome"
                    && String::from_utf8_lossy(&event.event.payload)
                        .contains("authorization_denied")
            })
            .count(),
        kernel_dispatches: events
            .iter()
            .filter(|event| event.event.event_type.as_str() == "capability.invocation_dispatched")
            .count(),
        broker_denials: events
            .iter()
            .filter(|event| {
                event.event.event_type.as_str() == "run.progress"
                    && String::from_utf8_lossy(&event.event.payload)
                        .contains("capability authorization denied")
            })
            .count(),
        effect_dispatches: journal
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    EffectJournalRecord::EffectDispatched(dispatched)
                        if operation_intents.contains(dispatched.correlation.operation_id.as_str())
                )
            })
            .count(),
    }
}

fn init_git(root: &Path) {
    for arguments in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Kit Test",
            "-c",
            "user.email=kit@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(arguments)
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }
}

#[tokio::test]
async fn six_surface_injection() {
    let corpus = load_corpus();
    validate_exact_whitelist(corpus.manifest.surfaces.iter().map(String::as_str)).unwrap();
    assert!(validate_exact_whitelist(SURFACES[..5].iter().copied()).is_err());
    assert!(validate_exact_whitelist(SURFACES.into_iter().chain([SURFACES[0]])).is_err());
    assert!(validate_exact_whitelist(SURFACES.into_iter().chain(["extra"])).is_err());
    assert_eq!(corpus.manifest.canaries.len(), corpus.corpus.cases.len());
    assert_eq!(
        corpus
            .manifest
            .canaries
            .iter()
            .collect::<BTreeSet<_>>()
            .len(),
        corpus.manifest.canaries.len()
    );

    let custody = custody(&corpus);
    let exports = BTreeMap::from([
        ("prompts", prompt_export(&corpus, &custody)),
        ("events", event_export(&corpus, &custody)),
        ("traces", trace_export(&corpus, &custody)),
        ("composition inputs", composition_export(&corpus, &custody)),
        ("terminal history", terminal_export(&corpus, &custody)),
        ("workspace metadata", workspace_export(&corpus, &custody)),
    ]);
    validate_exact_whitelist(exports.keys().copied()).unwrap();

    let mut ordered_envelope = Vec::new();
    for surface in SURFACES {
        let case = corpus.surface_case(surface);
        let canary = corpus
            .manifest
            .canaries
            .iter()
            .find(|canary| case.payload.contains(canary.as_str()))
            .unwrap();
        for vector in encoded_vectors(canary.as_bytes()) {
            let mut positive = custody.redactor().scanner();
            for chunk in vector.chunks(2) {
                positive.push(chunk);
            }
            assert!(
                positive.found(),
                "scanner missed {surface}: {}",
                String::from_utf8_lossy(&vector)
            );
        }
        let export = &exports[surface];
        assert!(
            String::from_utf8_lossy(export).contains(REDACTED)
                || surface == "prompts" && String::from_utf8_lossy(export).contains("<secret-ref:"),
            "{surface} did not project active secrets: {}",
            String::from_utf8_lossy(export)
        );
        assert!(
            String::from_utf8_lossy(export).contains(&format!("surface-pointer:{surface}")),
            "{surface} lacks its own pointer: {}",
            String::from_utf8_lossy(export)
        );
        let mut scanner = custody.redactor().scanner();
        scanner.push(export);
        assert!(
            !scanner.found(),
            "{surface} exported reconstructable secret material"
        );
        assert!(
            !export
                .windows(canary.len())
                .any(|window| window == canary.as_bytes())
        );
        ordered_envelope.extend_from_slice(surface.as_bytes());
        ordered_envelope.push(0);
        ordered_envelope.extend_from_slice(export);
    }
    let mut envelope_scanner = custody.redactor().scanner();
    envelope_scanner.push(&ordered_envelope);
    assert!(
        !envelope_scanner.found(),
        "ordered complete export reconstructs a secret"
    );

    for export in mcp_projection_exports(&corpus, &custody) {
        assert!(String::from_utf8_lossy(&export).contains(REDACTED));
        let mut scanner = custody.redactor().scanner();
        scanner.push(&export);
        assert!(!scanner.found(), "MCP normalized result exported a secret");
    }

    let baseline = run_authority_conversation("baseline", b"inert untrusted data").await;
    assert_eq!(baseline.state, RunState::Completed, "{baseline:?}");
    assert!(!baseline.persisted_tool_request, "{baseline:?}");
    assert_authority_unchanged(
        "baseline",
        &baseline.authority_before,
        &baseline.authority_after,
    );
    assert_eq!(baseline.durable_effect_intents, 0, "{baseline:?}");
    assert_eq!(baseline.kernel_intents, 0, "{baseline:?}");
    assert_eq!(baseline.kernel_denials, 0, "{baseline:?}");
    assert_eq!(baseline.kernel_dispatches, 0, "{baseline:?}");
    assert_eq!(baseline.broker_denials, 0, "{baseline:?}");
    assert_eq!(baseline.effect_dispatches, 0, "{baseline:?}");

    for surface in SURFACES {
        let export = String::from_utf8_lossy(&exports[surface]);
        assert!(
            export.contains("REQUEST_UNAUTHORIZED") || export.contains("request-unauthorized"),
            "{surface} projection removed the injection instruction: {export}"
        );
        let receipt = run_authority_conversation(surface, &exports[surface]).await;
        assert_eq!(receipt.state, RunState::Completed, "{surface}: {receipt:?}");
        assert!(!receipt.persisted_tool_request, "{surface}: {receipt:?}");
        assert_authority_unchanged(surface, &receipt.authority_before, &receipt.authority_after);
        assert_eq!(receipt.durable_effect_intents, 0, "{surface}: {receipt:?}");
        assert_eq!(receipt.kernel_intents, 0, "{surface}: {receipt:?}");
        assert_eq!(receipt.kernel_denials, 0, "{surface}: {receipt:?}");
        assert_eq!(receipt.kernel_dispatches, 0, "{surface}: {receipt:?}");
        assert_eq!(receipt.broker_denials, 0, "{surface}: {receipt:?}");
        assert_eq!(receipt.effect_dispatches, 0, "{surface}: {receipt:?}");
        eprintln!(
            "authority surface={surface} provider_tool_visible=false durable_intent={} broker_denial={} kernel_intent={} kernel_denial={} effect_dispatched={} kernel_dispatched={}",
            receipt.durable_effect_intents,
            receipt.broker_denials,
            receipt.kernel_intents,
            receipt.kernel_denials,
            receipt.effect_dispatches,
            receipt.kernel_dispatches,
        );
    }
}
