use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{
    api::auth::contract::{AuthenticatedPrincipal, GrantSnapshot},
    domain::{
        config::{Grant, RunConfigSnapshot},
        secret::SecretLease,
    },
    executor::{
        check::{
            CheckArtifactRef, CheckCommand, CheckCompletion, CheckExecutionFailure,
            CheckExecutionRequest, CheckExecutorError, CheckFailureState, CheckProcessEvidence,
            CheckRunner, CheckStatus, immutable_tree_digest, validate_process_evidence_report,
        },
        profile::ResourceLimits,
    },
    store::artifacts::{
        ArtifactClass, ArtifactMetadata, ArtifactReference, ArtifactRetention, ArtifactStore,
        now_unix_micros,
    },
    telemetry::redact::{CaptureBoundary, CaptureRedactor},
};

pub const VERIFICATION_PROFILE_VERSION: u16 = 3;
const MAX_CHECKS: usize = 64;
const MAX_PREVIEW_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationProfile {
    None,
    Syntax,
    Fast,
    Targeted,
    Full,
}

impl VerificationProfile {
    pub const ALL: [Self; 5] = [
        Self::None,
        Self::Syntax,
        Self::Fast,
        Self::Targeted,
        Self::Full,
    ];
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileSelection {
    None,
    Syntax,
    Fast,
    Targeted { exact_targets: BTreeSet<String> },
    Full,
}

impl ProfileSelection {
    pub const fn profile(&self) -> VerificationProfile {
        match self {
            Self::None => VerificationProfile::None,
            Self::Syntax => VerificationProfile::Syntax,
            Self::Fast => VerificationProfile::Fast,
            Self::Targeted { .. } => VerificationProfile::Targeted,
            Self::Full => VerificationProfile::Full,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckClass {
    Syntax,
    Diagnostics,
    Typecheck,
    Targeted,
    Full,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckRequirement {
    Required,
    Advisory,
}

impl CheckRequirement {
    const fn required(self) -> bool {
        matches!(self, Self::Required)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeclaredCheck {
    class: CheckClass,
    command: CheckCommand,
    requirement: CheckRequirement,
    changed_path_prefixes: BTreeSet<String>,
    post_commit_safe: bool,
}

impl DeclaredCheck {
    pub fn new(
        class: CheckClass,
        command: CheckCommand,
        requirement: CheckRequirement,
        changed_path_prefixes: BTreeSet<String>,
        post_commit_safe: bool,
    ) -> Result<Self, VerificationError> {
        if changed_path_prefixes.iter().any(|path| !valid_path(path))
            || requirement.required() && post_commit_safe
        {
            return Err(VerificationError::MaliciousProjectConfiguration);
        }
        Ok(Self {
            class,
            command,
            requirement,
            changed_path_prefixes,
            post_commit_safe,
        })
    }

    pub const fn class(&self) -> CheckClass {
        self.class
    }

    pub const fn command(&self) -> &CheckCommand {
        &self.command
    }

    pub const fn requirement(&self) -> CheckRequirement {
        self.requirement
    }

    pub const fn cost(&self) -> ResourceLimits {
        self.command.resources()
    }

    fn affected(&self, changed: &BTreeSet<String>) -> bool {
        self.changed_path_prefixes.is_empty()
            || changed.iter().any(|path| {
                self.changed_path_prefixes.iter().any(|prefix| {
                    path == prefix
                        || path
                            .strip_prefix(prefix)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                })
            })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VerificationRegistry {
    checks: Vec<DeclaredCheck>,
}

impl VerificationRegistry {
    pub const fn empty() -> Self {
        Self { checks: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }

    pub(crate) fn checks(&self) -> &[DeclaredCheck] {
        &self.checks
    }

    pub fn new(checks: Vec<DeclaredCheck>) -> Result<Self, VerificationError> {
        let mut registry = Self { checks };
        registry.checks.sort_by(check_order);
        let mut ids = BTreeSet::new();
        if registry
            .checks
            .iter()
            .any(|check| !ids.insert(check.command.id()))
        {
            return Err(VerificationError::DuplicateCheck);
        }
        Ok(registry)
    }

    pub(crate) fn select_native(
        &self,
        selection: &ProfileSelection,
        grants: &GrantSnapshot,
        config: &RunConfigSnapshot,
    ) -> Result<Vec<DeclaredCheck>, VerificationError> {
        if grants.principal_id() != config.principal_id()
            || grants.project_id() != config.project_id()
            || config.effective().max_concurrent_tools == 0
        {
            return Err(VerificationError::AuthorityRequired);
        }
        let authorized = |grant| {
            grants.grants().contains(&grant) && config.effective_authority().contains(&grant)
        };
        select_checks(
            selection,
            self,
            &BTreeSet::new(),
            &PolicyContext {
                authority_id: String::new(),
                process_spawn: authorized(Grant::ProcessSpawn),
                targeted: authorized(Grant::VerificationTargeted),
                full: authorized(Grant::VerificationFull),
                budget: VerificationBudget {
                    max_checks: usize::try_from(config.effective().max_concurrent_tools)
                        .unwrap_or(usize::MAX)
                        .min(MAX_CHECKS),
                    max_preview_bytes: MAX_PREVIEW_BYTES,
                    aggregate: hard_resource_ceiling(),
                },
                retention: ArtifactRetention::Forever,
                stored_at_unix_micros: 0,
            },
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckFailureBehavior {
    Abort,
    Commit,
}

pub struct VerificationRequest<'a> {
    pub selection: ProfileSelection,
    pub registry: &'a VerificationRegistry,
    pub authenticated: &'a AuthenticatedPrincipal,
    pub grants: &'a GrantSnapshot,
    pub config: &'a RunConfigSnapshot,
    pub runner: Option<&'a mut CheckRunner>,
    pub observer: Option<&'a mut dyn VerificationObserver>,
    pub artifacts: &'a ArtifactStore,
    pub secrets: &'a [SecretLease],
    pub on_check_failure: CheckFailureBehavior,
    pub model_outcome: Option<&'a crate::agent::adapters::grammar_edit::GrammarEditOutcomeEvidence>,
    pub cancellation: Option<&'a AtomicBool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationBoundary {
    Started,
    Progress,
    Completed,
    Failure,
}

pub struct VerificationObservation<'a> {
    pub plan_digest: &'a str,
    pub check_id: &'a str,
    pub boundary: VerificationBoundary,
    pub status: Option<CheckResultStatus>,
}

pub trait VerificationObserver {
    fn observe(
        &mut self,
        observation: VerificationObservation<'_>,
    ) -> Result<(), VerificationError>;
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct VerificationBinding {
    revision: String,
    epoch: String,
    revision_digest: String,
    guard_binding: String,
    root_identity: String,
    plan_digest: String,
    state_digest: String,
    evidence_digest: String,
    changes_digest: String,
    syntax_evidence_digest: String,
    source_digest: String,
    experiment_identity: String,
    experiment_digest: String,
    model_outcome: Option<crate::agent::adapters::grammar_edit::GrammarEditOutcomeEvidence>,
}

impl VerificationBinding {
    fn digest(&self) -> String {
        digest(&serde_json::to_vec(self).expect("verification binding serialization cannot fail"))
    }
}

pub(crate) struct StagedVerificationInput<'a> {
    pub revision: &'a str,
    pub epoch: &'a str,
    pub revision_digest: &'a str,
    pub guard_binding: &'a [u8; 32],
    pub root_identity: &'a str,
    pub plan_digest: &'a str,
    pub state_digest: &'a str,
    pub evidence_digest: &'a str,
    pub changes_digest: &'a str,
    pub syntax_evidence_digest: &'a str,
    pub changed_paths: BTreeSet<String>,
    pub immutable_source: &'a Path,
    pub build: &'a Path,
    pub temp: &'a Path,
    pub authority_principal: &'a str,
    pub authority_project: &'a str,
    pub more_boundaries_after: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn verify_current(
    context: &crate::workspace::edit::validate::EditOperationContext,
    immutable_source: &Path,
    build: &Path,
    temp: &Path,
    changed_paths: BTreeSet<String>,
    request: VerificationRequest<'_>,
    more_boundaries_after: bool,
) -> Result<VerificationResult, VerificationError> {
    let guard_binding = [0_u8; 32];
    let root_identity = immutable_source.to_string_lossy();
    let state_digest = context.base_workspace_digest();
    verify_precommit(
        StagedVerificationInput {
            revision: context.base_revision(),
            epoch: context.base_epoch(),
            revision_digest: context.base_workspace_digest(),
            guard_binding: &guard_binding,
            root_identity: &root_identity,
            plan_digest: context.selected_plan_digest(),
            state_digest,
            evidence_digest: state_digest,
            changes_digest: state_digest,
            syntax_evidence_digest: state_digest,
            changed_paths,
            immutable_source,
            build,
            temp,
            authority_principal: &request.authenticated.principal_id().to_string(),
            authority_project: &request.grants.project_id().to_string(),
            more_boundaries_after,
        },
        request,
    )
}

#[derive(Clone, Copy)]
struct VerificationBudget {
    max_checks: usize,
    max_preview_bytes: usize,
    aggregate: ResourceLimits,
}

struct PolicyContext {
    authority_id: String,
    process_spawn: bool,
    targeted: bool,
    full: bool,
    budget: VerificationBudget,
    retention: ArtifactRetention,
    stored_at_unix_micros: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct PlanSpec {
    version: u16,
    profile: VerificationProfile,
    binding: VerificationBinding,
    changed_paths: BTreeSet<String>,
    checks: Vec<DeclaredCheck>,
    authority_id: String,
    max_checks: usize,
    max_preview_bytes: usize,
    on_check_failure: CheckFailureBehavior,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VerificationPlan {
    spec: PlanSpec,
    digest: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredOutcome {
    Pass,
    Fail,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitDecision {
    Commit,
    Abort,
    AlreadyCommittedWithFailure,
}

pub const fn declared_decision(
    profile: VerificationProfile,
    outcome: RequiredOutcome,
) -> CommitDecision {
    match (profile, outcome) {
        (VerificationProfile::None, _) | (_, RequiredOutcome::Pass) => CommitDecision::Commit,
        (_, RequiredOutcome::Fail) => CommitDecision::Abort,
    }
}

pub const fn postcommit_decision(outcome: RequiredOutcome) -> CommitDecision {
    match outcome {
        RequiredOutcome::Pass => CommitDecision::Commit,
        RequiredOutcome::Fail => CommitDecision::AlreadyCommittedWithFailure,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckResultStatus {
    Pass,
    Nonzero,
    Unavailable,
    Timeout,
    Cancelled,
    Rejected,
    NotQuiescent,
    ProtocolFailure,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckLaunchStatus {
    NotStarted,
    Launched,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationCheckResult {
    pub check_id: String,
    pub class: CheckClass,
    pub required: bool,
    pub status: CheckResultStatus,
    pub launch: CheckLaunchStatus,
    pub exit_code: Option<i32>,
    pub stdout_artifact: Option<String>,
    pub stdout_length: Option<u64>,
    pub stderr_artifact: Option<String>,
    pub stderr_length: Option<u64>,
    pub stdout_preview: Vec<u8>,
    pub stderr_preview: Vec<u8>,
    pub process_artifact: Option<String>,
    pub process_artifact_digest: Option<String>,
    pub process_artifact_length: Option<u64>,
    pub cancellation_evidence: Option<String>,
    pub quiescent: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct VerificationResultPayload {
    version: u16,
    profile: VerificationProfile,
    plan_digest: String,
    binding_digest: String,
    provenance: String,
    decision: CommitDecision,
    skipped: bool,
    accepted_failure: bool,
    checks: Vec<VerificationCheckResult>,
    quiescent: bool,
    evidence_digest: String,
    experiment_identity: String,
    experiment_digest: String,
    #[serde(default)]
    model_outcome: Option<crate::agent::adapters::grammar_edit::GrammarEditOutcomeEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VerificationResult {
    version: u16,
    profile: VerificationProfile,
    plan_digest: String,
    binding_digest: String,
    provenance: String,
    decision: CommitDecision,
    skipped: bool,
    accepted_failure: bool,
    checks: Vec<VerificationCheckResult>,
    quiescent: bool,
    evidence_digest: String,
    experiment_identity: String,
    experiment_digest: String,
    model_outcome: Option<crate::agent::adapters::grammar_edit::GrammarEditOutcomeEvidence>,
    result_digest: String,
    result_artifact: VerificationArtifactReference,
}

impl VerificationResult {
    fn persist_payload(
        mut payload: VerificationResultPayload,
        artifacts: &ArtifactStore,
        principal: &str,
        project: &str,
        retention: ArtifactRetention,
        stored_at_unix_micros: i64,
        secrets: &[SecretLease],
    ) -> Result<Self, VerificationError> {
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce).map_err(|_| VerificationError::RandomnessUnavailable)?;
        payload.provenance = digest(
            &serde_json::to_vec(&(
                nonce,
                &payload.binding_digest,
                &payload.plan_digest,
                principal,
                project,
            ))
            .map_err(|error| VerificationError::Canonicalization(error.to_string()))?,
        );
        payload.evidence_digest = evidence_digest(&payload.checks);
        let bytes = serde_json::to_vec(&payload)
            .map_err(|error| VerificationError::Canonicalization(error.to_string()))?;
        let sanitized = CaptureRedactor::new(secrets).sanitize(CaptureBoundary::Artifact, &bytes);
        if sanitized
            .bytes()
            .map_err(|_| VerificationError::UnsanitizedMetadata)?
            != bytes
        {
            return Err(VerificationError::UnsanitizedMetadata);
        }
        let artifact = artifacts
            .put(
                &bytes,
                ArtifactMetadata::new(
                    "application/json",
                    ArtifactClass::Report,
                    principal,
                    project,
                    retention,
                    stored_at_unix_micros,
                )
                .map_err(|error| VerificationError::ArtifactPersistence(error.to_string()))?,
            )
            .map_err(|error| VerificationError::ArtifactPersistence(error.to_string()))?;
        Ok(Self {
            version: payload.version,
            profile: payload.profile,
            plan_digest: payload.plan_digest,
            binding_digest: payload.binding_digest,
            provenance: payload.provenance,
            decision: payload.decision,
            skipped: payload.skipped,
            accepted_failure: payload.accepted_failure,
            checks: payload.checks,
            quiescent: payload.quiescent,
            evidence_digest: payload.evidence_digest,
            experiment_identity: payload.experiment_identity,
            experiment_digest: payload.experiment_digest,
            model_outcome: payload.model_outcome,
            result_digest: artifact.digest().to_string(),
            result_artifact: VerificationArtifactReference {
                reference: artifact.reference().to_string(),
                length: artifact.manifest().size,
            },
        })
    }

    fn payload(&self) -> VerificationResultPayload {
        VerificationResultPayload {
            version: self.version,
            profile: self.profile,
            plan_digest: self.plan_digest.clone(),
            binding_digest: self.binding_digest.clone(),
            provenance: self.provenance.clone(),
            decision: self.decision,
            skipped: self.skipped,
            accepted_failure: self.accepted_failure,
            checks: self.checks.clone(),
            quiescent: self.quiescent,
            evidence_digest: self.evidence_digest.clone(),
            experiment_identity: self.experiment_identity.clone(),
            experiment_digest: self.experiment_digest.clone(),
            model_outcome: self.model_outcome.clone(),
        }
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.payload()).expect("verification result serialization cannot fail")
    }

    pub fn verify_digest(&self) -> bool {
        valid_digest(&self.provenance)
            && self.evidence_digest == evidence_digest(&self.checks)
            && self.result_digest == digest(&self.canonical_bytes())
    }

    pub const fn profile(&self) -> VerificationProfile {
        self.profile
    }

    pub fn plan_digest(&self) -> &str {
        &self.plan_digest
    }

    pub fn binding_digest(&self) -> &str {
        &self.binding_digest
    }

    pub fn provenance(&self) -> &str {
        &self.provenance
    }

    pub const fn decision(&self) -> CommitDecision {
        self.decision
    }

    pub const fn skipped(&self) -> bool {
        self.skipped
    }

    pub const fn accepted_failure(&self) -> bool {
        self.accepted_failure
    }

    pub fn checks(&self) -> &[VerificationCheckResult] {
        &self.checks
    }

    pub const fn quiescent(&self) -> bool {
        self.quiescent
    }

    pub fn evidence_digest(&self) -> &str {
        &self.evidence_digest
    }

    pub fn result_digest(&self) -> &str {
        &self.result_digest
    }

    pub const fn result_artifact(&self) -> &VerificationArtifactReference {
        &self.result_artifact
    }

    pub fn receipt(&self) -> VerificationReceipt {
        let streams = |stdout: bool| {
            self.checks
                .iter()
                .filter_map(|check| {
                    let (reference, length) = if stdout {
                        (&check.stdout_artifact, check.stdout_length)
                    } else {
                        (&check.stderr_artifact, check.stderr_length)
                    };
                    Some(VerificationArtifactReference {
                        reference: reference.clone()?,
                        length: length?,
                    })
                })
                .collect()
        };
        let process_artifacts = self
            .checks
            .iter()
            .filter_map(|check| match check.launch {
                CheckLaunchStatus::NotStarted => Some(VerificationProcessReference::NotStarted {
                    check_id: check.check_id.clone(),
                }),
                CheckLaunchStatus::Launched => Some(VerificationProcessReference::Report {
                    check_id: check.check_id.clone(),
                    reference: check.process_artifact.clone()?,
                    digest: check.process_artifact_digest.clone()?,
                    length: check.process_artifact_length?,
                    cancellation_evidence: check.cancellation_evidence.clone(),
                }),
            })
            .collect::<Vec<_>>();
        VerificationReceipt {
            version: VERIFICATION_PROFILE_VERSION,
            schema_digest: verification_schema_digest(),
            plan_digest: self.plan_digest.clone(),
            binding_digest: self.binding_digest.clone(),
            provenance: self.provenance.clone(),
            result_digest: self.result_digest.clone(),
            evidence_digest: self.evidence_digest.clone(),
            experiment_identity: self.experiment_identity.clone(),
            experiment_digest: self.experiment_digest.clone(),
            model_outcome: self.model_outcome.clone(),
            result_artifact: self.result_artifact.clone(),
            stdout_artifacts: streams(true),
            stderr_artifacts: streams(false),
            process_artifacts,
            selected_check_count: self.checks.len(),
            process_artifact_count: self
                .checks
                .iter()
                .filter(|check| check.process_artifact.is_some())
                .count(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationArtifactReference {
    pub reference: String,
    pub length: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationReportReference {
    pub reference: String,
    pub digest: String,
    pub length: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum VerificationProcessReference {
    NotStarted {
        check_id: String,
    },
    Report {
        check_id: String,
        reference: String,
        digest: String,
        length: u64,
        cancellation_evidence: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationReceipt {
    pub version: u16,
    pub schema_digest: String,
    pub plan_digest: String,
    pub binding_digest: String,
    pub provenance: String,
    pub result_digest: String,
    pub evidence_digest: String,
    pub experiment_identity: String,
    pub experiment_digest: String,
    #[serde(default)]
    pub model_outcome: Option<crate::agent::adapters::grammar_edit::GrammarEditOutcomeEvidence>,
    pub result_artifact: VerificationArtifactReference,
    pub stdout_artifacts: Vec<VerificationArtifactReference>,
    pub stderr_artifacts: Vec<VerificationArtifactReference>,
    pub process_artifacts: Vec<VerificationProcessReference>,
    pub selected_check_count: usize,
    pub process_artifact_count: usize,
}

impl VerificationReceipt {
    pub fn validate_artifacts(
        &self,
        artifacts: &ArtifactStore,
        principal: &str,
        project: &str,
    ) -> Result<(), VerificationError> {
        if self.version != VERIFICATION_PROFILE_VERSION
            || self.schema_digest != verification_schema_digest()
            || self.experiment_identity != crate::domain::config::GRAMMAR_EDIT_EXPERIMENT_ID
            || !valid_sha256_digest(&self.experiment_digest)
            || [
                &self.plan_digest,
                &self.binding_digest,
                &self.provenance,
                &self.result_digest,
                &self.evidence_digest,
            ]
            .into_iter()
            .any(|value| !valid_digest(value))
        {
            return Err(VerificationError::InvalidReceipt);
        }
        let result_reference = ArtifactReference::parse(&self.result_artifact.reference)
            .map_err(|_| VerificationError::InvalidReceipt)?;
        let result_artifact = artifacts
            .open_reference(result_reference)
            .map_err(|_| VerificationError::InvalidReceipt)?;
        if result_artifact.manifest().size != self.result_artifact.length
            || result_artifact.digest().to_string() != self.result_digest
            || result_artifact.manifest().class != ArtifactClass::Report
            || result_artifact.manifest().principal != principal
            || result_artifact.manifest().project != project
        {
            return Err(VerificationError::InvalidReceipt);
        }
        let result_bytes = artifacts
            .open_bytes_bounded(
                result_artifact.digest(),
                usize::try_from(self.result_artifact.length)
                    .map_err(|_| VerificationError::InvalidReceipt)?,
            )
            .map_err(|_| VerificationError::InvalidReceipt)?;
        let payload: VerificationResultPayload =
            serde_json::from_slice(&result_bytes).map_err(|_| VerificationError::InvalidReceipt)?;
        let canonical_result =
            serde_json::to_vec(&payload).map_err(|_| VerificationError::InvalidReceipt)?;
        if result_bytes.len() as u64 != self.result_artifact.length
            || canonical_result != result_bytes
            || payload.version != VERIFICATION_PROFILE_VERSION
            || payload.plan_digest != self.plan_digest
            || payload.binding_digest != self.binding_digest
            || payload.provenance != self.provenance
            || payload.evidence_digest != self.evidence_digest
            || payload.experiment_identity != self.experiment_identity
            || payload.experiment_digest != self.experiment_digest
            || payload.model_outcome != self.model_outcome
            || evidence_digest(&payload.checks) != self.evidence_digest
            || !valid_payload(&payload)
        {
            return Err(VerificationError::InvalidReceipt);
        }
        let expected_streams = |stdout: bool| {
            payload
                .checks
                .iter()
                .filter_map(|check| {
                    let (reference, length) = if stdout {
                        (&check.stdout_artifact, check.stdout_length)
                    } else {
                        (&check.stderr_artifact, check.stderr_length)
                    };
                    Some(VerificationArtifactReference {
                        reference: reference.clone()?,
                        length: length?,
                    })
                })
                .collect::<Vec<_>>()
        };
        if self.stdout_artifacts != expected_streams(true)
            || self.stderr_artifacts != expected_streams(false)
        {
            return Err(VerificationError::InvalidReceipt);
        }
        for stream in self.stdout_artifacts.iter().chain(&self.stderr_artifacts) {
            let reference = ArtifactReference::parse(&stream.reference)
                .map_err(|_| VerificationError::InvalidReceipt)?;
            let artifact = artifacts
                .open_reference(reference)
                .map_err(|_| VerificationError::InvalidReceipt)?;
            if artifact.manifest().size != stream.length
                || artifact.manifest().class != ArtifactClass::Log
                || artifact.manifest().principal != principal
                || artifact.manifest().project != project
            {
                return Err(VerificationError::InvalidReceipt);
            }
        }
        let expected_process_artifacts =
            process_references(&payload.checks).ok_or(VerificationError::InvalidReceipt)?;
        if self.selected_check_count != payload.checks.len()
            || self.process_artifacts.len() != self.selected_check_count
            || self.process_artifacts != expected_process_artifacts
            || self.process_artifact_count
                != self
                    .process_artifacts
                    .iter()
                    .filter(|report| matches!(report, VerificationProcessReference::Report { .. }))
                    .count()
        {
            return Err(VerificationError::InvalidReceipt);
        }
        let mut references = BTreeSet::new();
        for report in &self.process_artifacts {
            let VerificationProcessReference::Report {
                reference,
                digest: report_digest,
                length,
                cancellation_evidence,
                ..
            } = report
            else {
                continue;
            };
            if !valid_digest(report_digest) || !references.insert(reference) {
                return Err(VerificationError::InvalidReceipt);
            }
            let reference = ArtifactReference::parse(reference)
                .map_err(|_| VerificationError::InvalidReceipt)?;
            let artifact = artifacts
                .open_reference(reference)
                .map_err(|_| VerificationError::InvalidReceipt)?;
            if artifact.manifest().size != *length
                || artifact.digest().to_string() != *report_digest
                || artifact.manifest().class != ArtifactClass::Report
                || artifact.manifest().principal != principal
                || artifact.manifest().project != project
            {
                return Err(VerificationError::InvalidReceipt);
            }
            let max_bytes =
                usize::try_from(*length).map_err(|_| VerificationError::InvalidReceipt)?;
            let bytes = artifacts
                .open_bytes_bounded(artifact.digest(), max_bytes)
                .map_err(|_| VerificationError::InvalidReceipt)?;
            let process = serde_json::from_slice::<CheckProcessEvidence>(&bytes)
                .map_err(|_| VerificationError::InvalidReceipt)?;
            if bytes.len() as u64 != *length
                || !validate_process_evidence_report(&bytes)
                || process.cancellation_record != *cancellation_evidence
                || !process.quiescent
                || !process.boundary_absent
                || !process.reaped
                || !process.inspected
                || process.survivors != 0
            {
                return Err(VerificationError::InvalidReceipt);
            }
        }
        Ok(())
    }
}

fn verification_schema_digest() -> String {
    digest(b"kit-verification-receipt-schema-v6")
}

fn evidence_digest(checks: &[VerificationCheckResult]) -> String {
    digest(&serde_json::to_vec(checks).expect("verification evidence serialization cannot fail"))
}

fn process_references(
    checks: &[VerificationCheckResult],
) -> Option<Vec<VerificationProcessReference>> {
    checks
        .iter()
        .map(|check| match check.launch {
            CheckLaunchStatus::NotStarted => {
                if check.process_artifact.is_some()
                    || check.process_artifact_digest.is_some()
                    || check.process_artifact_length.is_some()
                    || check.cancellation_evidence.is_some()
                {
                    return None;
                }
                Some(VerificationProcessReference::NotStarted {
                    check_id: check.check_id.clone(),
                })
            }
            CheckLaunchStatus::Launched => Some(VerificationProcessReference::Report {
                check_id: check.check_id.clone(),
                reference: check.process_artifact.clone()?,
                digest: check.process_artifact_digest.clone()?,
                length: check.process_artifact_length?,
                cancellation_evidence: check.cancellation_evidence.clone(),
            }),
        })
        .collect()
}

fn valid_payload(payload: &VerificationResultPayload) -> bool {
    if !valid_digest(&payload.plan_digest)
        || !valid_digest(&payload.binding_digest)
        || !valid_digest(&payload.provenance)
        || !valid_digest(&payload.evidence_digest)
        || payload.experiment_identity != crate::domain::config::GRAMMAR_EDIT_EXPERIMENT_ID
        || !valid_sha256_digest(&payload.experiment_digest)
        || payload.model_outcome.as_ref().is_some_and(|outcome| {
            outcome.intent.experiment_identity != payload.experiment_identity
                || outcome.intent.experiment_digest != payload.experiment_digest
                || !crate::agent::adapters::grammar_edit::valid_outcome_evidence(outcome)
        })
        || payload.skipped != (payload.profile == VerificationProfile::None)
        || payload.skipped && !payload.checks.is_empty()
        || payload.quiescent != payload.checks.iter().all(|check| check.quiescent)
    {
        return false;
    }
    let mut previous = None;
    for check in &payload.checks {
        let key = (check.class, check.check_id.as_str());
        if check.check_id.is_empty() || previous.is_some_and(|value| value >= key) {
            return false;
        }
        previous = Some(key);
        if check.stdout_artifact.is_some() != check.stdout_length.is_some()
            || check.stderr_artifact.is_some() != check.stderr_length.is_some()
        {
            return false;
        }
        if check.launch == CheckLaunchStatus::Launched
            && (check.process_artifact.is_none()
                || check.process_artifact_digest.is_none()
                || check.process_artifact_length.is_none())
        {
            return false;
        }
    }
    true
}

pub(crate) fn verify_precommit(
    input: StagedVerificationInput<'_>,
    mut request: VerificationRequest<'_>,
) -> Result<VerificationResult, VerificationError> {
    let source_digest = immutable_tree_digest(input.immutable_source)
        .map_err(|_| VerificationError::StaleBinding)?;
    let model_outcome = request.model_outcome.cloned();
    let (experiment_identity, experiment_digest) = model_outcome.as_ref().map_or_else(
        || {
            (
                crate::domain::config::GRAMMAR_EDIT_EXPERIMENT_ID.to_owned(),
                request.config.grammar_edit_experiment_digest(),
            )
        },
        |outcome| {
            (
                outcome.intent.experiment_identity.clone(),
                outcome.intent.experiment_digest.clone(),
            )
        },
    );
    let binding = VerificationBinding {
        revision: input.revision.to_owned(),
        epoch: input.epoch.to_owned(),
        revision_digest: input.revision_digest.to_owned(),
        guard_binding: format!(
            "blake3:{}",
            blake3::Hash::from_bytes(*input.guard_binding).to_hex()
        ),
        root_identity: input.root_identity.to_owned(),
        plan_digest: input.plan_digest.to_owned(),
        state_digest: input.state_digest.to_owned(),
        evidence_digest: input.evidence_digest.to_owned(),
        changes_digest: input.changes_digest.to_owned(),
        syntax_evidence_digest: input.syntax_evidence_digest.to_owned(),
        source_digest,
        experiment_identity,
        experiment_digest,
        model_outcome,
    };
    let binding_digest = binding.digest();
    let policy = mint_policy(&input, &request)?;
    let checks = select_checks(
        &request.selection,
        request.registry,
        &input.changed_paths,
        &policy,
    )?;
    let profile = request.selection.profile();
    if request.on_check_failure == CheckFailureBehavior::Commit
        && (matches!(
            profile,
            VerificationProfile::Syntax | VerificationProfile::Fast
        ) || checks.iter().any(|check| check.requirement.required()))
    {
        return Err(VerificationError::RequiredFailureCannotCommit);
    }
    let spec = PlanSpec {
        version: VERIFICATION_PROFILE_VERSION,
        profile,
        binding,
        changed_paths: input.changed_paths.clone(),
        checks,
        authority_id: policy.authority_id,
        max_checks: policy.budget.max_checks,
        max_preview_bytes: policy.budget.max_preview_bytes,
        on_check_failure: request.on_check_failure,
    };
    let digest = digest(
        &serde_json::to_vec(&spec)
            .map_err(|error| VerificationError::Canonicalization(error.to_string()))?,
    );
    let plan = VerificationPlan { spec, digest };
    run_plan(
        &plan,
        request.runner.take(),
        request.observer.take(),
        &input,
        request.artifacts,
        request.secrets,
        policy.retention,
        policy.stored_at_unix_micros,
        binding_digest,
        request.cancellation,
    )
}

fn mint_policy(
    input: &StagedVerificationInput<'_>,
    request: &VerificationRequest<'_>,
) -> Result<PolicyContext, VerificationError> {
    let authenticated = request.authenticated;
    let grants = request.grants;
    let config = request.config;
    if authenticated.grant_snapshot() != grants
        || authenticated.principal_id() != grants.principal_id()
        || grants.principal_id() != config.principal_id()
        || grants.project_id() != config.project_id()
        || input.authority_principal != grants.principal_id().to_string()
        || input.authority_project != grants.project_id().to_string()
        || (!grants.grants().contains(&Grant::WorkspaceWrite)
            && !grants.grants().contains(&Grant::VerificationTargeted))
    {
        return Err(VerificationError::AuthorityRequired);
    }
    let effective = config.effective_authority();
    let authorized = |grant| grants.grants().contains(&grant) && effective.contains(&grant);
    let max_checks = usize::try_from(config.effective().max_concurrent_tools)
        .unwrap_or(usize::MAX)
        .min(MAX_CHECKS);
    if max_checks == 0 {
        return Err(VerificationError::BudgetExceeded);
    }
    let days = i64::from(config.effective().artifact_retention_days);
    let day = 86_400_000_000_i64;
    let stored_at_unix_micros = now_unix_micros()
        .map_err(|_| VerificationError::BudgetExceeded)?
        .div_euclid(day)
        * day;
    let retention = days
        .checked_mul(day)
        .and_then(|ttl| stored_at_unix_micros.checked_add(ttl))
        .map(ArtifactRetention::UntilUnixMicros)
        .ok_or(VerificationError::BudgetExceeded)?;
    Ok(PolicyContext {
        authority_id: digest(
            &[
                grants.principal_id().to_string().as_bytes(),
                grants.project_id().to_string().as_bytes(),
                &config.digest(),
            ]
            .concat(),
        ),
        process_spawn: authorized(Grant::ProcessSpawn),
        targeted: authorized(Grant::VerificationTargeted),
        full: authorized(Grant::VerificationFull),
        budget: VerificationBudget {
            max_checks,
            max_preview_bytes: MAX_PREVIEW_BYTES,
            aggregate: hard_resource_ceiling(),
        },
        retention,
        stored_at_unix_micros,
    })
}

fn select_checks(
    selection: &ProfileSelection,
    registry: &VerificationRegistry,
    changed_paths: &BTreeSet<String>,
    policy: &PolicyContext,
) -> Result<Vec<DeclaredCheck>, VerificationError> {
    if changed_paths.iter().any(|path| !valid_path(path)) {
        return Err(VerificationError::InvalidBinding);
    }
    let checks = match selection {
        ProfileSelection::None => Vec::new(),
        ProfileSelection::Syntax => registry
            .checks
            .iter()
            .filter(|check| check.class == CheckClass::Syntax && check.affected(changed_paths))
            .cloned()
            .collect(),
        ProfileSelection::Fast => registry
            .checks
            .iter()
            .filter(|check| {
                matches!(
                    check.class,
                    CheckClass::Syntax | CheckClass::Diagnostics | CheckClass::Typecheck
                ) && check.affected(changed_paths)
            })
            .cloned()
            .collect(),
        ProfileSelection::Targeted { exact_targets } => {
            if !policy.targeted || exact_targets.is_empty() {
                return Err(VerificationError::AuthorityRequired);
            }
            let declared = registry
                .checks
                .iter()
                .filter(|check| check.class == CheckClass::Targeted)
                .map(|check| (check.command.id(), check))
                .collect::<BTreeMap<_, _>>();
            let mut selected = registry
                .checks
                .iter()
                .filter(|check| {
                    check.requirement.required()
                        && matches!(
                            check.class,
                            CheckClass::Syntax | CheckClass::Diagnostics | CheckClass::Typecheck
                        )
                        && check.affected(changed_paths)
                })
                .cloned()
                .collect::<Vec<_>>();
            selected.extend(
                exact_targets
                    .iter()
                    .map(|id| {
                        declared
                            .get(id.as_str())
                            .cloned()
                            .cloned()
                            .ok_or_else(|| VerificationError::UnknownCheck(id.clone()))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            );
            selected
        }
        ProfileSelection::Full => {
            if !policy.full {
                return Err(VerificationError::AuthorityRequired);
            }
            registry.checks.clone()
        }
    };
    let mut checks = checks;
    checks.sort_by(check_order);
    if !checks.is_empty() && !policy.process_spawn {
        return Err(VerificationError::AuthorityRequired);
    }
    if checks.len() > policy.budget.max_checks || !within_budget(&checks, policy.budget.aggregate) {
        return Err(VerificationError::BudgetExceeded);
    }
    Ok(checks)
}

#[allow(clippy::too_many_arguments)]
fn run_plan(
    plan: &VerificationPlan,
    mut runner: Option<&mut CheckRunner>,
    mut observer: Option<&mut dyn VerificationObserver>,
    input: &StagedVerificationInput<'_>,
    artifacts: &ArtifactStore,
    secrets: &[SecretLease],
    retention: ArtifactRetention,
    stored_at_unix_micros: i64,
    binding_digest: String,
    cancellation: Option<&AtomicBool>,
) -> Result<VerificationResult, VerificationError> {
    let mut failed_required = false;
    let mut accepted_failure = false;
    let mut results = Vec::with_capacity(plan.spec.checks.len());
    for (index, check) in plan.spec.checks.iter().enumerate() {
        observe(
            &mut observer,
            plan,
            check,
            VerificationBoundary::Started,
            None,
        )?;
        let result = if request_cancelled(cancellation) {
            Err(CheckExecutionFailure {
                state: CheckFailureState::NotStarted(CheckExecutorError::Cancelled),
            })
        } else {
            match runner.as_deref_mut() {
                Some(runner) => runner.execute(CheckExecutionRequest {
                    command: &check.command,
                    immutable_source: input.immutable_source,
                    source_digest: &plan.spec.binding.source_digest,
                    build: input.build,
                    temp: input.temp,
                    max_preview_bytes: plan.spec.max_preview_bytes,
                    artifacts,
                    principal: input.authority_principal,
                    project: input.authority_project,
                    retention,
                    stored_at_unix_micros,
                    secrets,
                    more_boundaries: index + 1 < plan.spec.checks.len()
                        || input.more_boundaries_after,
                }),
                None => Err(CheckExecutionFailure {
                    state: CheckFailureState::NotStarted(CheckExecutorError::Unavailable),
                }),
            }
        };
        observe(
            &mut observer,
            plan,
            check,
            VerificationBoundary::Progress,
            None,
        )?;
        let result = check_result(check, result);
        let boundary = if result.status == CheckResultStatus::Pass {
            VerificationBoundary::Completed
        } else {
            VerificationBoundary::Failure
        };
        observe(&mut observer, plan, check, boundary, Some(result.status))?;
        let failed = result.status != CheckResultStatus::Pass;
        failed_required |= failed && check.requirement.required();
        accepted_failure |= failed && !check.requirement.required();
        results.push(result);
    }
    let decision = if failed_required
        || accepted_failure && plan.spec.on_check_failure == CheckFailureBehavior::Abort
    {
        CommitDecision::Abort
    } else {
        CommitDecision::Commit
    };
    let quiescent = results.iter().all(|result| result.quiescent);
    VerificationResult::persist_payload(
        VerificationResultPayload {
            version: VERIFICATION_PROFILE_VERSION,
            profile: plan.spec.profile,
            plan_digest: plan.digest.clone(),
            binding_digest,
            provenance: String::new(),
            decision,
            skipped: plan.spec.profile == VerificationProfile::None,
            accepted_failure: decision == CommitDecision::Commit && accepted_failure,
            checks: results,
            quiescent,
            evidence_digest: String::new(),
            experiment_identity: plan.spec.binding.experiment_identity.to_owned(),
            experiment_digest: plan.spec.binding.experiment_digest.clone(),
            model_outcome: plan.spec.binding.model_outcome.clone(),
        },
        artifacts,
        input.authority_principal,
        input.authority_project,
        retention,
        stored_at_unix_micros,
        secrets,
    )
}

fn request_cancelled(cancellation: Option<&AtomicBool>) -> bool {
    cancellation.is_some_and(|signal| signal.load(Ordering::Acquire))
}

fn observe(
    observer: &mut Option<&mut dyn VerificationObserver>,
    plan: &VerificationPlan,
    check: &DeclaredCheck,
    boundary: VerificationBoundary,
    status: Option<CheckResultStatus>,
) -> Result<(), VerificationError> {
    if let Some(observer) = observer.as_deref_mut() {
        observer.observe(VerificationObservation {
            plan_digest: &plan.digest,
            check_id: check.command.id(),
            boundary,
            status,
        })?;
    }
    Ok(())
}

fn check_result(
    check: &DeclaredCheck,
    result: Result<CheckCompletion, CheckExecutionFailure>,
) -> VerificationCheckResult {
    let mut checked = VerificationCheckResult {
        check_id: check.command.id().to_owned(),
        class: check.class,
        required: check.requirement.required(),
        status: CheckResultStatus::Unavailable,
        launch: CheckLaunchStatus::NotStarted,
        exit_code: None,
        stdout_artifact: None,
        stdout_length: None,
        stderr_artifact: None,
        stderr_length: None,
        stdout_preview: Vec::new(),
        stderr_preview: Vec::new(),
        process_artifact: None,
        process_artifact_digest: None,
        process_artifact_length: None,
        cancellation_evidence: None,
        quiescent: false,
    };
    match result {
        Ok(completion) => {
            checked.launch = CheckLaunchStatus::Launched;
            checked.status = match completion.status {
                CheckStatus::Pass => CheckResultStatus::Pass,
                CheckStatus::Exit(code) => {
                    checked.exit_code = Some(code);
                    CheckResultStatus::Nonzero
                }
            };
            checked.stdout_artifact = Some(completion.stdout_artifact.reference().to_owned());
            checked.stdout_length = Some(completion.stdout_artifact.length());
            checked.stderr_artifact = Some(completion.stderr_artifact.reference().to_owned());
            checked.stderr_length = Some(completion.stderr_artifact.length());
            checked.stdout_preview = completion.stdout_preview;
            checked.stderr_preview = completion.stderr_preview;
            checked.process_artifact = Some(completion.process_artifact.reference().to_owned());
            checked.process_artifact_digest = Some(completion.process_artifact.digest().to_owned());
            checked.process_artifact_length = Some(completion.process_artifact.length());
            checked.cancellation_evidence = completion.process.cancellation_record;
            checked.quiescent = completion.process.quiescent
                && completion.process.boundary_absent
                && completion.process.survivors == 0;
            if !checked.quiescent {
                checked.status = CheckResultStatus::NotQuiescent;
            }
        }
        Err(failure) => match failure.state {
            CheckFailureState::NotStarted(kind) => {
                checked.status = failure_status(kind);
            }
            CheckFailureState::LaunchedFailure {
                kind,
                process,
                process_artifact,
            } => {
                checked.launch = CheckLaunchStatus::Launched;
                let evidence_valid = process.as_ref().is_some_and(|process| {
                    process.quiescent
                        && process.boundary_absent
                        && process.reaped
                        && process.inspected
                        && process.survivors == 0
                }) && process_artifact.is_some();
                checked.process_artifact = process_artifact
                    .as_ref()
                    .map(|artifact| artifact.reference().to_owned());
                checked.process_artifact_digest = process_artifact
                    .as_ref()
                    .map(|artifact| artifact.digest().to_owned());
                checked.process_artifact_length =
                    process_artifact.as_ref().map(CheckArtifactRef::length);
                checked.cancellation_evidence = process
                    .as_ref()
                    .and_then(|process| process.cancellation_record.clone());
                checked.quiescent = evidence_valid;
                let cancellation_valid = !matches!(
                    kind,
                    CheckExecutorError::Timeout | CheckExecutorError::Cancelled
                ) || process
                    .as_ref()
                    .is_some_and(|process| process.cancellation.is_some());
                checked.quiescent &= cancellation_valid;
                checked.status = if evidence_valid && cancellation_valid {
                    failure_status(kind)
                } else {
                    CheckResultStatus::NotQuiescent
                };
            }
        },
    }
    checked
}

fn failure_status(kind: CheckExecutorError) -> CheckResultStatus {
    match kind {
        CheckExecutorError::Unavailable | CheckExecutorError::Io(_) => {
            CheckResultStatus::Unavailable
        }
        CheckExecutorError::Timeout => CheckResultStatus::Timeout,
        CheckExecutorError::Cancelled => CheckResultStatus::Cancelled,
        CheckExecutorError::Rejected | CheckExecutorError::StaleTree => CheckResultStatus::Rejected,
        CheckExecutorError::NotQuiescent => CheckResultStatus::NotQuiescent,
        CheckExecutorError::OutputLimit | CheckExecutorError::Protocol => {
            CheckResultStatus::ProtocolFailure
        }
    }
}

#[allow(dead_code)]
pub(crate) fn validate_postcommit_policy(
    behavior: CheckFailureBehavior,
    checks: &[DeclaredCheck],
) -> Result<(), VerificationError> {
    if behavior == CheckFailureBehavior::Abort
        || checks
            .iter()
            .any(|check| check.requirement.required() || !check.post_commit_safe)
    {
        Err(VerificationError::UnsafePostCommitPolicy)
    } else {
        Ok(())
    }
}

fn check_order(left: &DeclaredCheck, right: &DeclaredCheck) -> std::cmp::Ordering {
    (left.class, left.command.id()).cmp(&(right.class, right.command.id()))
}

fn hard_resource_ceiling() -> ResourceLimits {
    ResourceLimits::new(
        64 * 60 * 1_000,
        16 * 1024 * 1024 * 1024,
        4096,
        4 * 1024 * 1024 * 1024,
        16 * 1024 * 1024 * 1024,
        16 * 1024 * 1024 * 1024,
        512 * 1024 * 1024,
        64 * 60 * 1_000,
    )
}

fn within_budget(checks: &[DeclaredCheck], budget: ResourceLimits) -> bool {
    let mut used = [0_u64; 8];
    for check in checks {
        let resources = check.cost();
        for (slot, value) in used.iter_mut().zip([
            resources.cpu_millis,
            resources.memory_bytes,
            u64::from(resources.pids),
            resources.file_bytes,
            resources.disk_bytes,
            resources.io_bytes,
            resources.output_bytes,
            resources.wall_time_millis,
        ]) {
            let Some(sum) = slot.checked_add(value) else {
                return false;
            };
            *slot = sum;
        }
    }
    used.iter()
        .zip([
            budget.cpu_millis,
            budget.memory_bytes,
            u64::from(budget.pids),
            budget.file_bytes,
            budget.disk_bytes,
            budget.io_bytes,
            budget.output_bytes,
            budget.wall_time_millis,
        ])
        .all(|(used, limit)| *used <= limit)
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.is_ascii()
        && !path.starts_with('/')
        && !path.contains(['\\', ':'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && !matches!(part, "." | ".."))
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("blake3:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn digest(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerificationError {
    InvalidBinding,
    StaleBinding,
    DuplicateCheck,
    UnknownCheck(String),
    AuthorityRequired,
    UnsafePostCommitPolicy,
    RequiredFailureCannotCommit,
    BudgetExceeded,
    MaliciousProjectConfiguration,
    Canonicalization(String),
    ArtifactPersistence(String),
    RandomnessUnavailable,
    Observer(String),
    UnsanitizedMetadata,
    InvalidReceipt,
}

impl fmt::Display for VerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBinding => formatter.write_str("verification input binding is invalid"),
            Self::StaleBinding => formatter.write_str("verification stage binding is stale"),
            Self::DuplicateCheck => formatter.write_str("project check IDs are not unique"),
            Self::UnknownCheck(check) => write!(formatter, "unknown explicit check {check}"),
            Self::AuthorityRequired => {
                formatter.write_str("explicit verification authority is required")
            }
            Self::UnsafePostCommitPolicy => {
                formatter.write_str("post-commit verification cannot abort or run required checks")
            }
            Self::RequiredFailureCannotCommit => {
                formatter.write_str("a failing required check cannot be configured to commit")
            }
            Self::BudgetExceeded => {
                formatter.write_str("verification plan exceeds its hard budget")
            }
            Self::MaliciousProjectConfiguration => {
                formatter.write_str("unsafe project verification declaration")
            }
            Self::Canonicalization(error) => {
                write!(formatter, "verification canonicalization failed: {error}")
            }
            Self::ArtifactPersistence(error) => {
                write!(formatter, "verification result persistence failed: {error}")
            }
            Self::RandomnessUnavailable => {
                formatter.write_str("verification result provenance randomness unavailable")
            }
            Self::Observer(error) => write!(formatter, "verification observer failed: {error}"),
            Self::UnsanitizedMetadata => {
                formatter.write_str("verification metadata contains secret material")
            }
            Self::InvalidReceipt => formatter.write_str("verification receipt is invalid"),
        }
    }
}

impl std::error::Error for VerificationError {}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::PathBuf,
        sync::Arc,
        sync::atomic::{AtomicU64, Ordering},
    };

    use crate::{
        api::auth::contract::{AuthenticatedPrincipal, GrantSnapshot},
        domain::{
            config::{Grant, LayerStack, RunConfigContext},
            ids::{DaemonServiceId, PrincipalId, ProcessId, ProjectId, RunId},
            lifecycle::{ProcessClaim, ProcessOwnership},
            secret::SecretLease,
        },
        executor::{
            check::{CheckCommand, CheckRunner, ConformanceCheck},
            process::{
                own::{
                    ProcessRecord, ProcessRegistrationContext, ProcessRegistry,
                    ProcessRegistryRegistration, ProcessTerminalConfig,
                },
                tree::PersistedBoundary,
            },
            profile::ResourceLimits,
        },
        store::artifacts::ArtifactStore,
    };

    use super::*;

    const IMAGE: &str = "example.invalid/check@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TOOL: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CONFIG: &str = "blake3:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        source: PathBuf,
        build: PathBuf,
        temp: PathBuf,
        artifacts: ArtifactStore,
        authenticated: AuthenticatedPrincipal,
        grants: GrantSnapshot,
        config: RunConfigSnapshot,
        principal: String,
        project: String,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "kit-verify-unit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let source = root.join("source");
            let build = root.join("build");
            let temp = root.join("temp");
            fs::create_dir_all(source.join("src")).unwrap();
            fs::create_dir(&build).unwrap();
            fs::create_dir(&temp).unwrap();
            fs::write(source.join("src/lib.rs"), b"pub fn checked() {}\n").unwrap();
            let artifacts = ArtifactStore::open(root.join("artifacts")).unwrap();
            let principal_id = PrincipalId::generate().unwrap();
            let project_id = ProjectId::generate().unwrap();
            let authority = [
                Grant::WorkspaceWrite,
                Grant::ProcessSpawn,
                Grant::VerificationTargeted,
                Grant::VerificationFull,
            ];
            let grants = GrantSnapshot::new(principal_id, project_id, authority);
            let authenticated = AuthenticatedPrincipal::from_grants(grants.clone());
            let mut layers = LayerStack::safe_defaults();
            layers
                .built_in
                .grants
                .as_mut()
                .unwrap()
                .insert(Grant::VerificationFull);
            let config = layers
                .materialize(
                    RunConfigContext {
                        principal_id,
                        project_id,
                        run_id: RunId::generate().unwrap(),
                    },
                    grants.grants(),
                )
                .unwrap();
            Self {
                root,
                source,
                build,
                temp,
                artifacts,
                authenticated,
                grants,
                config,
                principal: principal_id.to_string(),
                project: project_id.to_string(),
            }
        }

        fn input(&self) -> StagedVerificationInput<'_> {
            StagedVerificationInput {
                revision: "r:0123456789abcdef0123456789abcdef",
                epoch: "e:0123456789abcdef0123456789abcdef",
                revision_digest: "blake3:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                guard_binding: &[7; 32],
                root_identity: "1:2",
                plan_digest: "blake3:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                state_digest: "blake3:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                evidence_digest: "blake3:1111111111111111111111111111111111111111111111111111111111111111",
                changes_digest: "blake3:2222222222222222222222222222222222222222222222222222222222222222",
                syntax_evidence_digest: "blake3:3333333333333333333333333333333333333333333333333333333333333333",
                changed_paths: BTreeSet::from(["src/lib.rs".to_owned()]),
                immutable_source: &self.source,
                build: &self.build,
                temp: &self.temp,
                authority_principal: &self.principal,
                authority_project: &self.project,
                more_boundaries_after: false,
            }
        }

        fn request<'a>(
            &'a self,
            selection: ProfileSelection,
            registry: &'a VerificationRegistry,
            runner: &'a mut CheckRunner,
            secrets: &'a [SecretLease],
        ) -> VerificationRequest<'a> {
            VerificationRequest {
                selection,
                registry,
                authenticated: &self.authenticated,
                grants: &self.grants,
                config: &self.config,
                runner: Some(runner),
                observer: None,
                artifacts: &self.artifacts,
                secrets,
                on_check_failure: CheckFailureBehavior::Abort,
                model_outcome: None,
                cancellation: None,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn command(id: &str) -> CheckCommand {
        CheckCommand::new(
            id,
            "/usr/bin/cargo",
            vec!["check".to_owned(), "--locked".to_owned()],
            IMAGE,
            TOOL,
            CONFIG,
            ResourceLimits::new(
                1_000,
                64 << 20,
                16,
                8 << 20,
                64 << 20,
                64 << 20,
                1 << 20,
                10_000,
            ),
        )
        .unwrap()
    }

    fn registry(
        class: CheckClass,
        id: &str,
        requirement: CheckRequirement,
    ) -> VerificationRegistry {
        VerificationRegistry::new(vec![
            DeclaredCheck::new(
                class,
                command(id),
                requirement,
                BTreeSet::from(["src".to_owned()]),
                false,
            )
            .unwrap(),
        ])
        .unwrap()
    }

    fn selection(profile: VerificationProfile, id: &str) -> ProfileSelection {
        match profile {
            VerificationProfile::None => ProfileSelection::None,
            VerificationProfile::Syntax => ProfileSelection::Syntax,
            VerificationProfile::Fast => ProfileSelection::Fast,
            VerificationProfile::Targeted => ProfileSelection::Targeted {
                exact_targets: BTreeSet::from([id.to_owned()]),
            },
            VerificationProfile::Full => ProfileSelection::Full,
        }
    }

    #[test]
    fn all_ten_profile_cells_follow_the_declared_commit_matrix() {
        let fixture = Fixture::new();
        let mut cells = 0;
        let mut provenances = BTreeSet::new();
        for profile in VerificationProfile::ALL {
            for outcome in [RequiredOutcome::Pass, RequiredOutcome::Fail] {
                let id = format!("check-{profile:?}-{outcome:?}").to_ascii_lowercase();
                let class = match profile {
                    VerificationProfile::None => CheckClass::Full,
                    VerificationProfile::Syntax => CheckClass::Syntax,
                    VerificationProfile::Fast => CheckClass::Diagnostics,
                    VerificationProfile::Targeted => CheckClass::Targeted,
                    VerificationProfile::Full => CheckClass::Full,
                };
                let registry = if profile == VerificationProfile::None {
                    VerificationRegistry::empty()
                } else {
                    registry(class, &id, CheckRequirement::Required)
                };
                let output = format!("{id}-{outcome:?}").into_bytes();
                let action = match outcome {
                    RequiredOutcome::Pass => ConformanceCheck::pass(output, Vec::new()),
                    RequiredOutcome::Fail => ConformanceCheck::exit(1, Vec::new(), output),
                };
                let mut runner = CheckRunner::conformance([action]);
                let result = verify_precommit(
                    fixture.input(),
                    fixture.request(selection(profile, &id), &registry, &mut runner, &[]),
                )
                .unwrap();
                assert_eq!(result.decision, declared_decision(profile, outcome));
                assert!(result.verify_digest());
                assert!(provenances.insert(result.provenance().to_owned()));
                cells += 1;
            }
        }
        assert_eq!(cells, 10);
        assert_eq!(provenances.len(), 10);
    }

    #[test]
    fn observer_runs_at_each_check_execution_boundary() {
        #[derive(Default)]
        struct RecordingObserver(Vec<(String, VerificationBoundary, Option<CheckResultStatus>)>);

        impl VerificationObserver for RecordingObserver {
            fn observe(
                &mut self,
                observation: VerificationObservation<'_>,
            ) -> Result<(), VerificationError> {
                self.0.push((
                    observation.check_id.to_owned(),
                    observation.boundary,
                    observation.status,
                ));
                Ok(())
            }
        }

        let fixture = Fixture::new();
        let registry = registry(
            CheckClass::Diagnostics,
            "observed",
            CheckRequirement::Required,
        );
        let mut runner = CheckRunner::conformance([ConformanceCheck::pass(b"", b"")]);
        let mut observer = RecordingObserver::default();
        let mut request = fixture.request(ProfileSelection::Fast, &registry, &mut runner, &[]);
        request.observer = Some(&mut observer);
        verify_precommit(fixture.input(), request).unwrap();
        assert_eq!(
            observer.0,
            [
                ("observed".into(), VerificationBoundary::Started, None),
                ("observed".into(), VerificationBoundary::Progress, None),
                (
                    "observed".into(),
                    VerificationBoundary::Completed,
                    Some(CheckResultStatus::Pass),
                ),
            ]
        );
    }

    #[test]
    fn full_stream_artifacts_are_independently_redacted_and_verified() {
        let fixture = Fixture::new();
        let secret = b"KIT_CHECK_CANARY";
        let leases = [SecretLease::new(secret.to_vec())];
        let registry = registry(
            CheckClass::Diagnostics,
            "redact",
            CheckRequirement::Required,
        );
        let stdout = [b"before-".as_slice(), secret, b"-after"].concat();
        let stderr = [secret.as_slice(), b"-stderr".as_slice()].concat();
        let mut runner = CheckRunner::conformance([ConformanceCheck::pass(stdout, stderr)]);
        let result = verify_precommit(
            fixture.input(),
            fixture.request(ProfileSelection::Fast, &registry, &mut runner, &leases),
        )
        .unwrap();
        let check = &result.checks[0];
        for (reference, length, class) in [
            (
                check.stdout_artifact.as_ref().unwrap(),
                check.stdout_length.unwrap(),
                crate::store::artifacts::ArtifactClass::Log,
            ),
            (
                check.stderr_artifact.as_ref().unwrap(),
                check.stderr_length.unwrap(),
                crate::store::artifacts::ArtifactClass::Log,
            ),
            (
                check.process_artifact.as_ref().unwrap(),
                0,
                crate::store::artifacts::ArtifactClass::Report,
            ),
        ] {
            let reference = crate::store::artifacts::ArtifactReference::parse(reference).unwrap();
            let verified = fixture.artifacts.open_reference(reference).unwrap();
            let digest = verified.digest();
            let bytes = fixture.artifacts.open_bytes(digest).unwrap();
            assert_eq!(verified.manifest().class, class);
            assert_eq!(verified.manifest().principal, fixture.principal);
            assert_eq!(verified.manifest().project, fixture.project);
            assert!(matches!(
                verified.manifest().retention,
                ArtifactRetention::UntilUnixMicros(_)
            ));
            assert!(!bytes.windows(secret.len()).any(|window| window == secret));
            if length != 0 {
                assert_eq!(bytes.len() as u64, length);
                assert_eq!(
                    digest.to_string(),
                    format!("blake3:{}", blake3::hash(&bytes).to_hex())
                );
            }
        }
        assert!(
            !serde_json::to_vec(&result)
                .unwrap()
                .windows(secret.len())
                .any(|window| window == secret)
        );
    }

    #[test]
    fn class_then_id_order_is_stable_and_target_ids_cannot_reorder_it() {
        let fixture = Fixture::new();
        let checks = [
            (CheckClass::Full, "q"),
            (CheckClass::Diagnostics, "z"),
            (CheckClass::Diagnostics, "a"),
            (CheckClass::Typecheck, "m"),
        ]
        .into_iter()
        .map(|(class, id)| {
            DeclaredCheck::new(
                class,
                command(id),
                CheckRequirement::Required,
                BTreeSet::new(),
                false,
            )
            .unwrap()
        })
        .rev()
        .collect();
        let registry = VerificationRegistry::new(checks).unwrap();
        let mut runner =
            CheckRunner::conformance((0..4).map(|_| ConformanceCheck::pass(b"ok", b"")));
        let result = verify_precommit(
            fixture.input(),
            fixture.request(ProfileSelection::Full, &registry, &mut runner, &[]),
        )
        .unwrap();
        assert_eq!(
            result
                .checks
                .iter()
                .map(|check| (check.class, check.check_id.as_str()))
                .collect::<Vec<_>>(),
            [
                (CheckClass::Diagnostics, "a"),
                (CheckClass::Diagnostics, "z"),
                (CheckClass::Typecheck, "m"),
                (CheckClass::Full, "q"),
            ]
        );
    }

    #[test]
    fn trusted_registry_keeps_metacharacters_as_literal_argv() {
        let command = CheckCommand::new(
            "trusted-executable",
            "/usr/bin/cargo",
            vec!["check; rm -rf /".to_owned()],
            IMAGE,
            TOOL,
            CONFIG,
            ResourceLimits::new(
                1_000,
                64 << 20,
                16,
                8 << 20,
                64 << 20,
                64 << 20,
                1 << 20,
                10_000,
            ),
        )
        .unwrap();
        assert_eq!(command.program(), "/usr/bin/cargo");
        assert_eq!(command.arguments()[0], "check; rm -rf /");
        assert!(
            VerificationRegistry::new(vec![
                DeclaredCheck::new(
                    CheckClass::Targeted,
                    command,
                    CheckRequirement::Required,
                    BTreeSet::new(),
                    false,
                )
                .unwrap(),
            ])
            .is_ok()
        );
    }

    #[test]
    fn failures_carry_persisted_evidence_and_postcommit_abort_is_rejected() {
        let fixture = Fixture::new();
        for action in [ConformanceCheck::Timeout, ConformanceCheck::Cancelled] {
            let registry = registry(CheckClass::Diagnostics, "fault", CheckRequirement::Required);
            let mut runner = CheckRunner::conformance([action]);
            let result = verify_precommit(
                fixture.input(),
                fixture.request(ProfileSelection::Fast, &registry, &mut runner, &[]),
            )
            .unwrap();
            let check = &result.checks[0];
            assert_eq!(result.decision, CommitDecision::Abort);
            assert!(check.process_artifact.is_some());
            assert!(check.process_artifact_digest.is_some());
            assert!(check.process_artifact_length.is_some());
            assert!(check.cancellation_evidence.is_some());
            assert!(check.quiescent);
            assert_eq!(check.launch, CheckLaunchStatus::Launched);
            let receipt: VerificationReceipt =
                serde_json::from_slice(&serde_json::to_vec(&result.receipt()).unwrap()).unwrap();
            assert_eq!(receipt.process_artifacts.len(), 1);
            receipt
                .validate_artifacts(&fixture.artifacts, &fixture.principal, &fixture.project)
                .unwrap();
            let mut mismatched = receipt;
            let VerificationProcessReference::Report { digest, .. } =
                &mut mismatched.process_artifacts[0]
            else {
                panic!("launched check must reference a report")
            };
            *digest = valid_digest_for_test('a');
            assert_eq!(
                mismatched.validate_artifacts(
                    &fixture.artifacts,
                    &fixture.principal,
                    &fixture.project
                ),
                Err(VerificationError::InvalidReceipt)
            );
            let mut mismatched = result.receipt();
            let VerificationProcessReference::Report {
                cancellation_evidence,
                ..
            } = &mut mismatched.process_artifacts[0]
            else {
                panic!("launched check must reference a report")
            };
            *cancellation_evidence = Some(valid_digest_for_test('b'));
            assert_eq!(
                mismatched.validate_artifacts(
                    &fixture.artifacts,
                    &fixture.principal,
                    &fixture.project
                ),
                Err(VerificationError::InvalidReceipt)
            );
        }
        {
            let action = ConformanceCheck::NotQuiescent;
            let registry = registry(
                CheckClass::Diagnostics,
                "invalid-evidence",
                CheckRequirement::Required,
            );
            let mut runner = CheckRunner::conformance([action]);
            let result = verify_precommit(
                fixture.input(),
                fixture.request(ProfileSelection::Fast, &registry, &mut runner, &[]),
            )
            .unwrap();
            let check = &result.checks[0];
            assert_eq!(check.status, CheckResultStatus::NotQuiescent);
            assert!(check.process_artifact.is_some());
            assert!(!check.quiescent);
        }
        let protocol_registry = registry(
            CheckClass::Diagnostics,
            "protocol",
            CheckRequirement::Required,
        );
        let mut runner = CheckRunner::conformance([ConformanceCheck::ProtocolMismatch]);
        let result = verify_precommit(
            fixture.input(),
            fixture.request(ProfileSelection::Fast, &protocol_registry, &mut runner, &[]),
        )
        .unwrap();
        assert_eq!(result.checks[0].status, CheckResultStatus::ProtocolFailure);
        assert_eq!(result.checks[0].launch, CheckLaunchStatus::Launched);
        assert!(result.checks[0].quiescent);

        let missing_registry = registry(
            CheckClass::Diagnostics,
            "missing-evidence",
            CheckRequirement::Required,
        );
        let mut runner = CheckRunner::conformance([ConformanceCheck::MissingEvidence]);
        let result = verify_precommit(
            fixture.input(),
            fixture.request(ProfileSelection::Fast, &missing_registry, &mut runner, &[]),
        )
        .unwrap();
        let check = &result.checks[0];
        assert_eq!(check.launch, CheckLaunchStatus::Launched);
        assert_eq!(check.status, CheckResultStatus::NotQuiescent);
        assert!(check.process_artifact.is_none());
        assert!(!check.quiescent);

        let registry = registry(
            CheckClass::Diagnostics,
            "not-started",
            CheckRequirement::Required,
        );
        let mut runner = CheckRunner::conformance([ConformanceCheck::Unavailable]);
        let result = verify_precommit(
            fixture.input(),
            fixture.request(ProfileSelection::Fast, &registry, &mut runner, &[]),
        )
        .unwrap();
        let check = &result.checks[0];
        assert_eq!(check.status, CheckResultStatus::Unavailable);
        assert_eq!(check.launch, CheckLaunchStatus::NotStarted);
        assert!(check.process_artifact.is_none());
        assert!(check.cancellation_evidence.is_none());
        assert!(!check.quiescent);
        let receipt = result.receipt();
        assert_eq!(receipt.selected_check_count, 1);
        assert_eq!(receipt.process_artifact_count, 0);
        assert!(matches!(
            receipt.process_artifacts.as_slice(),
            [VerificationProcessReference::NotStarted { check_id }] if check_id == "not-started"
        ));
        receipt
            .validate_artifacts(&fixture.artifacts, &fixture.principal, &fixture.project)
            .unwrap();
        let advisory = DeclaredCheck::new(
            CheckClass::Diagnostics,
            command("postcommit"),
            CheckRequirement::Advisory,
            BTreeSet::new(),
            true,
        )
        .unwrap();
        assert_eq!(
            validate_postcommit_policy(CheckFailureBehavior::Abort, &[advisory]),
            Err(VerificationError::UnsafePostCommitPolicy)
        );
        assert_eq!(
            postcommit_decision(RequiredOutcome::Fail),
            CommitDecision::AlreadyCommittedWithFailure
        );
    }

    fn valid_digest_for_test(byte: char) -> String {
        format!("blake3:{}", byte.to_string().repeat(64))
    }

    #[test]
    fn targeted_adds_the_affected_required_floor_then_exact_targets() {
        let fixture = Fixture::new();
        let checks = [
            (
                CheckClass::Targeted,
                "target-b",
                CheckRequirement::Required,
                "other",
            ),
            (
                CheckClass::Typecheck,
                "type",
                CheckRequirement::Required,
                "src",
            ),
            (
                CheckClass::Diagnostics,
                "diag",
                CheckRequirement::Required,
                "src",
            ),
            (
                CheckClass::Syntax,
                "syntax",
                CheckRequirement::Required,
                "src",
            ),
            (
                CheckClass::Diagnostics,
                "advisory",
                CheckRequirement::Advisory,
                "src",
            ),
            (
                CheckClass::Typecheck,
                "unaffected",
                CheckRequirement::Required,
                "other",
            ),
            (
                CheckClass::Targeted,
                "target-a",
                CheckRequirement::Required,
                "other",
            ),
        ]
        .into_iter()
        .map(|(class, id, requirement, prefix)| {
            DeclaredCheck::new(
                class,
                command(id),
                requirement,
                BTreeSet::from([prefix.to_owned()]),
                false,
            )
            .unwrap()
        })
        .collect();
        let registry = VerificationRegistry::new(checks).unwrap();
        let mut runner = CheckRunner::conformance(
            (0..5).map(|_| ConformanceCheck::pass(Vec::new(), Vec::new())),
        );
        let result = verify_precommit(
            fixture.input(),
            fixture.request(
                ProfileSelection::Targeted {
                    exact_targets: BTreeSet::from(["target-b".to_owned(), "target-a".to_owned()]),
                },
                &registry,
                &mut runner,
                &[],
            ),
        )
        .unwrap();
        assert_eq!(
            result
                .checks
                .iter()
                .map(|check| (check.class, check.check_id.as_str()))
                .collect::<Vec<_>>(),
            [
                (CheckClass::Syntax, "syntax"),
                (CheckClass::Diagnostics, "diag"),
                (CheckClass::Typecheck, "type"),
                (CheckClass::Targeted, "target-a"),
                (CheckClass::Targeted, "target-b"),
            ]
        );
        assert_eq!(result.decision, CommitDecision::Commit);
        let receipt = result.receipt();
        assert_eq!(receipt.stdout_artifacts.len(), 5);
        assert_eq!(receipt.stderr_artifacts.len(), 5);
        receipt
            .validate_artifacts(&fixture.artifacts, &fixture.principal, &fixture.project)
            .unwrap();

        let invalid = |receipt: &VerificationReceipt| {
            assert_eq!(
                receipt.validate_artifacts(
                    &fixture.artifacts,
                    &fixture.principal,
                    &fixture.project
                ),
                Err(VerificationError::InvalidReceipt)
            );
        };
        let mut attacked = receipt.clone();
        attacked.process_artifacts.clear();
        attacked.process_artifact_count = 0;
        invalid(&attacked);

        let mut attacked = receipt.clone();
        attacked.process_artifacts.remove(0);
        attacked.process_artifact_count -= 1;
        attacked.selected_check_count -= 1;
        invalid(&attacked);

        let mut attacked = receipt.clone();
        attacked.process_artifacts[1] = attacked.process_artifacts[0].clone();
        invalid(&attacked);

        let mut attacked = receipt.clone();
        attacked.process_artifacts.swap(0, 1);
        invalid(&attacked);

        let mut attacked = receipt.clone();
        let VerificationProcessReference::Report { reference, .. } =
            &mut attacked.process_artifacts[0]
        else {
            panic!("launched check must reference a report")
        };
        *reference = attacked.result_artifact.reference.clone();
        invalid(&attacked);

        let mut attacked = receipt.clone();
        attacked
            .process_artifacts
            .push(attacked.process_artifacts[0].clone());
        attacked.process_artifact_count += 1;
        attacked.selected_check_count += 1;
        invalid(&attacked);

        let mut attacked = receipt.clone();
        attacked.plan_digest = valid_digest_for_test('a');
        invalid(&attacked);
        let mut attacked = receipt.clone();
        attacked.binding_digest = valid_digest_for_test('b');
        invalid(&attacked);
        let mut attacked = receipt.clone();
        attacked.provenance = valid_digest_for_test('d');
        invalid(&attacked);
        let mut attacked = receipt.clone();
        attacked.evidence_digest = valid_digest_for_test('c');
        invalid(&attacked);
        let mut attacked = receipt.clone();
        attacked.result_artifact.length += 1;
        invalid(&attacked);
        assert_eq!(
            receipt.validate_artifacts(&fixture.artifacts, "other-principal", &fixture.project),
            Err(VerificationError::InvalidReceipt)
        );

        let mut reordered = result.clone();
        reordered.checks.swap(0, 1);
        assert_ne!(digest(&reordered.canonical_bytes()), result.result_digest);

        let mut runner = CheckRunner::conformance([
            ConformanceCheck::pass(Vec::new(), Vec::new()),
            ConformanceCheck::exit(1, Vec::new(), b"diagnostic failed"),
            ConformanceCheck::pass(Vec::new(), Vec::new()),
            ConformanceCheck::pass(Vec::new(), Vec::new()),
            ConformanceCheck::pass(Vec::new(), Vec::new()),
        ]);
        let failed = verify_precommit(
            fixture.input(),
            fixture.request(
                ProfileSelection::Targeted {
                    exact_targets: BTreeSet::from(["target-a".to_owned(), "target-b".to_owned()]),
                },
                &registry,
                &mut runner,
                &[],
            ),
        )
        .unwrap();
        assert_eq!(failed.checks[1].status, CheckResultStatus::Nonzero);
        assert_eq!(failed.decision, CommitDecision::Abort);
    }

    struct ExternalRegistry;

    impl ProcessRegistry for ExternalRegistry {
        fn prepared(
            &self,
            _context: ProcessRegistrationContext,
            _claim: ProcessClaim,
            _boundary: &PersistedBoundary,
            _terminal: ProcessTerminalConfig,
        ) -> io::Result<()> {
            Ok(())
        }

        fn started(
            &self,
            _context: ProcessRegistrationContext,
            _record: &ProcessRecord,
        ) -> io::Result<()> {
            Ok(())
        }

        fn exited(
            &self,
            _context: ProcessRegistrationContext,
            _record: &ProcessRecord,
        ) -> io::Result<()> {
            Ok(())
        }

        fn outcome_unknown(
            &self,
            _context: ProcessRegistrationContext,
            _process_id: ProcessId,
        ) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    #[ignore = "requires KIT_CHECK_IMAGE, KIT_CHECK_TOOL_DIGEST, and the installed trusted helper"]
    fn trusted_helper_external_canary_is_absent_from_every_verification_surface() {
        let fixture = Fixture::new();
        let canary = b"KIT_EXTERNAL_CHECK_CANARY";
        let command = CheckCommand::new(
            "external-canary",
            "/usr/bin/printf",
            vec![String::from_utf8(canary.to_vec()).unwrap()],
            std::env::var("KIT_CHECK_IMAGE").expect("KIT_CHECK_IMAGE"),
            std::env::var("KIT_CHECK_TOOL_DIGEST").expect("KIT_CHECK_TOOL_DIGEST"),
            CONFIG,
            ResourceLimits::new(
                1_000,
                64 << 20,
                16,
                8 << 20,
                64 << 20,
                64 << 20,
                1 << 20,
                10_000,
            ),
        )
        .unwrap();
        let registry = VerificationRegistry::new(vec![
            DeclaredCheck::new(
                CheckClass::Diagnostics,
                command,
                CheckRequirement::Required,
                BTreeSet::new(),
                false,
            )
            .unwrap(),
        ])
        .unwrap();
        let registration = ProcessRegistryRegistration::new(
            Arc::new(ExternalRegistry),
            ProcessRegistrationContext {
                project_id: fixture.grants.project_id(),
                principal_id: fixture.grants.principal_id(),
            },
        );
        let mut runner = CheckRunner::registered_container(
            ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap()),
            registration,
        );
        let leases = [SecretLease::new(canary.to_vec())];
        let result = verify_precommit(
            fixture.input(),
            fixture.request(ProfileSelection::Fast, &registry, &mut runner, &leases),
        )
        .unwrap();
        let check = &result.checks[0];
        for reference in [
            check.stdout_artifact.as_ref().unwrap(),
            check.stderr_artifact.as_ref().unwrap(),
            check.process_artifact.as_ref().unwrap(),
        ] {
            let artifact = fixture
                .artifacts
                .open_reference(
                    crate::store::artifacts::ArtifactReference::parse(reference).unwrap(),
                )
                .unwrap();
            let bytes = fixture.artifacts.open_bytes(artifact.digest()).unwrap();
            assert!(!bytes.windows(canary.len()).any(|window| window == canary));
        }
        assert!(
            !serde_json::to_vec(&result)
                .unwrap()
                .windows(canary.len())
                .any(|window| window == canary)
        );
    }
}
