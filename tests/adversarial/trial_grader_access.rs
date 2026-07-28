use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use kit::domain::{
    ids::{AttemptId, DaemonServiceId, PrincipalId, ProcessId, ProjectId, RunId},
    lifecycle::{AttemptOwnership, FencingToken, ProcessClaim, ProcessOwnership},
};
#[cfg(not(target_os = "linux"))]
use kit::executor::backends::container::limits::NotAvailableReason;
use kit::executor::process::own::{
    ProcessRecord, ProcessRegistrationContext, ProcessRegistry, ProcessRegistryRegistration,
};
use kit::executor::trial::{
    AccessDecision, AgentAccess, AgentAuthority, AgentMaterial, BoundaryOutcome, ExecutionRoute,
    FreshTrialIdentity, GraderInputs, GraderTestChannel, GraderTestEncoding, GraderTestProbe,
    ImmutableTrialManifest, ProductionTrialRequest, TrialAllocation, TrialError, TrialPhase,
    TrialUsageReceipt, TrustedInput, TrustedInputSource, orchestrate_conformance,
};
use kit::runtime::scheduler::{
    AdmissionKind, DurableScheduler, ReservationRequest, limits::Spend, reserve::ReservationId,
};
use kit::store::sqlite::trial_usage::SqliteTrialUsageReceiptStore;
use kit::workspace::acquire::{
    AcquisitionMode, AcquisitionRequest, AcquisitionResult, OwnerId, WorkspaceId, WriterPolicy,
    acquire,
};

#[path = "../../eval/harness/core/mod.rs"]
mod harness_core;

fn manifest() -> ImmutableTrialManifest {
    ImmutableTrialManifest::from_phase0_bytes(include_bytes!(
        "../../eval/manifests/examples/trial.json"
    ))
    .expect("Phase 0 example must remain a valid immutable trial manifest")
}

struct ProductionFixture {
    root: PathBuf,
    workspace: AcquisitionResult,
    record_root: PathBuf,
    usage_database: PathBuf,
    usage_run_id: RunId,
    usage_owner: AttemptOwnership,
    usage_scheduler: DurableScheduler,
    usage_receipt: Option<TrialUsageReceipt>,
    usage_receipts: SqliteTrialUsageReceiptStore,
}

impl ProductionFixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kit-trial-adversarial-{}-{}",
            std::process::id(),
            random_hex()
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let source = root.join("source");
        let managed = root.join("managed");
        let record_root = root.join("records");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&managed).unwrap();
        fs::create_dir(&record_root).unwrap();
        let usage_database = root.join("usage.sqlite");
        let (usage_run_id, usage_owner, usage_scheduler, usage_receipts) =
            deterministic_usage_run(&usage_database);
        git(&source, ["init", "--quiet"]);
        git(&source, ["config", "user.name", "Kit Test"]);
        git(&source, ["config", "user.email", "kit@example.invalid"]);
        fs::write(source.join("README"), b"fixture").unwrap();
        git(&source, ["add", "README"]);
        git(&source, ["commit", "--quiet", "-m", "fixture"]);
        let workspace = acquire(AcquisitionRequest::new(
            &source,
            &managed,
            WorkspaceId::new(format!("trial-{}", random_hex())).unwrap(),
            OwnerId::new("trial-test").unwrap(),
            AcquisitionMode::LocalClone,
            WriterPolicy::Restricted,
        ))
        .unwrap();
        Self {
            root,
            workspace,
            record_root: fs::canonicalize(record_root).unwrap(),
            usage_database,
            usage_run_id,
            usage_owner,
            usage_scheduler,
            usage_receipt: None,
            usage_receipts,
        }
    }

    fn bind_usage(&mut self, manifest: &ImmutableTrialManifest) {
        self.usage_scheduler
            .admit_trial_run(
                self.usage_run_id,
                &manifest.trial_run_binding(self.usage_owner).unwrap(),
            )
            .unwrap();
        record_deterministic_usage(
            &self.usage_database,
            &self.usage_scheduler,
            self.usage_run_id,
            self.usage_owner,
        );
        self.usage_scheduler
            .finish_run(self.usage_run_id, false)
            .unwrap();
        self.usage_receipt = Some(
            self.usage_receipts
                .mint(self.usage_run_id, manifest.trial_id())
                .unwrap(),
        );
    }
}

fn deterministic_usage_run(
    path: &Path,
) -> (
    RunId,
    AttemptOwnership,
    DurableScheduler,
    SqliteTrialUsageReceiptStore,
) {
    let run_id = RunId::parse("run_00000000000000000000000001").unwrap();
    let principal = PrincipalId::parse("principal_00000000000000000000000001").unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
        principal,
        FencingToken::new(1),
    );
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE store_clock (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1), unix_micros INTEGER NOT NULL
             );
             INSERT INTO store_clock VALUES (1, 0);
             CREATE TABLE commit_watermark (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1), position INTEGER NOT NULL
             );
             INSERT INTO commit_watermark VALUES (1, 3);
             CREATE TABLE events (
                 commit_position INTEGER PRIMARY KEY, event_type TEXT NOT NULL,
                 correlation_id TEXT NOT NULL, attempt_id TEXT, payload BLOB NOT NULL
             );",
        )
        .unwrap();
    drop(connection);
    let scheduler = DurableScheduler::open(path).unwrap();
    scheduler
        .register_run(run_id, principal, "production-core-provider")
        .unwrap();
    rusqlite::Connection::open(path)
        .unwrap()
        .execute(
            "UPDATE scheduler_runs SET config_digest = ?1 WHERE run_id = ?2",
            rusqlite::params![
                "2a2e56b7fd4a32f2a3e91c0c67e62c604428388285ca7f8c56c6c52db76c68cb",
                run_id.to_string()
            ],
        )
        .unwrap();
    let receipts = SqliteTrialUsageReceiptStore::open(path).unwrap();
    (run_id, owner, scheduler, receipts)
}

fn record_deterministic_usage(
    path: &Path,
    scheduler: &DurableScheduler,
    run_id: RunId,
    owner: AttemptOwnership,
) {
    let reservation_id = ReservationId::new(0x00112233445566778899aabbccddeeff);
    scheduler
        .reserve(&ReservationRequest {
            id: reservation_id,
            run_id,
            principal_id: owner.principal_id,
            attempt: Some(owner),
            idempotency_key: "production-core-model-call".to_owned(),
            kind: AdmissionKind::Model,
            spend: Spend::new(6, 9, 1, 0, 0),
        })
        .unwrap();
    scheduler.mark_dispatched(reservation_id).unwrap();
    scheduler.debit(reservation_id).unwrap();
    scheduler
        .reconcile(reservation_id, Spend::new(6, 9, 1, 0, 0))
        .unwrap();
    let reservation = format!("{:032x}", reservation_id.get());
    let common = serde_json::json!({
        "schema_version": 1,
        "model_call_id": "model_call_00000000000000000000000001",
        "attempt_id": owner.attempt_id,
        "attempt_fence": owner.fencing_token.get(),
        "reservation_id": reservation,
    });
    let mut intent = common.clone();
    intent["provider"] = "deterministic-test".into();
    intent["model"] = "fake-deterministic-v1".into();
    intent["model_snapshot_digest"] = format!("sha256:{}", "3".repeat(64)).into();
    intent["config_snapshot_digest"] =
        "sha256:2a2e56b7fd4a32f2a3e91c0c67e62c604428388285ca7f8c56c6c52db76c68cb".into();
    let mut outcome = common.clone();
    outcome["status"] = "succeeded".into();
    outcome["charged"] = true.into();
    outcome["provider_request_id"] = "deterministic-provider-request-1".into();
    outcome["usage"] = serde_json::json!({
        "tokens": {
            "input_tokens": 4,
            "output_tokens": 2,
            "reasoning_tokens": 3,
            "cached_input_tokens": 0,
            "cache_write_input_tokens": 0
        },
        "cost": {"amount": 0.000006, "currency": "USD", "provider_amount": "0.000006"},
        "metadata": {}
    });
    let connection = rusqlite::Connection::open(path).unwrap();
    for (position, event_type, payload) in [
        (1, "model_call.intent", intent),
        (2, "model_call.dispatched", common),
        (3, "model_call.outcome", outcome),
    ] {
        connection
            .execute(
                "INSERT INTO events VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    position,
                    event_type,
                    run_id.to_string(),
                    owner.attempt_id.to_string(),
                    serde_json::to_vec(&payload).unwrap()
                ],
            )
            .unwrap();
    }
}

impl Drop for ProductionFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git<const N: usize>(directory: &Path, arguments: [&str; N]) {
    assert!(
        Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .status()
            .unwrap()
            .success()
    );
}

fn random_hex() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).unwrap();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

const EMPTY_SHA256: &str =
    "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const EMPTY_HIDDEN_MANIFEST: &[u8] =
    b"{\"schema_version\":1,\"checks\":[],\"canaries\":[\"HIDDEN-CANARY-EMPTY-0a11\"]}";
const EMPTY_HIDDEN_MANIFEST_SHA256: &str =
    "sha256:79e8f72624d6dd1b0fa41ac8d98dcc6584e7bcffbbff09388bedcaaf1c3195af";
const CANARY_HIDDEN_MANIFEST: &[u8] =
    b"{\"schema_version\":1,\"checks\":[],\"canaries\":[\"HIDDEN-CANARY-PROTOCOL-7c21\"]}";
const CANARY_HIDDEN_MANIFEST_SHA256: &str =
    "sha256:0a68bcc7a2482ae66cd009fe290197908284515c52e4da070b95e44164b981db";

fn production_manifest(
    fixture: &ProductionFixture,
    agent_image: &str,
    grader_image: &str,
) -> ImmutableTrialManifest {
    production_manifest_with_hidden_digest(
        fixture,
        agent_image,
        grader_image,
        EMPTY_HIDDEN_MANIFEST_SHA256,
    )
}

fn production_manifest_with_hidden_digest(
    fixture: &ProductionFixture,
    agent_image: &str,
    grader_image: &str,
    hidden_tests_digest: &str,
) -> ImmutableTrialManifest {
    let mut value: serde_json::Value =
        serde_json::from_slice(include_bytes!("../../eval/manifests/examples/trial.json")).unwrap();
    value["task"]["repository"]["commit"] = fixture.workspace.base_commit.clone().into();
    value["task"]["specification_digest"] = EMPTY_SHA256.into();
    value["task"]["scaffold_digest"] = EMPTY_SHA256.into();
    value["environment"]["image_digest"] = agent_image.into();
    value["environment"]["architecture"] = if cfg!(target_arch = "aarch64") {
        "aarch64".into()
    } else {
        "x86_64".into()
    };
    value["grader"]["image_digest"] = grader_image.into();
    value["grader"]["hidden_tests_digest"] = hidden_tests_digest.into();
    value["grader"]["acceptance_digest"] = EMPTY_SHA256.into();
    ImmutableTrialManifest::from_phase0_bytes(&serde_json::to_vec(&value).unwrap()).unwrap()
}

fn empty_grader_inputs() -> GraderInputs<'static> {
    let input = TrustedInput {
        source: TrustedInputSource::Bytes(b""),
        expected_sha256: EMPTY_SHA256,
    };
    GraderInputs {
        specification: input,
        scaffold: input,
        hidden_tests: TrustedInput {
            source: TrustedInputSource::Bytes(EMPTY_HIDDEN_MANIFEST),
            expected_sha256: EMPTY_HIDDEN_MANIFEST_SHA256,
        },
        gold_patch: input,
        acceptance_rules: input,
        harness_config: input,
        harness_commit: "89abcdef0123456789abcdef0123456789abcdef",
    }
}

fn production_request<'a>(
    fixture: &'a ProductionFixture,
    manifest: &'a ImmutableTrialManifest,
    command: &'a [OsString],
) -> ProductionTrialRequest<'a> {
    struct Registry;
    impl ProcessRegistry for Registry {
        fn prepared(
            &self,
            _: ProcessRegistrationContext,
            _: ProcessClaim,
            _: &kit::executor::process::tree::PersistedBoundary,
            _: kit::executor::process::own::ProcessTerminalConfig,
        ) -> std::io::Result<()> {
            Ok(())
        }
        fn started(&self, _: ProcessRegistrationContext, _: &ProcessRecord) -> std::io::Result<()> {
            Ok(())
        }
        fn exited(&self, _: ProcessRegistrationContext, _: &ProcessRecord) -> std::io::Result<()> {
            Ok(())
        }
        fn outcome_unknown(
            &self,
            _: ProcessRegistrationContext,
            _: ProcessId,
        ) -> std::io::Result<()> {
            Ok(())
        }
    }
    ProductionTrialRequest {
        manifest,
        workspace: &fixture.workspace,
        record_root: &fixture.record_root,
        owner: ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap()),
        process_registry: ProcessRegistryRegistration::new(
            Arc::new(Registry),
            ProcessRegistrationContext {
                project_id: ProjectId::parse("project_00000000000000000000000001").unwrap(),
                principal_id: PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
            },
        ),
        cancellation: None,
        agent_command: command,
        grader_inputs: empty_grader_inputs(),
        usage_receipt: fixture.usage_receipt.as_ref().unwrap(),
        usage_receipts: &fixture.usage_receipts,
        grader_resource_bounds: None,
        grader_test_probe: None,
    }
}

fn production_request_with_hidden<'a>(
    fixture: &'a ProductionFixture,
    manifest: &'a ImmutableTrialManifest,
    command: &'a [OsString],
    hidden_tests: &'a [u8],
    hidden_tests_digest: &'a str,
) -> ProductionTrialRequest<'a> {
    let mut request = production_request(fixture, manifest, command);
    request.grader_inputs.hidden_tests = TrustedInput {
        source: TrustedInputSource::Bytes(hidden_tests),
        expected_sha256: hidden_tests_digest,
    };
    request
}

#[test]
fn agent_cannot_read_or_write_any_grader_material() {
    let authority = AgentAuthority;
    for material in [
        AgentMaterial::Grader,
        AgentMaterial::GoldPatch,
        AgentMaterial::HiddenAcceptanceRules,
    ] {
        for access in [AgentAccess::Read, AgentAccess::Write] {
            assert_eq!(
                authority.authorize(material, access),
                AccessDecision::Denied
            );
        }
    }
    assert_eq!(
        authority.authorize(AgentMaterial::Source, AgentAccess::Read),
        AccessDecision::Allowed
    );
    assert_eq!(
        authority.authorize(AgentMaterial::Source, AgentAccess::Write),
        AccessDecision::Denied
    );
    assert_eq!(
        authority.authorize(AgentMaterial::TaskInput, AgentAccess::Write),
        AccessDecision::Denied
    );
}

#[test]
fn semantics_only_fake_models_distinct_trial_identities() {
    let manifest = manifest();
    let mut executor = harness_core::trial_runner::SemanticsOnlyTrialExecutor::default();
    let mut instances = BTreeSet::new();
    let mut rootfs_layers = BTreeSet::new();
    let mut writable_layers = BTreeSet::new();
    let mut previous_writable = None;

    for sequence in 0_u64..1000 {
        let identity = FreshTrialIdentity::for_conformance(sequence);
        if let Some(previous) = previous_writable.as_deref() {
            assert!(executor.marker_present(previous));
            assert_ne!(previous, identity.writable_layer_id);
            assert!(!executor.marker_present(&identity.writable_layer_id));
        }
        let result = orchestrate_conformance(&manifest, &identity, &mut executor).unwrap();
        assert_eq!(result.agent.image_digest, manifest.agent_image_digest());
        assert_eq!(result.grader.image_digest, manifest.grader_image_digest());
        assert!(instances.insert(result.agent.instance_id));
        assert!(rootfs_layers.insert(result.agent.rootfs_layer_id));
        previous_writable = Some(result.agent.writable_layer_id.clone());
        assert!(writable_layers.insert(result.agent.writable_layer_id));
    }

    assert_eq!(executor.instance_count(), 1000);
    assert_eq!(instances.len(), 1000);
    assert_eq!(rootfs_layers.len(), 1000);
    assert_eq!(writable_layers.len(), 1000);
}

#[test]
fn reported_image_must_exactly_match_the_manifest_pin() {
    let manifest = manifest();
    let identity = FreshTrialIdentity::for_conformance(1);
    let mut executor = harness_core::trial_runner::SemanticsOnlyTrialExecutor::default();
    executor.report_image_once(format!("sha256:{}", "f".repeat(64)));
    assert!(matches!(
        orchestrate_conformance(&manifest, &identity, &mut executor),
        Err(TrialError::BoundaryIdentityMismatch(_))
    ));
}

#[test]
fn agent_and_grader_exit_or_signal_outcomes_fail_the_trial() {
    let manifest = manifest();
    for (phase, outcome) in [
        (TrialPhase::Agent, BoundaryOutcome::Exit(0)),
        (TrialPhase::Agent, BoundaryOutcome::Signal(9)),
        (TrialPhase::Grader, BoundaryOutcome::Exit(1)),
        (TrialPhase::Grader, BoundaryOutcome::Signal(15)),
    ] {
        let identity = FreshTrialIdentity::for_conformance(1);
        let mut executor = harness_core::trial_runner::SemanticsOnlyTrialExecutor::default();
        executor.report_outcome_once(phase, outcome);
        assert!(matches!(
            orchestrate_conformance(&manifest, &identity, &mut executor),
            Err(TrialError::BoundaryFailed(actual_phase, actual_outcome))
                if actual_phase == phase && actual_outcome == outcome
        ));
    }
}

#[test]
fn full_manifest_digest_changes_when_a_component_pin_changes() {
    let original = include_bytes!("../../eval/manifests/examples/trial.json");
    let first = ImmutableTrialManifest::from_phase0_bytes(original).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(original).unwrap();
    value["environment"]["components"]["prompt_digest"] =
        format!("sha256:{}", "c".repeat(64)).into();
    let changed =
        ImmutableTrialManifest::from_phase0_bytes(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(first.identity_digest(), changed.identity_digest());
    assert_ne!(
        first.manifest_bytes_digest(),
        changed.manifest_bytes_digest()
    );
}

#[test]
fn phase0_loader_rejects_missing_required_grader_pins() {
    let original = include_bytes!("../../eval/manifests/examples/trial.json");
    for field in ["gold_patch_digest", "harness_config_digest"] {
        let mut value: serde_json::Value = serde_json::from_slice(original).unwrap();
        value["grader"].as_object_mut().unwrap().remove(field);
        assert!(matches!(
            ImmutableTrialManifest::from_phase0_bytes(&serde_json::to_vec(&value).unwrap()),
            Err(TrialError::Manifest(_))
        ));
    }
}

#[test]
#[cfg(not(target_os = "linux"))]
fn unsupported_host_is_typed_unavailable_before_trial_allocation() {
    let mut fixture = ProductionFixture::new();
    let manifest = production_manifest(
        &fixture,
        &format!("sha256:{}", "2".repeat(64)),
        &format!("sha256:{}", "9".repeat(64)),
    );
    fixture.bind_usage(&manifest);
    let command = [OsString::from("/bin/true")];
    assert!(matches!(
        harness_core::trial_runner::execute_trial(production_request(
            &fixture, &manifest, &command,
        )),
        Err(TrialError::Unavailable(ref unavailable))
            if unavailable.reason == NotAvailableReason::UnsupportedHost
    ));
}

#[cfg(unix)]
#[test]
fn one_thousand_filesystem_allocations_are_reserved_and_fresh() {
    let fixture = ProductionFixture::new();
    let mut identities = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut previous: Option<TrialAllocation> = None;
    for _ in 0..1000 {
        let allocation = TrialAllocation::allocate(&fixture.workspace).unwrap();
        assert!(identities.insert((
            allocation.identity().instance_id.clone(),
            allocation.identity().rootfs_layer_id.clone(),
            allocation.identity().writable_layer_id.clone(),
        )));
        assert!(identities.iter().all(|identity| !identity.0.is_empty()));
        assert!(
            allocation
                .writable_path()
                .join("freshness-marker")
                .try_exists()
                .is_ok_and(|v| !v)
        );
        for path in allocation.reserved_paths() {
            assert!(paths.insert(path.to_owned()));
            assert!(path.join(".kit-trial-owner").is_file());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
        assert!(allocation.marker_identity().starts_with("kit-trial-v1:"));
        fs::write(
            allocation.writable_path().join("freshness-marker"),
            b"owned",
        )
        .unwrap();
        if let Some(previous) = previous.replace(allocation) {
            assert_eq!(
                fs::read(previous.writable_path().join("freshness-marker")).unwrap(),
                b"owned"
            );
            previous.cleanup().unwrap();
        }
    }
    previous.unwrap().cleanup().unwrap();
    assert_eq!(identities.len(), 1000);
    assert_eq!(paths.len(), 6000);
}

#[cfg(unix)]
#[test]
fn cleanup_refuses_a_symlink_replacement_without_removing_its_target() {
    use std::os::unix::fs::symlink;
    let fixture = ProductionFixture::new();
    let allocation = TrialAllocation::allocate(&fixture.workspace).unwrap();
    let writable = allocation.writable_path().to_owned();
    let original = writable.with_extension("original");
    let target = fixture.root.join("must-survive");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("sentinel"), b"safe").unwrap();
    fs::rename(&writable, &original).unwrap();
    symlink(&target, &writable).unwrap();
    assert!(matches!(allocation.cleanup(), Err(TrialError::Cleanup(_))));
    assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"safe");
    assert!(writable.is_symlink());
}

#[cfg(unix)]
#[test]
fn cleanup_refuses_an_unowned_directory_replacement() {
    let fixture = ProductionFixture::new();
    let allocation = TrialAllocation::allocate(&fixture.workspace).unwrap();
    let writable = allocation.writable_path().to_owned();
    let original = writable.with_extension("original");
    fs::rename(&writable, &original).unwrap();
    fs::create_dir(&writable).unwrap();
    fs::write(writable.join("sentinel"), b"replacement").unwrap();
    assert!(matches!(allocation.cleanup(), Err(TrialError::Cleanup(_))));
    assert_eq!(fs::read(writable.join("sentinel")).unwrap(), b"replacement");
}

#[cfg(unix)]
#[test]
fn cleanup_permission_failure_is_returned() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = ProductionFixture::new();
    let allocation = TrialAllocation::allocate(&fixture.workspace).unwrap();
    let parent = allocation.writable_path().parent().unwrap().to_owned();
    let original = fs::metadata(&parent).unwrap().permissions();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
    let result = allocation.cleanup();
    fs::set_permissions(&parent, original).unwrap();
    assert!(matches!(result, Err(TrialError::Cleanup(_))));
}

#[test]
#[ignore = "requires KIT_TRIAL_AGENT_IMAGE, KIT_TRIAL_GRADER_IMAGE, and the trusted helper"]
fn production_helper_denies_agent_grader_input_reads_and_writes() {
    let agent_image = std::env::var("KIT_TRIAL_AGENT_IMAGE")
        .expect("KIT_TRIAL_AGENT_IMAGE is required when this ignored test is requested");
    let grader_image = std::env::var("KIT_TRIAL_GRADER_IMAGE")
        .expect("KIT_TRIAL_GRADER_IMAGE is required when this ignored test is requested");
    let mut fixture = ProductionFixture::new();
    let manifest = production_manifest(&fixture, &agent_image, &grader_image);
    fixture.bind_usage(&manifest);
    let command = [
        OsString::from("/bin/sh"),
        OsString::from("-eu"),
        OsString::from("-c"),
        OsString::from(
            "test ! -e /kit-trusted-input; for material in hidden-tests gold-patch acceptance-rules harness-config; do ! cat /kit-trusted-input/$material; ! touch /kit-trusted-input/$material; done; printf '%s' '{\"nonce\":\"agent-forged\"}' > /build/grader-test-manifest.json",
        ),
    ];
    harness_core::trial_runner::execute_trial(production_request(&fixture, &manifest, &command))
        .unwrap();
}

#[test]
#[ignore = "requires KIT_TRIAL_AGENT_IMAGE, KIT_TRIAL_GRADER_IMAGE, and the trusted helper"]
fn production_protocol_rejects_all_canary_encodings_before_persistence() {
    let agent_image = std::env::var("KIT_TRIAL_AGENT_IMAGE").unwrap();
    let grader_image = std::env::var("KIT_TRIAL_GRADER_IMAGE").unwrap();
    let mut fixture = ProductionFixture::new();
    let mut value: serde_json::Value =
        serde_json::from_slice(include_bytes!("../../eval/manifests/examples/trial.json")).unwrap();
    value["task"]["repository"]["commit"] = fixture.workspace.base_commit.clone().into();
    value["task"]["specification_digest"] = EMPTY_SHA256.into();
    value["task"]["scaffold_digest"] = EMPTY_SHA256.into();
    value["environment"]["image_digest"] = agent_image.into();
    value["environment"]["architecture"] = if cfg!(target_arch = "aarch64") {
        "aarch64".into()
    } else {
        "x86_64".into()
    };
    value["grader"]["image_digest"] = grader_image.into();
    value["grader"]["hidden_tests_digest"] = CANARY_HIDDEN_MANIFEST_SHA256.into();
    value["grader"]["acceptance_digest"] = EMPTY_SHA256.into();
    let manifest =
        ImmutableTrialManifest::from_phase0_bytes(&serde_json::to_vec(&value).unwrap()).unwrap();
    fixture.bind_usage(&manifest);
    let command = [
        OsString::from("/bin/sh"),
        OsString::from("-eu"),
        OsString::from("-c"),
        OsString::from(
            r"printf '%s' 'HIDDEN-CANARY-PROTOCOL-7c21' > /build/plain; \
             printf '%s' '%48%49%44%44%45%4E%2D%43%41%4E%41%52%59%2D%50%52%4F%54%4F%43%4F%4C%2D%37%63%32%31' > /build/url; \
             printf '%s' 'SElEREVOLUNBTkFSWS1QUk9UT0NPTC03YzIx' > /build/base64; \
             printf '%s' 'HIDDEN-CANARY-' > /build/split; printf '%s' 'PROTOCOL-7c21' >> /build/split; \
             printf '\000HIDDEN-CANARY-PROTOCOL-7c21\377' > /build/binary",
        ),
    ];
    let result = harness_core::trial_runner::execute_trial(production_request_with_hidden(
        &fixture,
        &manifest,
        &command,
        CANARY_HIDDEN_MANIFEST,
        CANARY_HIDDEN_MANIFEST_SHA256,
    ));
    assert!(matches!(result, Err(TrialError::SensitiveArtifact)));
    assert_eq!(fs::read_dir(&fixture.record_root).unwrap().count(), 0);
    let allocation_parent = fixture.workspace.path.parent().unwrap();
    assert!(fs::read_dir(allocation_parent).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("trial-")
    }));
}

#[test]
#[ignore = "requires pinned trial images, trusted helper, and persistent external evidence root"]
fn production_grader_probe_rejects_every_channel_and_encoding_before_persistence() {
    let agent_image = std::env::var("KIT_TRIAL_AGENT_IMAGE").unwrap();
    let grader_image = std::env::var("KIT_TRIAL_GRADER_IMAGE").unwrap();
    let evidence_root = PathBuf::from(std::env::var_os("KIT_CORE_EVIDENCE_ROOT").unwrap());
    fs::create_dir_all(&evidence_root).unwrap();
    let mut fixture = ProductionFixture::new();
    let manifest = production_manifest_with_hidden_digest(
        &fixture,
        &agent_image,
        &grader_image,
        CANARY_HIDDEN_MANIFEST_SHA256,
    );
    fixture.bind_usage(&manifest);
    let command = [OsString::from("/bin/true")];
    let channels = [
        GraderTestChannel::GraderLog,
        GraderTestChannel::CanonicalReport,
        GraderTestChannel::Checks,
        GraderTestChannel::FinalTree,
        GraderTestChannel::ExtraArtifact,
    ];
    let encodings = [
        GraderTestEncoding::Raw,
        GraderTestEncoding::Percent,
        GraderTestEncoding::Base64,
        GraderTestEncoding::Split,
        GraderTestEncoding::Binary,
    ];
    for channel in channels {
        for encoding in encodings {
            let mut request = production_request_with_hidden(
                &fixture,
                &manifest,
                &command,
                CANARY_HIDDEN_MANIFEST,
                CANARY_HIDDEN_MANIFEST_SHA256,
            );
            request.grader_test_probe = Some(GraderTestProbe::new(channel, encoding));
            assert!(
                matches!(
                    harness_core::trial_runner::execute_trial(request),
                    Err(TrialError::SensitiveArtifact)
                ),
                "grader probe did not fail closed for {channel:?}/{encoding:?}"
            );
            assert_eq!(fs::read_dir(&fixture.record_root).unwrap().count(), 0);
            assert!(
                fs::read_dir(fixture.workspace.path.parent().unwrap())
                    .unwrap()
                    .all(|entry| {
                        let name = entry.unwrap().file_name();
                        let name = name.to_string_lossy();
                        !name.starts_with("trial-") && !name.starts_with(".kit-trial-quarantine")
                    })
            );
        }
    }
    fs::write(
        evidence_root.join("grader-canary-rejection.txt"),
        format!(
            "schema_version=1\nagent_image={agent_image}\ngrader_image={grader_image}\nchannels={}\nencodings={}\nprobes={}\nsensitive_artifact=25\ntrial_rows=0\nartifact_refs=0\nallocation_survivors=0\n",
            channels.len(),
            encodings.len(),
            channels.len() * encodings.len(),
        ),
    )
    .unwrap();
}

#[test]
#[ignore = "requires KIT_TRIAL_AGENT_IMAGE, KIT_TRIAL_GRADER_IMAGE, and the trusted helper"]
fn one_thousand_production_helper_trials_have_fresh_attested_writable_leases() {
    let agent_image = std::env::var("KIT_TRIAL_AGENT_IMAGE")
        .expect("KIT_TRIAL_AGENT_IMAGE is required when this ignored test is requested");
    let grader_image = std::env::var("KIT_TRIAL_GRADER_IMAGE")
        .expect("KIT_TRIAL_GRADER_IMAGE is required when this ignored test is requested");
    let mut fixture = ProductionFixture::new();
    let manifest = production_manifest(&fixture, &agent_image, &grader_image);
    fixture.bind_usage(&manifest);
    let mut instances = BTreeSet::new();
    let mut rootfs_leases = BTreeSet::new();
    let mut writable_leases = BTreeSet::new();
    let mut carryovers = 0;
    for sequence in 0_u32..1000 {
        let previous = sequence.saturating_sub(1);
        let command = [
            OsString::from("/bin/sh"),
            OsString::from("-eu"),
            OsString::from("-c"),
            OsString::from(format!(
                "if test -e /build/freshness-marker-{previous}; then touch /build/carryover-detected; fi; printf '%s\\n' {sequence} > /build/freshness-marker-{sequence}"
            )),
        ];
        let trial = harness_core::trial_runner::execute_trial(production_request(
            &fixture, &manifest, &command,
        ))
        .unwrap();
        let agent = &trial.record.agent;
        let grader = &trial.record.grader;
        assert_eq!(agent.route, ExecutionRoute::TrustedContainerHelper);
        assert_eq!(grader.route, ExecutionRoute::TrustedContainerHelper);
        assert_eq!(agent.image_digest, manifest.agent_image_digest());
        assert_eq!(grader.image_digest, manifest.grader_image_digest());
        assert_eq!(agent.outcome, BoundaryOutcome::Success);
        assert_eq!(grader.outcome, BoundaryOutcome::Success);
        assert!(agent.quiescent && grader.quiescent);
        assert!(!agent.helper_identity.is_empty() && !grader.helper_identity.is_empty());
        assert!(instances.insert(agent.instance_id.clone()));
        assert!(rootfs_leases.insert(agent.rootfs_layer_id.clone()));
        assert!(writable_leases.insert(agent.writable_layer_id.clone()));
        assert!(trial.record.artifacts.iter().any(|artifact| {
            artifact.path == format!("artifacts/agent/freshness-marker-{sequence}")
        }));
        carryovers += trial
            .record
            .artifacts
            .iter()
            .filter(|artifact| artifact.path == "artifacts/agent/carryover-detected")
            .count();
    }
    assert_eq!(instances.len(), 1000);
    assert_eq!(rootfs_leases.len(), 1000);
    assert_eq!(writable_leases.len(), 1000);
    assert_eq!(carryovers, 0);
}

#[test]
#[ignore = "requires pinned trial images, trusted helper, and persistent external evidence root"]
fn production_core_harness_calibrates_and_reproduces_through_trusted_helper() {
    let agent_image = std::env::var("KIT_TRIAL_AGENT_IMAGE").unwrap();
    let grader_image = std::env::var("KIT_TRIAL_GRADER_IMAGE").unwrap();
    let evidence_root = PathBuf::from(std::env::var_os("KIT_CORE_EVIDENCE_ROOT").unwrap());
    fs::create_dir_all(&evidence_root).unwrap();
    let evidence_root = fs::canonicalize(evidence_root).unwrap();
    let mut fixture = ProductionFixture::new();
    let bounds = harness_core::GraderBounds {
        max_patch_bytes: 16 * 1024,
        max_source_bytes: 64 * 1024,
        max_files: 32,
        max_checks: 16,
        max_check_bytes: 16 * 1024,
        max_log_bytes: 4096,
        max_artifact_bytes: 128 * 1024,
        max_memory_bytes: 256 * 1024 * 1024,
        max_time_millis: 30_000,
    };
    let source =
        harness_core::SourceSnapshot::new([("README".to_owned(), b"fixture".to_vec())], &bounds)
            .unwrap();
    let checks = vec![harness_core::Check::Digest {
        id: "readme-is-right".to_owned(),
        path: "README".to_owned(),
        sha256: harness_core::sha256(b"right"),
    }];
    let acceptance_rules = serde_json::to_vec(&checks).unwrap();
    let gold_patch = b"--- a/README\n+++ b/README\n@@ -1 +1 @@\n-fixture\n\\ No newline at end of file\n+right\n\\ No newline at end of file\n".to_vec();
    let hidden_tests = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "canaries": ["HIDDEN-CANARY-README-91ac"],
        "checks": [{
            "kind": "file_digest",
            "id": "hidden-readme-is-right",
            "path": "README",
            "sha256": harness_core::sha256(b"right")
        }]
    }))
    .unwrap();
    let specification = b"Change README from fixture to right.\n".to_vec();
    let scaffold = b"m004-core-scaffold-v1\n".to_vec();
    let toolchain_digest = harness_core::sha256(b"external-pinned-toolchain");
    let config = serde_json::to_vec(&serde_json::json!({
        "schema_version": 2,
        "harness_version": "m004-core-v2",
        "toolchain_digest": toolchain_digest,
        "source_snapshot_digest": source.digest(),
        "hidden_tests_digest": harness_core::sha256(&hidden_tests),
        "acceptance_rules_digest": harness_core::sha256(&acceptance_rules),
        "gold_patch_digest": harness_core::sha256(&gold_patch),
        "bounds": bounds,
        "checks": checks,
    }))
    .unwrap();
    let mut trial: serde_json::Value =
        serde_json::from_slice(include_bytes!("../../eval/manifests/examples/trial.json")).unwrap();
    trial["task"]["repository"]["commit"] = fixture.workspace.base_commit.clone().into();
    trial["task"]["specification_digest"] = harness_core::sha256(&specification).into();
    trial["task"]["scaffold_digest"] = harness_core::sha256(&scaffold).into();
    trial["environment"]["image_digest"] = agent_image.into();
    trial["environment"]["architecture"] = if cfg!(target_arch = "aarch64") {
        "aarch64".into()
    } else {
        "x86_64".into()
    };
    trial["grader"]["image_digest"] = grader_image.into();
    trial["grader"]["hidden_tests_digest"] = harness_core::sha256(&hidden_tests).into();
    trial["grader"]["acceptance_digest"] = harness_core::sha256(&acceptance_rules).into();
    trial["grader"]["gold_patch_digest"] = harness_core::sha256(&gold_patch).into();
    trial["grader"]["harness_config_digest"] = harness_core::sha256(&config).into();
    let manifest_bytes = serde_json::to_vec(&trial).unwrap();
    let harness = harness_core::CoreHarness::load(harness_core::HarnessInputs {
        trial_manifest: manifest_bytes,
        task_manifest: serde_json::to_vec(&trial["task"]).unwrap(),
        grader_manifest: serde_json::to_vec(&trial["grader"]).unwrap(),
        source,
        specification,
        scaffold,
        actual_toolchain_digest: toolchain_digest,
        hidden_tests_handle: harness_core::HiddenHandle::new("hidden-tests").unwrap(),
        hidden_tests,
        gold_patch_handle: harness_core::HiddenHandle::new("gold-patch").unwrap(),
        gold_patch: gold_patch.clone(),
        acceptance_handle: harness_core::HiddenHandle::new("acceptance").unwrap(),
        acceptance_rules,
        harness_config_handle: harness_core::HiddenHandle::new("harness-config").unwrap(),
        harness_config: config,
    })
    .unwrap();
    let manifest =
        ImmutableTrialManifest::from_phase0_bytes(&serde_json::to_vec(&trial).unwrap()).unwrap();
    fixture.bind_usage(&manifest);
    let template_command = [OsString::from("/bin/true")];
    let template = production_request(&fixture, &manifest, &template_command);
    let command = |patch: &[u8]| {
        let octal = patch
            .iter()
            .map(|byte| format!("\\{byte:03o}"))
            .collect::<String>();
        Ok(vec![
            OsString::from("/bin/sh"),
            OsString::from("-eu"),
            OsString::from("-c"),
            OsString::from(format!(
                "cp -R /workspace/. /build/final-tree; printf '%b' '{octal}' > /build/applied.patch; if test -s /build/applied.patch; then git -C /build/final-tree apply /build/applied.patch || :; fi"
            )),
        ])
    };
    let mut executor = harness_core::ProductionCoreTrialExecutor {
        workspace: &fixture.workspace,
        record_root: &evidence_root,
        owner: template.owner,
        process_registry: template.process_registry,
        cancellation: None,
        agent_command: &command,
        usage_receipt: fixture.usage_receipt.as_ref().unwrap(),
        usage_receipts: &fixture.usage_receipts,
    };
    let validation = harness.self_validate(&mut executor).unwrap();
    let first = harness
        .measure(&mut executor, &validation.token, &gold_patch)
        .unwrap();
    let second = harness
        .measure(&mut executor, &validation.token, &gold_patch)
        .unwrap();
    assert_eq!(first.bytes, second.bytes);
    assert_eq!(first.digest, second.digest);
}
