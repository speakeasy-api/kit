#![allow(dead_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use kit::executor::trial::{
    AgentAccess, AgentMaterial, BoundaryCompletion, BoundaryOutcome, BoundaryRequest, DurableTrial,
    ExecutionRoute, FreshTrialIdentity, GraderInputs, GraderResourceBounds, GraderTestChannel,
    GraderTestEncoding, ImmutableTrialManifest, IsolatedTrialContract, ProductionTrialRequest,
    TrialError, TrialPhase, TrialUsage, TrialUsageReceipt, TrialUsageReceiptStore, TrustedInput,
    TrustedInputSource, execute_production_trial, orchestrate_conformance,
};
use serde::{Deserialize, Serialize};

#[path = "../../graders/core/mod.rs"]
pub mod core_grader;

use core_grader::{
    AuthenticatedChannel, Check, CheckEvidence, GradeMetadata, GradeReport, GraderBounds,
    HiddenCheckAggregate, HiddenTestManifest, SourceSnapshot, sha256,
};

#[derive(Debug, Default)]
pub struct SemanticsOnlyTrialExecutor {
    instances: BTreeSet<String>,
    rootfs_layers: BTreeSet<String>,
    writable_layers: BTreeMap<String, BTreeSet<String>>,
    completed_agent: BTreeSet<String>,
    reported_image: Option<String>,
    reported_outcome: Option<(TrialPhase, BoundaryOutcome)>,
}

impl SemanticsOnlyTrialExecutor {
    pub fn report_image_once(&mut self, digest: impl Into<String>) {
        self.reported_image = Some(digest.into());
    }

    pub fn report_outcome_once(&mut self, phase: TrialPhase, outcome: BoundaryOutcome) {
        self.reported_outcome = Some((phase, outcome));
    }

    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }

    pub fn marker_present(&self, writable_layer_id: &str) -> bool {
        self.writable_layers
            .get(writable_layer_id)
            .is_some_and(|files| files.contains("freshness-marker"))
    }
}

impl IsolatedTrialContract for SemanticsOnlyTrialExecutor {
    fn execute(&mut self, request: BoundaryRequest<'_>) -> Result<BoundaryCompletion, TrialError> {
        match request.phase {
            TrialPhase::Agent => {
                if !self.instances.insert(request.identity.instance_id.clone())
                    || !self
                        .rootfs_layers
                        .insert(request.identity.rootfs_layer_id.clone())
                    || self
                        .writable_layers
                        .insert(request.identity.writable_layer_id.clone(), BTreeSet::new())
                        .is_some()
                {
                    return Err(TrialError::Executor(
                        "conformance fake observed reused trial storage".to_owned(),
                    ));
                }
                let authority = request.agent_authority.ok_or_else(|| {
                    TrialError::Executor("agent authority plan missing".to_owned())
                })?;
                for material in [
                    AgentMaterial::Grader,
                    AgentMaterial::GoldPatch,
                    AgentMaterial::HiddenAcceptanceRules,
                ] {
                    for access in [AgentAccess::Read, AgentAccess::Write] {
                        if !matches!(
                            authority.authorize(material, access),
                            kit::executor::trial::AccessDecision::Denied
                        ) {
                            return Err(TrialError::Executor(
                                "conformance fake observed hidden-material access".to_owned(),
                            ));
                        }
                    }
                }
                self.writable_layers
                    .get_mut(&request.identity.writable_layer_id)
                    .expect("layer inserted above")
                    .insert("freshness-marker".to_owned());
                self.completed_agent.insert(request.trial_id.to_owned());
            }
            TrialPhase::Grader => {
                if request.agent_authority.is_some()
                    || !self.completed_agent.contains(request.trial_id)
                {
                    return Err(TrialError::Executor(
                        "grader ran before the agent boundary quiesced".to_owned(),
                    ));
                }
            }
        }
        Ok(BoundaryCompletion {
            phase: request.phase,
            route: ExecutionRoute::ConformanceFake,
            image_digest: self
                .reported_image
                .take()
                .unwrap_or_else(|| request.image_digest.to_owned()),
            boundary_id: format!("semantics-{:?}", request.phase).to_ascii_lowercase(),
            instance_id: request.identity.instance_id.clone(),
            rootfs_layer_id: request.identity.rootfs_layer_id.clone(),
            writable_layer_id: request.identity.writable_layer_id.clone(),
            plan_digest: format!("semantics-plan-{:?}", request.phase).to_ascii_lowercase(),
            invocation_digest: format!("semantics-invocation-{:?}", request.phase)
                .to_ascii_lowercase(),
            runtime_identity: "semantics-runtime".to_owned(),
            helper_identity: "semantics-helper".to_owned(),
            permitted_profile_digest: request.permitted_profile_digest.to_owned(),
            survivor_processes: 0,
            quiescent: true,
            outcome: match self.reported_outcome {
                Some((phase, outcome)) if phase == request.phase => {
                    self.reported_outcome = None;
                    outcome
                }
                _ => BoundaryOutcome::Success,
            },
        })
    }
}

pub fn execute_trial(request: ProductionTrialRequest<'_>) -> Result<DurableTrial, TrialError> {
    execute_production_trial(request)
}

pub fn execute_external_trial(
    request: ProductionTrialRequest<'_>,
) -> Result<DurableTrial, CoreTrialError> {
    execute_production_trial(request).map_err(map_trial_error)
}

#[derive(Clone)]
pub struct HiddenHandle {
    name: String,
}

impl HiddenHandle {
    pub fn new(name: impl Into<String>) -> Result<Self, CoreTrialError> {
        let name = name.into();
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(CoreTrialError::InvalidHiddenHandle);
        }
        Ok(Self { name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Debug for HiddenHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HiddenHandle([redacted])")
    }
}

pub struct HiddenMaterial<'a> {
    pub handle: &'a HiddenHandle,
    pub bytes: &'a [u8],
}

pub struct CoreTrialRequest<'a> {
    pub manifest: &'a ImmutableTrialManifest,
    pub source: &'a SourceSnapshot,
    pub patch: &'a [u8],
    pub specification: &'a [u8],
    pub scaffold: &'a [u8],
    pub checks: &'a [Check],
    pub bounds: &'a GraderBounds,
    pub hidden_tests: HiddenMaterial<'a>,
    pub gold_patch: HiddenMaterial<'a>,
    pub acceptance_rules: HiddenMaterial<'a>,
    pub harness_config: HiddenMaterial<'a>,
    pub admission: Option<CoreTrialAdmissionBinding<'a>>,
}

#[derive(Clone, Copy)]
pub struct CoreTrialAdmissionBinding<'a> {
    pub scheduler_run_id: &'a str,
    pub authority_position: u64,
    pub nonce: &'a str,
    pub token_digest: &'a str,
    pub consumption_position: u64,
    pub consumption_digest: &'a str,
    pub run_config_digest: &'a str,
}

impl CoreTrialAdmissionBinding<'_> {
    fn valid(self) -> bool {
        !self.scheduler_run_id.is_empty()
            && self.authority_position > 0
            && !self.nonce.is_empty()
            && !self.token_digest.is_empty()
            && self.consumption_position > 0
            && !self.consumption_digest.is_empty()
            && !self.run_config_digest.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CoreTrialArtifacts {
    pub applied_patch: AuthenticatedArtifact,
    pub final_tree: AuthenticatedArtifact,
    pub agent_output: AuthenticatedArtifact,
    pub events: AuthenticatedArtifact,
    pub logs: AuthenticatedArtifact,
    pub artifacts_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AuthenticatedArtifact {
    pub bytes: Vec<u8>,
    pub digest: String,
}

impl AuthenticatedArtifact {
    fn new(bytes: Vec<u8>) -> Self {
        let digest = sha256(&bytes);
        Self { bytes, digest }
    }
}

#[derive(Clone, Debug)]
pub struct CoreTrialExecution {
    pub boundaries: kit::executor::trial::BoundaryPair,
    pub grade: GradeReport,
    pub artifacts: CoreTrialArtifacts,
    pub usage: TrialUsage,
    pub provider_request_ids: Vec<String>,
}

pub trait CoreTrialExecutor {
    fn execute_core(
        &mut self,
        request: CoreTrialRequest<'_>,
    ) -> Result<CoreTrialExecution, CoreTrialError>;
}

#[derive(Debug)]
pub struct ConformanceCoreTrialExecutor {
    boundary: SemanticsOnlyTrialExecutor,
    sequence: u64,
    route_calls: usize,
    isolated_grader_calls: usize,
    next_fault: Option<ConformanceFault>,
    authoritative_memory_evidence: bool,
}

#[derive(Clone, Copy, Debug)]
enum ConformanceFault {
    Crash,
    OutcomeUnknown,
    ArtifactDigest,
    GradeCheck,
    RouteMismatch,
    UsageOverage,
    UsageUnavailable,
    SensitiveArtifact(GraderTestChannel, GraderTestEncoding),
}

#[allow(clippy::derivable_impls)]
impl Default for ConformanceCoreTrialExecutor {
    fn default() -> Self {
        Self {
            boundary: SemanticsOnlyTrialExecutor::default(),
            sequence: 0,
            route_calls: 0,
            isolated_grader_calls: 0,
            next_fault: None,
            authoritative_memory_evidence: cfg!(target_os = "linux"),
        }
    }
}

impl ConformanceCoreTrialExecutor {
    #[cfg(any(test, debug_assertions))]
    pub fn source_semantics_fake(_: TrustedSourceSemanticsToken) -> Self {
        Self {
            authoritative_memory_evidence: true,
            ..Self::default()
        }
    }

    pub fn without_authoritative_memory_evidence() -> Self {
        Self {
            authoritative_memory_evidence: false,
            ..Self::default()
        }
    }
    pub fn route_calls(&self) -> usize {
        self.route_calls
    }

    pub const fn in_process_agent_calls(&self) -> usize {
        0
    }

    pub const fn in_process_grader_calls(&self) -> usize {
        0
    }

    pub const fn isolated_grader_calls(&self) -> usize {
        self.isolated_grader_calls
    }

    pub const fn hidden_agent_accesses(&self) -> usize {
        0
    }

    pub fn crash_once(&mut self) {
        self.next_fault = Some(ConformanceFault::Crash);
    }

    pub fn outcome_unknown_once(&mut self) {
        self.next_fault = Some(ConformanceFault::OutcomeUnknown);
    }

    pub fn substitute_artifact_digest_once(&mut self) {
        self.next_fault = Some(ConformanceFault::ArtifactDigest);
    }

    pub fn substitute_grade_check_once(&mut self) {
        self.next_fault = Some(ConformanceFault::GradeCheck);
    }

    pub fn mismatch_route_once(&mut self) {
        self.next_fault = Some(ConformanceFault::RouteMismatch);
    }

    pub fn exceed_usage_once(&mut self) {
        self.next_fault = Some(ConformanceFault::UsageOverage);
    }

    pub fn unavailable_usage_once(&mut self) {
        self.next_fault = Some(ConformanceFault::UsageUnavailable);
    }

    pub fn leak_sensitive_artifact_once(&mut self) {
        self.leak_sensitive_logs_once();
    }

    pub fn leak_sensitive_logs_once(&mut self) {
        self.leak_sensitive_probe_once(GraderTestChannel::GraderLog, GraderTestEncoding::Raw);
    }

    pub fn leak_sensitive_report_once(&mut self) {
        self.leak_sensitive_probe_once(GraderTestChannel::CanonicalReport, GraderTestEncoding::Raw);
    }

    pub fn leak_sensitive_checks_once(&mut self) {
        self.leak_sensitive_probe_once(GraderTestChannel::Checks, GraderTestEncoding::Raw);
    }

    pub fn leak_sensitive_final_tree_once(&mut self) {
        self.leak_sensitive_probe_once(GraderTestChannel::FinalTree, GraderTestEncoding::Raw);
    }

    pub fn leak_sensitive_channel_once(&mut self) {
        self.leak_sensitive_probe_once(GraderTestChannel::ExtraArtifact, GraderTestEncoding::Raw);
    }

    pub fn leak_sensitive_probe_once(
        &mut self,
        channel: GraderTestChannel,
        encoding: GraderTestEncoding,
    ) {
        self.next_fault = Some(ConformanceFault::SensitiveArtifact(channel, encoding));
    }
}

#[cfg(any(test, debug_assertions))]
pub struct TrustedSourceSemanticsToken(());

#[cfg(any(test, debug_assertions))]
pub fn trusted_source_semantics_token() -> TrustedSourceSemanticsToken {
    TrustedSourceSemanticsToken(())
}

impl CoreTrialExecutor for ConformanceCoreTrialExecutor {
    fn execute_core(
        &mut self,
        request: CoreTrialRequest<'_>,
    ) -> Result<CoreTrialExecution, CoreTrialError> {
        if !cfg!(any(test, debug_assertions)) {
            return Err(CoreTrialError::ConformanceUnavailable(
                "conformance executor is excluded from release execution".to_owned(),
            ));
        }
        if request.admission.is_some_and(|binding| !binding.valid()) {
            return Err(CoreTrialError::Executor(
                "invalid immutable statistical admission binding".to_owned(),
            ));
        }
        self.route_calls += 1;
        let fault = self.next_fault.take();
        match fault {
            Some(ConformanceFault::Crash) => return Err(CoreTrialError::Crashed),
            Some(ConformanceFault::OutcomeUnknown) => return Err(CoreTrialError::OutcomeUnknown),
            _ => {}
        }
        let handles = [
            request.hidden_tests.handle.name(),
            request.gold_patch.handle.name(),
            request.acceptance_rules.handle.name(),
            request.harness_config.handle.name(),
        ];
        let mut unique = BTreeSet::new();
        if handles.iter().any(|handle| !unique.insert(*handle)) {
            return Err(CoreTrialError::InvalidHiddenHandle);
        }
        if !cfg!(target_os = "linux") && !self.authoritative_memory_evidence {
            return Err(CoreTrialError::ConformanceUnavailable(
                "no proven hard grader-memory backend on this host".to_owned(),
            ));
        }
        let effective_bounds = effective_grader_bounds(request.manifest, request.bounds)?;
        let hidden_canaries = hidden_canaries(
            request.hidden_tests.bytes,
            &effective_bounds,
            request.checks,
        )?;
        let hidden_bytes = request
            .hidden_tests
            .bytes
            .len()
            .checked_add(request.gold_patch.bytes.len())
            .and_then(|value| value.checked_add(request.acceptance_rules.bytes.len()))
            .and_then(|value| value.checked_add(request.harness_config.bytes.len()))
            .ok_or(CoreTrialError::Bounds)?;
        if hidden_bytes > effective_bounds.max_artifact_bytes {
            return Err(CoreTrialError::Bounds);
        }

        let identity = FreshTrialIdentity::for_conformance(self.sequence);
        self.sequence = self.sequence.checked_add(1).ok_or(CoreTrialError::Bounds)?;
        let mut grade_result = None;
        let boundaries = {
            let mut boundary = ConformanceGradingBoundary {
                semantics: &mut self.boundary,
                source: request.source,
                patch: request.patch,
                checks: request.checks,
                bounds: &effective_bounds,
                hidden_tests: request.hidden_tests.bytes,
                gold_patch: request.gold_patch.bytes,
                acceptance_rules: request.acceptance_rules.bytes,
                harness_config: request.harness_config.bytes,
                grade_result: &mut grade_result,
                grader_calls: &mut self.isolated_grader_calls,
            };
            orchestrate_conformance(request.manifest, &identity, &mut boundary)
                .map_err(map_trial_error)?
        };
        let grade = grade_result
            .ok_or_else(|| {
                CoreTrialError::Executor("grader boundary produced no result".to_owned())
            })?
            .map_err(|error| CoreTrialError::Grader(error.to_string()))?;
        let applied_patch = AuthenticatedArtifact::new(request.patch.to_vec());
        let agent_output = AuthenticatedArtifact::new(
            serde_json::to_vec(&AgentResultBinding {
                schema_version: 1,
                trial_id: request.manifest.trial_id(),
                patch_digest: &applied_patch.digest,
            })
            .map_err(|error| CoreTrialError::Serialization(error.to_string()))?,
        );
        let events = AuthenticatedArtifact::new(
            serde_json::to_vec(&PublicEvents {
                schema_version: 1,
                phases: ["agent", "grader"],
                patch_digest: &applied_patch.digest,
            })
            .map_err(|error| CoreTrialError::Serialization(error.to_string()))?,
        );
        let logs = AuthenticatedArtifact::new(b"core grader completed\n".to_vec());
        if logs.bytes.len() > request.bounds.max_log_bytes {
            return Err(CoreTrialError::Bounds);
        }
        let final_tree = AuthenticatedArtifact::new(grade.final_tree_artifact.clone());
        let artifacts_digest = canonical_digest(&ArtifactBinding {
            patch_digest: &applied_patch.digest,
            agent_result_digest: &agent_output.digest,
            events_digest: &events.digest,
            logs_digest: &logs.digest,
            final_tree_digest: &final_tree.digest,
        })?;
        let mut execution = CoreTrialExecution {
            boundaries,
            grade,
            artifacts: CoreTrialArtifacts {
                applied_patch,
                final_tree,
                agent_output,
                events,
                logs,
                artifacts_digest,
            },
            usage: TrialUsage::ZERO,
            provider_request_ids: Vec::new(),
        };
        match fault {
            Some(ConformanceFault::ArtifactDigest) => {
                execution.artifacts.events.digest = format!("sha256:{}", "f".repeat(64));
            }
            Some(ConformanceFault::GradeCheck) => {
                if let Some(check) = execution.grade.checks.first_mut() {
                    check.id = "substituted".to_owned();
                }
            }
            Some(ConformanceFault::RouteMismatch) => {
                execution.boundaries.grader.route = ExecutionRoute::TrustedContainerHelper;
            }
            Some(ConformanceFault::UsageOverage) => {
                execution.usage.input_tokens =
                    kit::executor::trial::UsageMeasure::Measured(u64::MAX);
            }
            Some(ConformanceFault::UsageUnavailable) => {
                execution.usage.input_tokens = kit::executor::trial::UsageMeasure::Unavailable(
                    kit::executor::trial::UsageUnavailableReason::ProviderDidNotReport,
                );
            }
            Some(ConformanceFault::SensitiveArtifact(channel, encoding)) => {
                let canary = encode_test_canary(
                    hidden_canaries
                        .first()
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                    encoding,
                    4096,
                );
                match channel {
                    GraderTestChannel::GraderLog => {
                        execution.artifacts.logs = AuthenticatedArtifact::new(canary);
                    }
                    GraderTestChannel::CanonicalReport => {
                        execution.grade.diagnostic =
                            Some(String::from_utf8(canary).map_err(|_| {
                                CoreTrialError::Executor("invalid test canary".to_owned())
                            })?)
                    }
                    GraderTestChannel::Checks => {
                        execution
                            .grade
                            .checks
                            .first_mut()
                            .ok_or_else(|| {
                                CoreTrialError::Executor("missing test check".to_owned())
                            })?
                            .actual = String::from_utf8(canary).map_err(|_| {
                            CoreTrialError::Executor("invalid test canary".to_owned())
                        })?;
                    }
                    GraderTestChannel::FinalTree => {
                        execution.artifacts.final_tree = AuthenticatedArtifact::new(canary);
                    }
                    GraderTestChannel::ExtraArtifact => {
                        execution.artifacts.events = AuthenticatedArtifact::new(canary);
                    }
                }
            }
            Some(ConformanceFault::Crash | ConformanceFault::OutcomeUnknown) | None => {}
        }
        Ok(execution)
    }
}

fn encode_test_canary(
    canary: &[u8],
    encoding: GraderTestEncoding,
    scanner_chunk: usize,
) -> Vec<u8> {
    match encoding {
        GraderTestEncoding::Raw => canary.to_vec(),
        GraderTestEncoding::Percent => canary
            .iter()
            .flat_map(|byte| format!("%{byte:02X}").into_bytes())
            .collect(),
        GraderTestEncoding::Base64 => base64(canary),
        GraderTestEncoding::Split => {
            let mut bytes = vec![b'x'; scanner_chunk.saturating_sub(canary.len() / 2)];
            bytes.extend_from_slice(canary);
            bytes
        }
        GraderTestEncoding::Binary => {
            let mut bytes = vec![0];
            bytes.extend_from_slice(canary);
            bytes.push(0x7f);
            bytes
        }
    }
}

fn base64(bytes: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(ALPHABET[((value >> 18) & 63) as usize]);
        output.push(ALPHABET[((value >> 12) & 63) as usize]);
        output.push(if chunk.len() > 1 {
            ALPHABET[((value >> 6) & 63) as usize]
        } else {
            b'='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(value & 63) as usize]
        } else {
            b'='
        });
    }
    output
}

pub struct ProductionCoreTrialExecutor<'a> {
    pub workspace: &'a kit::workspace::acquire::AcquisitionResult,
    pub record_root: &'a std::path::Path,
    pub owner: kit::domain::lifecycle::ProcessOwnership,
    pub process_registry: kit::executor::process::own::ProcessRegistryRegistration,
    pub cancellation: Option<(
        &'a kit::executor::cancel::SqliteCancellationCoordinator,
        kit::executor::cancel::WorkspaceIdentity,
    )>,
    pub agent_command: &'a AgentCommand<'a>,
    pub usage_receipt: &'a TrialUsageReceipt,
    pub usage_receipts: &'a dyn TrialUsageReceiptStore,
}

pub type AgentCommand<'a> = dyn Fn(&[u8]) -> Result<Vec<std::ffi::OsString>, CoreTrialError> + 'a;

impl CoreTrialExecutor for ProductionCoreTrialExecutor<'_> {
    fn execute_core(
        &mut self,
        request: CoreTrialRequest<'_>,
    ) -> Result<CoreTrialExecution, CoreTrialError> {
        if request.admission.is_some_and(|binding| !binding.valid()) {
            return Err(CoreTrialError::Executor(
                "invalid immutable statistical admission binding".to_owned(),
            ));
        }
        let trusted = |bytes, digest| TrustedInput {
            source: TrustedInputSource::Bytes(bytes),
            expected_sha256: digest,
        };
        let agent_command = (self.agent_command)(request.patch)?;
        let trial = execute_production_trial(ProductionTrialRequest {
            manifest: request.manifest,
            workspace: self.workspace,
            record_root: self.record_root,
            owner: self.owner,
            process_registry: self.process_registry.clone(),
            cancellation: self.cancellation.clone(),
            agent_command: &agent_command,
            grader_inputs: GraderInputs {
                specification: trusted(
                    request.specification,
                    request.manifest.specification_digest(),
                ),
                scaffold: trusted(request.scaffold, request.manifest.scaffold_digest()),
                hidden_tests: trusted(
                    request.hidden_tests.bytes,
                    request.manifest.hidden_tests_digest(),
                ),
                gold_patch: trusted(
                    request.gold_patch.bytes,
                    request.manifest.gold_patch_digest(),
                ),
                acceptance_rules: trusted(
                    request.acceptance_rules.bytes,
                    request.manifest.acceptance_digest(),
                ),
                harness_config: trusted(
                    request.harness_config.bytes,
                    request.manifest.harness_config_digest(),
                ),
                harness_commit: request.manifest.grader_harness_commit(),
            },
            usage_receipt: self.usage_receipt,
            usage_receipts: self.usage_receipts,
            grader_resource_bounds: Some(GraderResourceBounds {
                memory_bytes: request.bounds.max_memory_bytes as u64,
                output_bytes: request.bounds.max_artifact_bytes as u64,
                wall_time_millis: request.bounds.max_time_millis,
            }),
            grader_test_probe: None,
        })
        .map_err(map_trial_error)?;
        production_execution(trial)
    }
}

fn production_execution(trial: DurableTrial) -> Result<CoreTrialExecution, CoreTrialError> {
    let artifact_root = trial
        .record_path
        .parent()
        .ok_or_else(|| CoreTrialError::Executor("trial record has no parent".to_owned()))?
        .join("artifacts/grader");
    let record = trial.record;
    let channels = &record.grader_result.artifacts;
    let applied_patch = AuthenticatedArtifact::new(read_persisted_channel(
        &artifact_root,
        &channels.applied_patch,
    )?);
    let final_tree = AuthenticatedArtifact::new(read_persisted_channel(
        &artifact_root,
        &channels.final_tree,
    )?);
    let checks: CheckChannel =
        serde_json::from_slice(&read_persisted_channel(&artifact_root, &channels.checks)?)
            .map_err(|error| CoreTrialError::Grader(error.to_string()))?;
    let agent_output = AuthenticatedArtifact::new(read_persisted_channel(
        &artifact_root,
        &channels.agent_output,
    )?);
    let events =
        AuthenticatedArtifact::new(read_persisted_channel(&artifact_root, &channels.events)?);
    let logs = AuthenticatedArtifact::new(read_persisted_channel(&artifact_root, &channels.logs)?);
    let usage: kit::executor::trial::GraderUsageReport = serde_json::from_slice(
        &read_persisted_channel(&artifact_root, &channels.usage_report)?,
    )
    .map_err(|error| CoreTrialError::Grader(error.to_string()))?;
    let report = &record.grader_result.report;
    let hidden = HiddenCheckAggregate {
        verdict: match report.hidden.verdict {
            kit::executor::trial::GraderVerdict::Pass => core_grader::GradeOutcome::Success,
            kit::executor::trial::GraderVerdict::Fail => core_grader::GradeOutcome::Failure,
            kit::executor::trial::GraderVerdict::Error => core_grader::GradeOutcome::Error,
        },
        count: usize::try_from(report.hidden.count).map_err(|_| CoreTrialError::Bounds)?,
        digest: report.hidden.digest.clone(),
    };
    if checks.hidden != hidden {
        return Err(CoreTrialError::Grader(
            "hidden-check aggregate mismatch".to_owned(),
        ));
    }
    let grade = GradeReport {
        schema_version: report.schema_version,
        outcome: match report.outcome {
            kit::executor::trial::CoreGradeOutcome::Success => core_grader::GradeOutcome::Success,
            kit::executor::trial::CoreGradeOutcome::Failure => core_grader::GradeOutcome::Failure,
            kit::executor::trial::CoreGradeOutcome::Error => core_grader::GradeOutcome::Error,
        },
        base_tree_digest: report.base_tree_digest.clone(),
        patch_digest: report.patch_digest.clone(),
        final_tree_digest: report.final_tree_digest.clone(),
        final_tree_artifact: final_tree.bytes.clone(),
        checks: checks.public,
        hidden_checks: Vec::new(),
        hidden,
        diagnostic: report.diagnostic.clone(),
        timing: core_grader::GradeTiming {
            wall_millis: report.timing.wall_millis,
        },
    };
    let artifacts_digest = canonical_digest(&ArtifactBinding {
        patch_digest: &applied_patch.digest,
        agent_result_digest: &agent_output.digest,
        events_digest: &events.digest,
        logs_digest: &logs.digest,
        final_tree_digest: &final_tree.digest,
    })?;
    Ok(CoreTrialExecution {
        boundaries: kit::executor::trial::BoundaryPair {
            agent: record.agent,
            grader: record.grader,
        },
        grade,
        artifacts: CoreTrialArtifacts {
            applied_patch,
            final_tree,
            agent_output,
            events,
            logs,
            artifacts_digest,
        },
        usage: usage.usage,
        provider_request_ids: usage.provider_request_ids,
    })
}

fn read_persisted_channel(
    root: &Path,
    channel: &kit::executor::trial::GraderArtifactHandle,
) -> Result<Vec<u8>, CoreTrialError> {
    let path = root.join(&channel.handle);
    let metadata =
        fs::symlink_metadata(&path).map_err(|error| CoreTrialError::Executor(error.to_string()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != channel.length
    {
        return Err(CoreTrialError::Grader(
            "persisted grader channel stat mismatch".to_owned(),
        ));
    }
    let mut bytes =
        Vec::with_capacity(usize::try_from(channel.length).map_err(|_| CoreTrialError::Bounds)?);
    fs::File::open(path)
        .and_then(|file| {
            file.take(channel.length.saturating_add(1))
                .read_to_end(&mut bytes)
        })
        .map_err(|error| CoreTrialError::Executor(error.to_string()))?;
    if bytes.len() as u64 != channel.length || sha256(&bytes) != channel.digest {
        return Err(CoreTrialError::Grader(
            "persisted grader channel digest mismatch".to_owned(),
        ));
    }
    Ok(bytes)
}

struct ConformanceGradingBoundary<'a> {
    semantics: &'a mut SemanticsOnlyTrialExecutor,
    source: &'a SourceSnapshot,
    patch: &'a [u8],
    checks: &'a [Check],
    bounds: &'a GraderBounds,
    hidden_tests: &'a [u8],
    gold_patch: &'a [u8],
    acceptance_rules: &'a [u8],
    harness_config: &'a [u8],
    grade_result: &'a mut Option<Result<GradeReport, CoreTrialError>>,
    grader_calls: &'a mut usize,
}

impl IsolatedTrialContract for ConformanceGradingBoundary<'_> {
    fn execute(&mut self, request: BoundaryRequest<'_>) -> Result<BoundaryCompletion, TrialError> {
        let phase = request.phase;
        let completion = self.semantics.execute(request)?;
        if phase == TrialPhase::Grader {
            *self.grader_calls = self
                .grader_calls
                .checked_add(1)
                .ok_or_else(|| TrialError::Executor("grader call count overflow".to_owned()))?;
            *self.grade_result = Some(execute_grader_subprocess(GraderInvocation {
                source: self.source,
                patch: self.patch,
                checks: self.checks,
                bounds: self.bounds,
                hidden_tests: self.hidden_tests,
                gold_patch: self.gold_patch,
                acceptance_rules: self.acceptance_rules,
                harness_config: self.harness_config,
            }));
        }
        Ok(completion)
    }
}

#[derive(Serialize)]
struct GraderProtocolRequest<'a> {
    schema_version: u16,
    source: Vec<(&'a str, &'a [u8])>,
    patch: &'a [u8],
    checks: &'a [Check],
    bounds: &'a GraderBounds,
    hidden_tests: &'a [u8],
    gold_patch: &'a [u8],
    acceptance_rules: &'a [u8],
    harness_config: &'a [u8],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GraderProtocolResponse {
    schema_version: u16,
    report: GradeMetadata,
    channels: Vec<AuthenticatedChannel>,
    input_digests: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckChannel {
    public: Vec<CheckEvidence>,
    hidden: HiddenCheckAggregate,
}

struct TemporaryArtifactRoot(PathBuf);

impl Drop for TemporaryArtifactRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct GraderInvocation<'a> {
    source: &'a SourceSnapshot,
    patch: &'a [u8],
    checks: &'a [Check],
    bounds: &'a GraderBounds,
    hidden_tests: &'a [u8],
    gold_patch: &'a [u8],
    acceptance_rules: &'a [u8],
    harness_config: &'a [u8],
}

fn effective_grader_bounds(
    manifest: &ImmutableTrialManifest,
    requested: &GraderBounds,
) -> Result<GraderBounds, CoreTrialError> {
    let resources = manifest
        .profile()
        .map_err(|error| CoreTrialError::Executor(error.to_string()))?
        .resources()
        .to_owned();
    let memory = usize::try_from(resources.memory_bytes).map_err(|_| CoreTrialError::Bounds)?;
    let output = usize::try_from(resources.output_bytes).map_err(|_| CoreTrialError::Bounds)?;
    let mut effective = requested.clone();
    effective.max_memory_bytes = effective.max_memory_bytes.min(memory);
    effective.max_artifact_bytes = effective.max_artifact_bytes.min(output);
    effective.max_time_millis = effective.max_time_millis.min(resources.wall_time_millis);
    effective.validate().map_err(|_| CoreTrialError::Bounds)?;
    Ok(effective)
}

fn hidden_canaries(
    bytes: &[u8],
    bounds: &GraderBounds,
    public_checks: &[Check],
) -> Result<Vec<Vec<u8>>, CoreTrialError> {
    let hidden: HiddenTestManifest =
        serde_json::from_slice(bytes).map_err(|error| CoreTrialError::Grader(error.to_string()))?;
    hidden
        .validated_canaries(bounds, public_checks)
        .map_err(|error| CoreTrialError::Grader(error.to_string()))
}

fn execute_grader_subprocess(input: GraderInvocation<'_>) -> Result<GradeReport, CoreTrialError> {
    let GraderInvocation {
        source,
        patch,
        checks,
        bounds,
        hidden_tests,
        gold_patch,
        acceptance_rules,
        harness_config,
    } = input;
    let executable = std::env::var_os("CARGO_BIN_EXE_kit")
        .map(Into::into)
        .or_else(|| {
            std::env::current_exe()
                .ok()?
                .parent()?
                .parent()
                .map(|directory| directory.join(format!("kit{}", std::env::consts::EXE_SUFFIX)))
        })
        .ok_or_else(|| CoreTrialError::Executor("core grader executable unavailable".to_owned()))?;
    let request = serde_json::to_vec(&GraderProtocolRequest {
        schema_version: 1,
        source: source
            .files()
            .iter()
            .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
            .collect(),
        patch,
        checks,
        bounds,
        hidden_tests,
        gold_patch,
        acceptance_rules,
        harness_config,
    })
    .map_err(|error| CoreTrialError::Serialization(error.to_string()))?;
    if request.len() > bounds.max_memory_bytes {
        return Err(CoreTrialError::Bounds);
    }
    let artifact_root = TemporaryArtifactRoot(new_grader_artifact_root()?);
    let auth_key = random_hex(32)?;
    let mut command = Command::new(executable);
    command
        .arg("__kit-core-grader")
        .env("KIT_CORE_GRADER_ARTIFACT_ROOT", &artifact_root.0)
        .env("KIT_CORE_GRADER_AUTH_KEY", &auth_key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::process::CommandExt;
        let memory = u64::try_from(bounds.max_memory_bytes).map_err(|_| CoreTrialError::Bounds)?;
        // SAFETY: the closure invokes only async-signal-safe setrlimit before exec.
        unsafe {
            command.pre_exec(move || {
                let limit = libc::rlimit {
                    rlim_cur: memory,
                    rlim_max: memory,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| CoreTrialError::Executor(error.to_string()))?;
    child
        .stdin
        .take()
        .ok_or_else(|| CoreTrialError::Executor("grader stdin unavailable".to_owned()))?
        .write_all(&request)
        .map_err(|error| CoreTrialError::Executor(error.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CoreTrialError::Executor("grader stdout unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CoreTrialError::Executor("grader stderr unavailable".to_owned()))?;
    let output_limit = 64_usize * 1024;
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(output_limit.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let stderr_limit = bounds.max_log_bytes;
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .take(stderr_limit.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(bounds.max_time_millis))
        .ok_or(CoreTrialError::Bounds)?;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| CoreTrialError::Executor(error.to_string()))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CoreTrialError::Bounds);
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let output = stdout_reader
        .join()
        .map_err(|_| CoreTrialError::Executor("grader stdout reader panicked".to_owned()))?
        .map_err(|error| CoreTrialError::Executor(error.to_string()))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| CoreTrialError::Executor("grader stderr reader panicked".to_owned()))?
        .map_err(|error| CoreTrialError::Executor(error.to_string()))?;
    if output.len() > output_limit || stderr.len() > bounds.max_log_bytes {
        return Err(CoreTrialError::Bounds);
    }
    if !status.success() {
        return Err(CoreTrialError::Grader(
            String::from_utf8_lossy(&stderr).into_owned(),
        ));
    }
    let response: GraderProtocolResponse = serde_json::from_slice(&output)
        .map_err(|error| CoreTrialError::Grader(error.to_string()))?;
    if response.schema_version != 1 {
        return Err(CoreTrialError::Grader(
            "unsupported grader protocol version".to_owned(),
        ));
    }
    if response.input_digests
        != BTreeMap::from([
            ("acceptance_rules".to_owned(), sha256(acceptance_rules)),
            ("gold_patch".to_owned(), sha256(gold_patch)),
            ("harness_config".to_owned(), sha256(harness_config)),
            ("hidden_tests".to_owned(), sha256(hidden_tests)),
        ])
    {
        return Err(CoreTrialError::Grader(
            "grader input digest attestation mismatch".to_owned(),
        ));
    }
    let expected = [
        ("diff", "applied.patch"),
        ("file", "final-tree.json"),
        ("report", "checks.json"),
        ("index", "events.json"),
        ("log", "grader.log"),
        ("report", "agent-output.json"),
        ("report", "usage.json"),
        ("restricted_encrypted", "hidden-checks.enc"),
    ];
    if response.channels.len() != expected.len() {
        return Err(CoreTrialError::Grader(
            "grader channel inventory mismatch".to_owned(),
        ));
    }
    let mut channel_bytes = BTreeMap::new();
    let mut total = 0_usize;
    for (channel, (class, handle)) in response.channels.iter().zip(expected) {
        if channel.class != class || channel.handle != handle {
            return Err(CoreTrialError::Grader(
                "grader channel inventory mismatch".to_owned(),
            ));
        }
        let limit = if class == "log" {
            bounds.max_log_bytes
        } else {
            bounds.max_artifact_bytes
        };
        let bytes = read_authenticated_channel(&artifact_root.0, channel, &auth_key, limit)?;
        total = total
            .checked_add(bytes.len())
            .ok_or(CoreTrialError::Bounds)?;
        if total > bounds.max_artifact_bytes {
            return Err(CoreTrialError::Bounds);
        }
        channel_bytes.insert(handle, bytes);
    }
    let applied_patch = channel_bytes.remove("applied.patch").unwrap();
    let final_tree_artifact = channel_bytes.remove("final-tree.json").unwrap();
    let checks: CheckChannel =
        serde_json::from_slice(&channel_bytes.remove("checks.json").unwrap())
            .map_err(|error| CoreTrialError::Grader(error.to_string()))?;
    if applied_patch != patch || sha256(&final_tree_artifact) != response.report.final_tree_digest {
        return Err(CoreTrialError::Grader(
            "grader channel binding mismatch".to_owned(),
        ));
    }
    Ok(GradeReport {
        schema_version: response.report.schema_version,
        outcome: response.report.outcome,
        base_tree_digest: response.report.base_tree_digest,
        patch_digest: response.report.patch_digest,
        final_tree_digest: response.report.final_tree_digest,
        final_tree_artifact,
        checks: checks.public,
        hidden_checks: Vec::new(),
        hidden: checks.hidden,
        diagnostic: response.report.diagnostic,
        timing: response.report.timing,
    })
}

fn new_grader_artifact_root() -> Result<PathBuf, CoreTrialError> {
    let path = std::env::temp_dir().join(format!(
        "kit-core-grader-{}-{}",
        std::process::id(),
        random_hex(16)?
    ));
    fs::create_dir(&path).map_err(|error| CoreTrialError::Executor(error.to_string()))?;
    Ok(path)
}

fn random_hex(bytes: usize) -> Result<String, CoreTrialError> {
    let mut value = vec![0_u8; bytes];
    getrandom::fill(&mut value)
        .map_err(|_| CoreTrialError::Executor("grader randomness unavailable".to_owned()))?;
    Ok(value.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn read_authenticated_channel(
    root: &Path,
    channel: &AuthenticatedChannel,
    auth_key: &str,
    limit: usize,
) -> Result<Vec<u8>, CoreTrialError> {
    if channel.handle.contains(['/', '\\'])
        || channel.length > limit as u64
        || channel.authentication
            != core_grader::channel_authentication(
                auth_key.as_bytes(),
                &channel.class,
                &channel.handle,
                &channel.digest,
                channel.length,
            )
    {
        return Err(CoreTrialError::Grader(
            "grader channel authentication failed".to_owned(),
        ));
    }
    let path = root.join(&channel.handle);
    let metadata =
        fs::symlink_metadata(&path).map_err(|error| CoreTrialError::Grader(error.to_string()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != channel.length
    {
        return Err(CoreTrialError::Grader(
            "grader channel stat mismatch".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(channel.length as usize);
    fs::File::open(path)
        .and_then(|file| {
            file.take(limit.saturating_add(1) as u64)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| CoreTrialError::Grader(error.to_string()))?;
    if bytes.len() as u64 != channel.length || sha256(&bytes) != channel.digest {
        return Err(CoreTrialError::Grader(
            "grader channel digest mismatch".to_owned(),
        ));
    }
    Ok(bytes)
}

#[derive(Serialize)]
struct AgentResultBinding<'a> {
    schema_version: u16,
    trial_id: &'a str,
    patch_digest: &'a str,
}

#[derive(Serialize)]
struct PublicEvents<'a> {
    schema_version: u16,
    phases: [&'static str; 2],
    patch_digest: &'a str,
}

#[derive(Serialize)]
struct ArtifactBinding<'a> {
    patch_digest: &'a str,
    agent_result_digest: &'a str,
    events_digest: &'a str,
    logs_digest: &'a str,
    final_tree_digest: &'a str,
}

fn canonical_digest(value: &impl Serialize) -> Result<String, CoreTrialError> {
    serde_json::to_vec(value)
        .map(|bytes| sha256(&bytes))
        .map_err(|error| CoreTrialError::Serialization(error.to_string()))
}

fn map_trial_error(error: TrialError) -> CoreTrialError {
    match error {
        TrialError::Unavailable(unavailable) => {
            CoreTrialError::ExternalBlocked(unavailable.to_string())
        }
        TrialError::BoundaryFailed(_, _)
        | TrialError::BoundaryNotQuiescent(_)
        | TrialError::BoundaryIdentityMismatch(_) => CoreTrialError::OutcomeUnknown,
        other => CoreTrialError::Executor(other.to_string()),
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum CoreTrialError {
    ExternalBlocked(String),
    ConformanceUnavailable(String),
    OutcomeUnknown,
    Crashed,
    Bounds,
    InvalidHiddenHandle,
    Executor(String),
    Grader(String),
    Serialization(String),
}

impl std::fmt::Display for CoreTrialError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExternalBlocked(detail) => write!(formatter, "external trial blocked: {detail}"),
            Self::ConformanceUnavailable(detail) => {
                write!(formatter, "local conformance unavailable: {detail}")
            }
            Self::OutcomeUnknown => formatter.write_str("isolated trial outcome_unknown"),
            Self::Crashed => formatter.write_str("isolated trial executor crashed"),
            Self::Bounds => formatter.write_str("isolated trial bound exceeded"),
            Self::InvalidHiddenHandle => formatter.write_str("invalid or reused hidden handle"),
            Self::Executor(detail) => write!(formatter, "isolated trial executor failed: {detail}"),
            Self::Grader(detail) => write!(formatter, "isolated grader failed: {detail}"),
            Self::Serialization(detail) => {
                write!(formatter, "trial serialization failed: {detail}")
            }
        }
    }
}

impl std::error::Error for CoreTrialError {}
