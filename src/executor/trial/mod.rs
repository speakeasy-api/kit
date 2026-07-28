use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use chacha20poly1305::{ChaCha20Poly1305, KeyInit, aead::Aead};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(windows)]
use crate::executor::{
    backends::windows_job::{
        WindowsCommand, spawn_attempt_registered_sequence, spawn_daemon_observed,
        spawn_daemon_observed_with_secret_broker,
    },
    process::own::ProcessState,
    profile::MountRole,
};

use crate::{
    domain::{lifecycle::ProcessOwnership, secret::SecretLease},
    executor::{
        backends::container::{
            self, ContainerError, ExecutionError, ExecutionOutcome, limits::NotAvailable,
        },
        profile::{
            Architecture, ExecutorProfile, Platform, ProfileError, ProfileSpec, ResourceLimits,
            TrustTier,
        },
        secrets::{ExecutorSecretBroker, SecretAuthorization},
        vm_iface::{
            VM_CONTRACT_VERSION, VM_SCHEMA_VERSION, VmContractError, VmFence, VmNetworkPolicy,
            VmResourceProfile, VmRunContract, VmRunSpec, VmStorageMode,
        },
    },
    telemetry::redact::{CaptureRedactor, SensitiveDataScanner},
    workspace::acquire::{AcquisitionResult, DirtyContent},
};

pub const AGENT_OUTPUT_PATH: &str = "/build";
pub const GRADER_RESULT_PATH: &str = "/build/.kit-grader-result.json";
const GRADER_RESULT_NAME: &str = ".kit-grader-result.json";
const RECORD_SCHEMA_VERSION: u16 = 1;
const ALLOCATION_ATTEMPTS: usize = 32;
const ALLOCATION_MARKER: &str = ".kit-trial-owner";
#[cfg(not(windows))]
const GRADER_ENTRYPOINT: &str = "/usr/libexec/kit-trial-grader";
#[cfg(windows)]
const WINDOWS_GRADER_ENTRYPOINT: &str = r"C:\Program Files\Kit\kit-trial-grader.exe";
const MAX_CPU_SECONDS: u64 = 8 * 60 * 60;
const MAX_MEMORY_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const MAX_PIDS: u32 = 4096;
const MAX_DISK_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_IO_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_WALL_SECONDS: u64 = 4 * 60 * 60;
const MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_GRADER_RESULT_BYTES: u64 = 64 * 1024;
const MAX_ARTIFACT_FILES: usize = 10_000;
const MAX_ARTIFACT_DIRECTORIES: usize = 2_000;
const MAX_ARTIFACT_DEPTH: usize = 32;
const MAX_ARTIFACT_PATH_BYTES: usize = 4096;
const MAX_ARTIFACT_METADATA_BYTES: usize = 2 * 1024 * 1024;
const ARTIFACT_MODE_DIRECTORY: &str = "040000";
const ARTIFACT_MODE_REGULAR: &str = "100644";
const ARTIFACT_MODE_EXECUTABLE: &str = "100755";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentMaterial {
    Source,
    TaskInput,
    Output,
    Grader,
    GoldPatch,
    HiddenAcceptanceRules,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentAccess {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessDecision {
    Allowed,
    Denied,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AgentAuthority;

impl AgentAuthority {
    pub const fn authorize(self, material: AgentMaterial, access: AgentAccess) -> AccessDecision {
        match (material, access) {
            (AgentMaterial::Source | AgentMaterial::TaskInput, AgentAccess::Read)
            | (AgentMaterial::Output, _) => AccessDecision::Allowed,
            _ => AccessDecision::Denied,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrialManifestWire {
    schema_version: String,
    kind: String,
    trial_id: String,
    identity: TrialIdentityWire,
    task: TaskWire,
    environment: EnvironmentWire,
    budget: BudgetWire,
    cache_condition: CacheConditionWire,
    grader: GraderWire,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrialIdentityWire {
    canonical_digest: String,
    randomization_id: String,
    attempt: u64,
    task_id: String,
    environment_id: String,
    budget_id: String,
    cache_condition_id: String,
    grader_id: String,
    config_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TaskWire {
    schema_version: String,
    kind: String,
    task_id: String,
    task_version: String,
    repository: RepositoryWire,
    specification_digest: String,
    scaffold_digest: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
enum RepositoryWire {
    Https {
        url: String,
        commit: String,
    },
    Ssh {
        url: String,
        commit: String,
    },
    LocalFixture {
        fixture: String,
        commit: String,
        fixture_grant: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentWire {
    schema_version: String,
    kind: String,
    environment_id: String,
    image_digest: String,
    architecture: ArchitectureWire,
    model: ModelWire,
    components: ComponentsWire,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ArchitectureWire {
    X86_64,
    Aarch64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelWire {
    provider: String,
    name: String,
    snapshot: String,
    reasoning_effort: String,
    model_digest: String,
    settings_digest: String,
    provider_capability_digest: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ComponentsWire {
    prompt_digest: String,
    tools_digest: String,
    router_digest: String,
    retry_policy_digest: String,
    verifier_digest: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BudgetWire {
    schema_version: String,
    kind: String,
    budget_id: String,
    limits: BudgetLimitsWire,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BudgetLimitsWire {
    cpu_seconds: u64,
    memory_bytes: u64,
    disk_bytes: u64,
    network_bytes: u64,
    processes: u32,
    wall_seconds: u64,
    turns: u64,
    tokens: u64,
    dollars_usd: f64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CacheConditionWire {
    schema_version: String,
    kind: String,
    cache_condition_id: String,
    prompt: CacheStateWire,
    infrastructure: CacheStateWire,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum CacheStateWire {
    Cold,
    Warm { state_digest: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GraderWire {
    schema_version: String,
    kind: String,
    grader_id: String,
    grader_version: String,
    image_digest: String,
    harness_commit: String,
    hidden_tests_digest: String,
    acceptance_digest: String,
    gold_patch_digest: String,
    harness_config_digest: String,
}

#[derive(Debug)]
pub struct ImmutableTrialManifest {
    wire: TrialManifestWire,
    bytes_digest: String,
}

impl ImmutableTrialManifest {
    pub fn from_phase0_bytes(bytes: &[u8]) -> Result<Self, TrialError> {
        let wire: TrialManifestWire = serde_json::from_slice(bytes)
            .map_err(|error| TrialError::Manifest(ManifestError::Json(error.to_string())))?;
        validate_manifest(&wire).map_err(TrialError::Manifest)?;
        let canonical = serde_json::to_vec(&wire)
            .map_err(|error| TrialError::Manifest(ManifestError::Json(error.to_string())))?;
        Ok(Self {
            wire,
            bytes_digest: sha256(&canonical),
        })
    }

    pub fn trial_id(&self) -> &str {
        &self.wire.trial_id
    }

    pub fn identity_digest(&self) -> &str {
        &self.wire.identity.canonical_digest
    }

    pub fn manifest_bytes_digest(&self) -> &str {
        &self.bytes_digest
    }

    pub fn agent_image_digest(&self) -> &str {
        &self.wire.environment.image_digest
    }

    pub fn grader_image_digest(&self) -> &str {
        &self.wire.grader.image_digest
    }

    pub fn grader_harness_commit(&self) -> &str {
        &self.wire.grader.harness_commit
    }

    pub fn model_digest(&self) -> &str {
        &self.wire.environment.model.model_digest
    }

    pub fn model_settings_digest(&self) -> &str {
        &self.wire.environment.model.settings_digest
    }

    pub fn provider_capability_digest(&self) -> &str {
        &self.wire.environment.model.provider_capability_digest
    }

    pub fn config_digest(&self) -> String {
        sha256(self.wire.identity.config_id.as_bytes())
    }

    pub fn repository_commit(&self) -> &str {
        match &self.wire.task.repository {
            RepositoryWire::Https { commit, .. }
            | RepositoryWire::Ssh { commit, .. }
            | RepositoryWire::LocalFixture { commit, .. } => commit,
        }
    }

    pub fn hidden_tests_digest(&self) -> &str {
        &self.wire.grader.hidden_tests_digest
    }

    pub fn acceptance_digest(&self) -> &str {
        &self.wire.grader.acceptance_digest
    }

    pub fn specification_digest(&self) -> &str {
        &self.wire.task.specification_digest
    }

    pub fn scaffold_digest(&self) -> &str {
        &self.wire.task.scaffold_digest
    }

    pub fn gold_patch_digest(&self) -> &str {
        &self.wire.grader.gold_patch_digest
    }

    pub fn harness_config_digest(&self) -> &str {
        &self.wire.grader.harness_config_digest
    }

    pub fn validate_usage(&self, usage: TrialUsage) -> Result<(), TrialError> {
        let measured = |value, name| match value {
            UsageMeasure::Measured(value) => Ok(value),
            UsageMeasure::Unavailable(reason) => Err(TrialError::UsageUnavailable(name, reason)),
        };
        let turns = measured(usage.turns, "turns")?;
        let input_tokens = measured(usage.input_tokens, "input tokens")?;
        let output_tokens = measured(usage.output_tokens, "output tokens")?;
        let tokens = input_tokens
            .checked_add(output_tokens)
            .ok_or(TrialError::UsageBudgetExceeded)?;
        let cost = measured(usage.cost_microusd, "cost")?;
        let limits = &self.wire.budget.limits;
        let cost_limit = (limits.dollars_usd * 1_000_000.0) as u64;
        if turns > limits.turns || tokens > limits.tokens || cost > cost_limit {
            return Err(TrialError::UsageBudgetExceeded);
        }
        measured(usage.tool_calls, "tool calls")?;
        measured(usage.processes, "processes")?;
        Ok(())
    }

    pub fn trial_run_binding(
        &self,
        attempt: crate::domain::lifecycle::AttemptOwnership,
    ) -> Result<crate::runtime::scheduler::TrialRunBinding, TrialError> {
        if attempt.fencing_token.get()
            != self
                .wire
                .identity
                .attempt
                .checked_add(1)
                .ok_or(TrialError::Manifest(ManifestError::AttemptOverflow))?
        {
            return Err(TrialError::UsageReceiptMismatch);
        }
        Ok(crate::runtime::scheduler::TrialRunBinding {
            trial_id: self.trial_id().to_owned(),
            trial_digest: self.manifest_bytes_digest().to_owned(),
            task_digest: sha256(
                &serde_json::to_vec(&self.wire.task)
                    .map_err(|error| TrialError::Serialization(error.to_string()))?,
            ),
            model_digest: self.wire.environment.model.model_digest.clone(),
            config_digest: self.config_digest(),
            attempt,
            admission: None,
        })
    }

    fn component_pins(&self) -> TrialComponentPins {
        let mut pins = BTreeMap::from([
            (
                "repository_commit".to_owned(),
                self.repository_commit().to_owned(),
            ),
            (
                "specification".to_owned(),
                self.specification_digest().to_owned(),
            ),
            ("scaffold".to_owned(), self.scaffold_digest().to_owned()),
            (
                "agent_image".to_owned(),
                self.agent_image_digest().to_owned(),
            ),
            (
                "model".to_owned(),
                self.wire.environment.model.model_digest.clone(),
            ),
            (
                "model_settings".to_owned(),
                self.wire.environment.model.settings_digest.clone(),
            ),
            (
                "provider_capability".to_owned(),
                self.wire
                    .environment
                    .model
                    .provider_capability_digest
                    .clone(),
            ),
            (
                "prompt".to_owned(),
                self.wire.environment.components.prompt_digest.clone(),
            ),
            (
                "tools".to_owned(),
                self.wire.environment.components.tools_digest.clone(),
            ),
            (
                "router".to_owned(),
                self.wire.environment.components.router_digest.clone(),
            ),
            (
                "retry_policy".to_owned(),
                self.wire.environment.components.retry_policy_digest.clone(),
            ),
            (
                "verifier".to_owned(),
                self.wire.environment.components.verifier_digest.clone(),
            ),
            (
                "grader_image".to_owned(),
                self.grader_image_digest().to_owned(),
            ),
            (
                "grader_harness_commit".to_owned(),
                self.wire.grader.harness_commit.clone(),
            ),
            (
                "hidden_tests".to_owned(),
                self.hidden_tests_digest().to_owned(),
            ),
            ("acceptance".to_owned(), self.acceptance_digest().to_owned()),
        ]);
        for (name, state) in [
            ("prompt_cache", &self.wire.cache_condition.prompt),
            (
                "infrastructure_cache",
                &self.wire.cache_condition.infrastructure,
            ),
        ] {
            if let CacheStateWire::Warm { state_digest } = state {
                pins.insert(name.to_owned(), state_digest.clone());
            }
        }
        pins.insert("gold_patch".to_owned(), self.gold_patch_digest().to_owned());
        pins.insert(
            "harness_config".to_owned(),
            self.harness_config_digest().to_owned(),
        );
        TrialComponentPins { pins }
    }

    pub fn profile(&self) -> Result<ExecutorProfile, TrialError> {
        let limits = &self.wire.budget.limits;
        let cpu_millis = limits
            .cpu_seconds
            .checked_mul(1000)
            .ok_or(TrialError::Manifest(ManifestError::BudgetOverflow("cpu")))?;
        let wall_time_millis =
            limits
                .wall_seconds
                .checked_mul(1000)
                .ok_or(TrialError::Manifest(ManifestError::BudgetOverflow(
                    "wall time",
                )))?;
        let resources = ResourceLimits::new(
            cpu_millis,
            limits.memory_bytes,
            limits.processes,
            limits.disk_bytes.min(MAX_FILE_BYTES),
            limits.disk_bytes,
            limits.disk_bytes.min(MAX_IO_BYTES),
            limits.disk_bytes.min(MAX_OUTPUT_BYTES),
            wall_time_millis,
        );
        ExecutorProfile::new(ProfileSpec::isolated(
            TrustTier::Restricted,
            if cfg!(windows) {
                Platform::Windows
            } else {
                Platform::Linux
            },
            match self.wire.environment.architecture {
                ArchitectureWire::X86_64 => Architecture::X86_64,
                ArchitectureWire::Aarch64 => Architecture::Aarch64,
            },
            resources,
        ))
        .map_err(TrialError::Profile)
    }

    fn vm_contract(
        &self,
        identity: &FreshTrialIdentity,
        phase: TrialPhase,
    ) -> Result<VmRunContract, TrialError> {
        let limits = &self.wire.budget.limits;
        VmRunContract::new(VmRunSpec {
            schema_version: VM_SCHEMA_VERSION,
            contract_version: VM_CONTRACT_VERSION,
            run_id: format!("{}-{phase:?}", self.wire.trial_id).to_ascii_lowercase(),
            fence: VmFence::new(
                self.wire
                    .identity
                    .attempt
                    .checked_add(1)
                    .ok_or(TrialError::Manifest(ManifestError::AttemptOverflow))?,
            )
            .map_err(TrialError::VmContract)?,
            image_digest: match phase {
                TrialPhase::Agent => self.agent_image_digest(),
                TrialPhase::Grader => self.grader_image_digest(),
            }
            .to_owned(),
            instance_id: identity.instance_id.clone(),
            rootfs_layer_id: identity.rootfs_layer_id.clone(),
            storage_mode: VmStorageMode::CopyOnWrite,
            network: VmNetworkPolicy::Deny,
            default_grants: BTreeSet::new(),
            secret_handles: BTreeSet::new(),
            resources: VmResourceProfile {
                cpu_millis: limits
                    .cpu_seconds
                    .checked_mul(1000)
                    .ok_or(TrialError::Manifest(ManifestError::BudgetOverflow("cpu")))?,
                memory_bytes: limits.memory_bytes,
                disk_bytes: limits.disk_bytes,
                pids: limits.processes,
                wall_time_millis: limits.wall_seconds.checked_mul(1000).ok_or(
                    TrialError::Manifest(ManifestError::BudgetOverflow("wall time")),
                )?,
            },
        })
        .map_err(TrialError::VmContract)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FreshTrialIdentity {
    pub instance_id: String,
    pub rootfs_layer_id: String,
    pub writable_layer_id: String,
}

impl FreshTrialIdentity {
    pub fn for_conformance(sequence: u64) -> Self {
        Self {
            instance_id: format!("conformance-instance-{sequence}"),
            rootfs_layer_id: format!("conformance-rootfs-{sequence}"),
            writable_layer_id: format!("conformance-writable-{sequence}"),
        }
    }
}

#[derive(Debug)]
pub struct TrialAllocation {
    identity: FreshTrialIdentity,
    writable_path: PathBuf,
    agent_temp: PathBuf,
    grader_temp: PathBuf,
    agent_snapshot: PathBuf,
    grader_input: PathBuf,
    grader_output: PathBuf,
    reservations: Vec<TrialReservation>,
}

#[derive(Debug)]
struct TrialReservation {
    path: PathBuf,
    marker: String,
    identity: FilesystemIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FilesystemIdentity {
    device: u64,
    inode: u64,
}

impl TrialAllocation {
    pub fn allocate(workspace: &AcquisitionResult) -> Result<Self, TrialError> {
        let parent = workspace
            .path
            .parent()
            .ok_or_else(|| TrialError::UnsafePath(workspace.path.clone()))?;
        for _ in 0..ALLOCATION_ATTEMPTS {
            let nonce = random_hex()?;
            match Self::allocate_with_nonce(parent, &nonce) {
                Ok(allocation) => return Ok(allocation),
                Err(TrialError::AllocationCollision) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(TrialError::AllocationCollision)
    }

    fn allocate_with_nonce(parent: &Path, nonce: &str) -> Result<Self, TrialError> {
        let mut reservations = Vec::new();
        for role in [
            "writable",
            "agent-temp",
            "grader-temp",
            "agent-snapshot",
            "grader-input",
            "grader-output",
        ] {
            let path = parent.join(format!("trial-{role}-{nonce}"));
            match reserve_trial_directory(&path, nonce, role) {
                Ok(reservation) => reservations.push(reservation),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    cleanup_partial(&mut reservations)?;
                    return Err(TrialError::AllocationCollision);
                }
                Err(error) => {
                    cleanup_partial(&mut reservations).map_err(|cleanup| {
                        TrialError::CleanupAfterFailure {
                            primary: error.to_string(),
                            cleanup: cleanup.to_string(),
                        }
                    })?;
                    return Err(TrialError::Io(error));
                }
            }
        }
        let path = |role: &str| parent.join(format!("trial-{role}-{nonce}"));
        Ok(Self {
            identity: FreshTrialIdentity {
                instance_id: format!("trial-instance-{nonce}"),
                rootfs_layer_id: format!("trial-rootfs-{nonce}"),
                writable_layer_id: format!("trial-writable-{nonce}"),
            },
            writable_path: path("writable"),
            agent_temp: path("agent-temp"),
            grader_temp: path("grader-temp"),
            agent_snapshot: path("agent-snapshot"),
            grader_input: path("grader-input"),
            grader_output: path("grader-output"),
            reservations,
        })
    }

    pub const fn identity(&self) -> &FreshTrialIdentity {
        &self.identity
    }

    pub fn writable_path(&self) -> &Path {
        &self.writable_path
    }

    pub fn marker_identity(&self) -> &str {
        &self.reservations[0].marker
    }

    pub fn reserved_paths(&self) -> impl Iterator<Item = &Path> {
        self.reservations
            .iter()
            .map(|reservation| reservation.path.as_path())
    }

    pub fn cleanup(mut self) -> Result<(), TrialError> {
        cleanup_partial(&mut self.reservations).map_err(TrialError::Cleanup)
    }
}

fn reserve_trial_directory(path: &Path, nonce: &str, role: &str) -> io::Result<TrialReservation> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    let identity = filesystem_identity(&fs::symlink_metadata(path)?)
        .ok_or_else(|| io::Error::other("filesystem identity unavailable"))?;
    let marker = format!("kit-trial-v1:{nonce}:{role}");
    let marker_path = path.join(ALLOCATION_MARKER);
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&marker_path)?;
        file.write_all(marker.as_bytes())?;
        file.sync_all()?;
        Ok(TrialReservation {
            path: path.to_owned(),
            marker,
            identity: identity.clone(),
        })
    })();
    if result.is_err() {
        cleanup_created_directory(path, &identity)?;
    }
    result
}

fn cleanup_created_directory(path: &Path, identity: &FilesystemIdentity) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || filesystem_identity(&metadata).as_ref() != Some(identity)
    {
        return Err(io::Error::other(
            "partial trial allocation identity changed",
        ));
    }
    let quarantine = path.with_file_name(format!(
        ".kit-trial-quarantine-partial-{}",
        random_hex_io()?
    ));
    fs::rename(path, &quarantine)?;
    let after = fs::symlink_metadata(&quarantine)?;
    if filesystem_identity(&after).as_ref() != Some(identity) {
        return Err(io::Error::other(
            "partial trial quarantine identity changed",
        ));
    }
    fs::remove_dir_all(quarantine)
}

fn cleanup_partial(reservations: &mut Vec<TrialReservation>) -> Result<(), io::Error> {
    while let Some(reservation) = reservations.pop() {
        cleanup_reservation(&reservation)?;
    }
    Ok(())
}

fn cleanup_reservation(reservation: &TrialReservation) -> io::Result<()> {
    let metadata = fs::symlink_metadata(&reservation.path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || filesystem_identity(&metadata).as_ref() != Some(&reservation.identity)
        || read_regular_bounded(
            &reservation.path.join(ALLOCATION_MARKER),
            reservation.marker.len(),
        )? != reservation.marker.as_bytes()
    {
        return Err(io::Error::other("trial allocation identity changed"));
    }
    let quarantine = reservation
        .path
        .with_file_name(format!(".kit-trial-quarantine-{}", random_hex_io()?));
    fs::rename(&reservation.path, &quarantine)?;
    let quarantined = fs::symlink_metadata(&quarantine)?;
    if filesystem_identity(&quarantined).as_ref() != Some(&reservation.identity) {
        return Err(io::Error::other("quarantined trial identity changed"));
    }
    fs::remove_dir_all(quarantine)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialPhase {
    Agent,
    Grader,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRoute {
    TrustedContainerHelper,
    TrustedWindowsComposite,
    ConformanceFake,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum BoundaryOutcome {
    Success,
    Exit(i32),
    Signal(i32),
}

impl From<ExecutionOutcome> for BoundaryOutcome {
    fn from(value: ExecutionOutcome) -> Self {
        match value {
            ExecutionOutcome::Success => Self::Success,
            ExecutionOutcome::Exit(code) => Self::Exit(code),
            ExecutionOutcome::Signal(signal) => Self::Signal(signal),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BoundaryRequest<'a> {
    pub trial_id: &'a str,
    pub phase: TrialPhase,
    pub image_digest: &'a str,
    pub identity: &'a FreshTrialIdentity,
    pub agent_authority: Option<AgentAuthority>,
    pub permitted_profile_digest: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BoundaryCompletion {
    pub phase: TrialPhase,
    pub route: ExecutionRoute,
    pub image_digest: String,
    pub boundary_id: String,
    pub instance_id: String,
    pub rootfs_layer_id: String,
    pub writable_layer_id: String,
    pub plan_digest: String,
    pub invocation_digest: String,
    pub runtime_identity: String,
    pub helper_identity: String,
    pub permitted_profile_digest: String,
    pub survivor_processes: u32,
    pub quiescent: bool,
    pub outcome: BoundaryOutcome,
}

pub trait IsolatedTrialContract {
    fn execute(&mut self, request: BoundaryRequest<'_>) -> Result<BoundaryCompletion, TrialError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundaryPair {
    pub agent: BoundaryCompletion,
    pub grader: BoundaryCompletion,
}

pub fn orchestrate_conformance(
    manifest: &ImmutableTrialManifest,
    identity: &FreshTrialIdentity,
    backend: &mut impl IsolatedTrialContract,
) -> Result<BoundaryPair, TrialError> {
    orchestrate(
        manifest,
        identity,
        backend,
        ExecutionRoute::ConformanceFake,
        None,
    )
}

fn orchestrate(
    manifest: &ImmutableTrialManifest,
    identity: &FreshTrialIdentity,
    backend: &mut impl IsolatedTrialContract,
    required_route: ExecutionRoute,
    grader_profile_digest: Option<&str>,
) -> Result<BoundaryPair, TrialError> {
    let permitted_profile_digest = manifest.profile()?.digest().to_string();
    let agent = backend.execute(BoundaryRequest {
        trial_id: manifest.trial_id(),
        phase: TrialPhase::Agent,
        image_digest: manifest.agent_image_digest(),
        identity,
        agent_authority: Some(AgentAuthority),
        permitted_profile_digest: grader_profile_digest.unwrap_or(&permitted_profile_digest),
    })?;
    validate_completion(
        &agent,
        TrialPhase::Agent,
        required_route,
        manifest.agent_image_digest(),
        identity,
        grader_profile_digest.unwrap_or(&permitted_profile_digest),
    )?;
    let grader_identity = FreshTrialIdentity {
        instance_id: format!("{}-grader", identity.instance_id),
        rootfs_layer_id: format!("{}-grader", identity.rootfs_layer_id),
        writable_layer_id: format!("{}-grader", identity.writable_layer_id),
    };
    let grader = backend.execute(BoundaryRequest {
        trial_id: manifest.trial_id(),
        phase: TrialPhase::Grader,
        image_digest: manifest.grader_image_digest(),
        identity: &grader_identity,
        agent_authority: None,
        permitted_profile_digest: &permitted_profile_digest,
    })?;
    validate_completion(
        &grader,
        TrialPhase::Grader,
        required_route,
        manifest.grader_image_digest(),
        &grader_identity,
        &permitted_profile_digest,
    )?;
    Ok(BoundaryPair { agent, grader })
}

fn validate_completion(
    completion: &BoundaryCompletion,
    phase: TrialPhase,
    route: ExecutionRoute,
    image_digest: &str,
    identity: &FreshTrialIdentity,
    identity_profile_digest: &str,
) -> Result<(), TrialError> {
    if completion.phase != phase
        || completion.route != route
        || completion.image_digest != image_digest
        || completion.instance_id != identity.instance_id
        || completion.rootfs_layer_id != identity.rootfs_layer_id
        || completion.writable_layer_id != identity.writable_layer_id
        || completion.permitted_profile_digest != identity_profile_digest
    {
        return Err(TrialError::BoundaryIdentityMismatch(phase));
    }
    if !completion.quiescent || completion.survivor_processes != 0 {
        return Err(TrialError::BoundaryNotQuiescent(phase));
    }
    if completion.outcome != BoundaryOutcome::Success {
        return Err(TrialError::BoundaryFailed(phase, completion.outcome));
    }
    Ok(())
}

pub struct ProductionTrialRequest<'a> {
    pub manifest: &'a ImmutableTrialManifest,
    pub workspace: &'a AcquisitionResult,
    pub record_root: &'a Path,
    pub owner: ProcessOwnership,
    pub process_registry: crate::executor::process::own::ProcessRegistryRegistration,
    pub cancellation: Option<(
        &'a crate::executor::cancel::SqliteCancellationCoordinator,
        crate::executor::cancel::WorkspaceIdentity,
    )>,
    pub agent_command: &'a [OsString],
    pub grader_inputs: GraderInputs<'a>,
    pub usage_receipt: &'a TrialUsageReceipt,
    pub usage_receipts: &'a dyn TrialUsageReceiptStore,
    pub grader_resource_bounds: Option<GraderResourceBounds>,
    pub grader_test_probe: Option<GraderTestProbe>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GraderTestChannel {
    GraderLog,
    CanonicalReport,
    Checks,
    FinalTree,
    ExtraArtifact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GraderTestEncoding {
    Raw,
    Percent,
    Base64,
    Split,
    Binary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraderTestProbe {
    channel: GraderTestChannel,
    encoding: GraderTestEncoding,
}

impl GraderTestProbe {
    pub const fn new(channel: GraderTestChannel, encoding: GraderTestEncoding) -> Self {
        Self { channel, encoding }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraderResourceBounds {
    pub memory_bytes: u64,
    pub output_bytes: u64,
    pub wall_time_millis: u64,
}

pub fn constrained_grader_profile(
    manifest: &ImmutableTrialManifest,
    bounds: GraderResourceBounds,
) -> Result<ExecutorProfile, TrialError> {
    let profile = manifest.profile()?;
    let mut resources = profile.resources();
    resources.memory_bytes = resources.memory_bytes.min(bounds.memory_bytes);
    resources.output_bytes = resources.output_bytes.min(bounds.output_bytes);
    resources.wall_time_millis = resources.wall_time_millis.min(bounds.wall_time_millis);
    profile
        .with_resources(resources)
        .map_err(TrialError::Profile)
}

#[derive(Clone, Copy)]
pub enum TrustedInputSource<'a> {
    Path(&'a Path),
    Bytes(&'a [u8]),
}

#[derive(Clone, Copy)]
pub struct TrustedInput<'a> {
    pub source: TrustedInputSource<'a>,
    pub expected_sha256: &'a str,
}

#[derive(Clone, Copy)]
pub struct GraderInputs<'a> {
    pub specification: TrustedInput<'a>,
    pub scaffold: TrustedInput<'a>,
    pub hidden_tests: TrustedInput<'a>,
    pub gold_patch: TrustedInput<'a>,
    pub acceptance_rules: TrustedInput<'a>,
    pub harness_config: TrustedInput<'a>,
    pub harness_commit: &'a str,
}

impl<'a> GraderInputs<'a> {
    fn named(self) -> [(&'static str, TrustedInput<'a>); 6] {
        [
            ("specification", self.specification),
            ("scaffold", self.scaffold),
            ("hidden-tests", self.hidden_tests),
            ("gold-patch", self.gold_patch),
            ("acceptance-rules", self.acceptance_rules),
            ("harness-config", self.harness_config),
        ]
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GraderVerdict {
    Pass,
    Fail,
    Error,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoreGradeOutcome {
    Success,
    Failure,
    Error,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "availability", content = "value", rename_all = "snake_case")]
pub enum UsageMeasure {
    Measured(u64),
    Unavailable(UsageUnavailableReason),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageUnavailableReason {
    ProviderDidNotReport,
    SchedulerEvidenceMissing,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialUsage {
    pub turns: UsageMeasure,
    pub input_tokens: UsageMeasure,
    pub output_tokens: UsageMeasure,
    pub cost_microusd: UsageMeasure,
    pub tool_calls: UsageMeasure,
    pub processes: UsageMeasure,
}

impl TrialUsage {
    pub const ZERO: Self = Self {
        turns: UsageMeasure::Measured(0),
        input_tokens: UsageMeasure::Measured(0),
        output_tokens: UsageMeasure::Measured(0),
        cost_microusd: UsageMeasure::Measured(0),
        tool_calls: UsageMeasure::Measured(0),
        processes: UsageMeasure::Measured(0),
    };
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TrialUsageReceipt(String);

impl TrialUsageReceipt {
    pub fn parse(value: impl Into<String>) -> Result<Self, TrialError> {
        let value = value.into();
        if value.len() < 16
            || value.len() > 255
            || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(TrialError::InvalidUsageReceipt);
        }
        Ok(Self(value))
    }

    pub fn opaque_id(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrialUsageReceiptBinding {
    pub run_id: String,
    pub trial_id: String,
    pub trial_digest: String,
    pub task_digest: String,
    pub model_digest: String,
    pub config_digest: String,
    pub attempt_id: String,
    pub attempt_fence: u64,
    pub scheduler_principal_id: String,
    pub scheduler_idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VerifiedTrialUsage {
    pub binding: TrialUsageReceiptBinding,
    pub provider_request_ids: Vec<String>,
    pub durable_event_positions: Vec<u64>,
    pub event_high_watermark: u64,
    pub terminal_version: u64,
    pub usage: TrialUsage,
}

pub trait TrialUsageReceiptStore {
    fn verify(
        &self,
        receipt: &TrialUsageReceipt,
        trial_id: &str,
    ) -> Result<VerifiedTrialUsage, TrialError>;
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoreCheckEvidence {
    pub id: String,
    pub passed: bool,
    pub path: String,
    pub expected: String,
    pub actual: String,
    pub duration_micros: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoreGradeTiming {
    pub wall_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoreGradeReport {
    pub schema_version: u16,
    pub outcome: CoreGradeOutcome,
    pub base_tree_digest: String,
    pub patch_digest: String,
    pub final_tree_digest: String,
    pub checks_digest: String,
    pub hidden: CoreHiddenCheckAggregate,
    pub diagnostic: Option<String>,
    pub timing: CoreGradeTiming,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoreHiddenCheckAggregate {
    pub verdict: GraderVerdict,
    pub count: u64,
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RestrictedHiddenCheck {
    id: String,
    passed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatedGraderResult {
    pub schema_version: u16,
    pub trial_id: String,
    pub manifest_digest: String,
    pub agent_artifact_digest: String,
    pub report: CoreGradeReport,
    pub artifacts: GraderArtifactChannels,
    pub component_pins: TrialComponentPins,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GraderArtifactHandle {
    pub class: String,
    pub handle: String,
    pub digest: String,
    pub length: u64,
    pub authentication: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GraderArtifactChannels {
    pub applied_patch: GraderArtifactHandle,
    pub final_tree: GraderArtifactHandle,
    pub checks: GraderArtifactHandle,
    pub events: GraderArtifactHandle,
    pub logs: GraderArtifactHandle,
    pub agent_output: GraderArtifactHandle,
    pub usage_report: GraderArtifactHandle,
    pub hidden_checks: GraderArtifactHandle,
}

impl GraderArtifactChannels {
    fn named(&self) -> [(&'static str, &GraderArtifactHandle); 8] {
        [
            ("applied_patch", &self.applied_patch),
            ("final_tree", &self.final_tree),
            ("checks", &self.checks),
            ("events", &self.events),
            ("logs", &self.logs),
            ("agent_output", &self.agent_output),
            ("usage_report", &self.usage_report),
            ("hidden_checks", &self.hidden_checks),
        ]
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GraderUsageReport {
    pub schema_version: u16,
    pub provider_request_ids: Vec<String>,
    pub usage: TrialUsage,
    pub usage_receipt: TrialUsageReceipt,
    pub usage_evidence_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableTrial {
    pub record_path: PathBuf,
    pub record_digest: String,
    pub record: TrialRecord,
}

pub fn production_trial_route(profile: &ExecutorProfile) -> ExecutionRoute {
    if profile.platform() == Platform::Windows
        && crate::executor::backends::windows_job::required_production_spawn_backend(profile)
            == Some(
                crate::executor::backends::windows_job::ProductionSpawnBackend::WindowsComposite,
            )
    {
        ExecutionRoute::TrustedWindowsComposite
    } else {
        ExecutionRoute::TrustedContainerHelper
    }
}

pub fn execute_production_trial(
    request: ProductionTrialRequest<'_>,
) -> Result<DurableTrial, TrialError> {
    execute_production_trial_inner(request, None)
}

pub fn execute_production_trial_with_secret_broker<'a>(
    request: ProductionTrialRequest<'a>,
    authorization: SecretAuthorization,
    broker: &'a dyn ExecutorSecretBroker,
) -> Result<DurableTrial, TrialError> {
    execute_production_trial_inner(request, Some((authorization, broker)))
}

fn execute_production_trial_inner<'a>(
    request: ProductionTrialRequest<'a>,
    secret_broker: Option<(SecretAuthorization, &'a dyn ExecutorSecretBroker)>,
) -> Result<DurableTrial, TrialError> {
    if request.grader_test_probe.is_some() && !cfg!(debug_assertions) {
        return Err(TrialError::Executor(
            "grader test probes are unavailable in production builds".to_owned(),
        ));
    }
    validate_workspace(request.manifest, request.workspace)?;
    validate_grader_inputs(request.manifest, request.workspace, request.grader_inputs)?;
    let hidden_canaries = hidden_manifest_canaries(request.grader_inputs.hidden_tests.source)?;
    let hidden_canary_refs = hidden_canaries
        .iter()
        .map(Vec::as_slice)
        .collect::<Vec<_>>();
    let verified_usage = request
        .usage_receipts
        .verify(request.usage_receipt, request.manifest.trial_id())?;
    validate_verified_usage(request.manifest, &verified_usage)?;
    let profile = request.manifest.profile()?;
    let grader_profile = request
        .grader_resource_bounds
        .map(|bounds| constrained_grader_profile(request.manifest, bounds))
        .transpose()?
        .unwrap_or_else(|| profile.clone());
    match production_trial_route(&profile) {
        ExecutionRoute::TrustedWindowsComposite => {
            match secret_broker {
                Some((_, broker)) if !profile.credentials().is_empty() => {
                    crate::executor::backends::windows_job::production_spawn_backend_with_secret_broker(
                        &profile, broker,
                    )
                }
                _ => crate::executor::backends::windows_job::production_spawn_backend(&profile),
            }
            .map_err(|error| TrialError::Executor(error.to_string()))?;
        }
        ExecutionRoute::TrustedContainerHelper => {
            container::limits::probe_backend().map_err(TrialError::Unavailable)?;
        }
        ExecutionRoute::ConformanceFake => unreachable!("production never selects a fake route"),
    }
    let allocation = TrialAllocation::allocate(request.workspace)?;
    let agent_vm_contract = request
        .manifest
        .vm_contract(allocation.identity(), TrialPhase::Agent)?;
    let grader_identity = FreshTrialIdentity {
        instance_id: format!("{}-grader", allocation.identity().instance_id),
        rootfs_layer_id: format!("{}-grader", allocation.identity().rootfs_layer_id),
        writable_layer_id: format!("{}-grader", allocation.identity().writable_layer_id),
    };
    let grader_vm_contract = request
        .manifest
        .vm_contract(&grader_identity, TrialPhase::Grader)?;
    let record_root = validate_record_root(request.record_root, request.workspace, &allocation)?;
    let result = {
        let mut backend = ProductionBackend {
            profile: &profile,
            grader_profile: &grader_profile,
            workspace: request.workspace,
            allocation: &allocation,
            owner: request.owner,
            process_registry: request.process_registry,
            cancellation: request.cancellation,
            agent_command: request.agent_command,
            grader_inputs: request.grader_inputs,
            manifest: request.manifest,
            agent_artifact_digest: None,
            grader_auth_key: None,
            usage_receipt: request.usage_receipt,
            verified_usage: &verified_usage,
            secret_broker,
            grader_test_probe: request.grader_test_probe,
        };
        let boundaries = orchestrate(
            request.manifest,
            allocation.identity(),
            &mut backend,
            production_trial_route(&profile),
            Some(&grader_profile.digest().to_string()),
        );
        let sequence = backend.finish_boundary_sequence();
        boundaries
            .and_then(|boundaries| sequence.map(|()| boundaries))
            .and_then(|boundaries| {
                let agent_artifact_digest = backend
                    .agent_artifact_digest
                    .ok_or(TrialError::MissingAgentSnapshot)?;
                let grader_result = read_grader_result(
                    &allocation.grader_output.join(GRADER_RESULT_NAME),
                    request.manifest,
                    &agent_artifact_digest,
                    GraderReceiptVerification {
                        receipt: request.usage_receipt,
                        store: request.usage_receipts,
                        usage: &verified_usage,
                        auth_key: backend
                            .grader_auth_key
                            .as_deref()
                            .ok_or(TrialError::GraderResultBindingMismatch)?,
                    },
                )?;
                scan_outward_artifacts(&allocation, &hidden_canary_refs)?;
                persist_trial(
                    request.manifest,
                    request.workspace,
                    &allocation,
                    agent_vm_contract.digest().to_string(),
                    grader_vm_contract.digest().to_string(),
                    boundaries,
                    agent_artifact_digest,
                    grader_result,
                    &profile,
                    &record_root,
                )
            })
    };
    match (result, allocation.cleanup()) {
        (Ok(trial), Ok(())) => Ok(trial),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(trial), Err(cleanup)) => Err(TrialError::CleanupAfterPersistence {
            record_path: trial.record_path,
            cleanup: cleanup.to_string(),
        }),
        (Err(primary), Err(cleanup)) => Err(TrialError::CleanupAfterFailure {
            primary: primary.to_string(),
            cleanup: cleanup.to_string(),
        }),
    }
}

struct ProductionBackend<'a> {
    profile: &'a ExecutorProfile,
    grader_profile: &'a ExecutorProfile,
    workspace: &'a AcquisitionResult,
    allocation: &'a TrialAllocation,
    owner: ProcessOwnership,
    process_registry: crate::executor::process::own::ProcessRegistryRegistration,
    cancellation: Option<(
        &'a crate::executor::cancel::SqliteCancellationCoordinator,
        crate::executor::cancel::WorkspaceIdentity,
    )>,
    agent_command: &'a [OsString],
    grader_inputs: GraderInputs<'a>,
    manifest: &'a ImmutableTrialManifest,
    agent_artifact_digest: Option<String>,
    grader_auth_key: Option<Vec<u8>>,
    usage_receipt: &'a TrialUsageReceipt,
    verified_usage: &'a VerifiedTrialUsage,
    secret_broker: Option<(SecretAuthorization, &'a dyn ExecutorSecretBroker)>,
    grader_test_probe: Option<GraderTestProbe>,
}

impl IsolatedTrialContract for ProductionBackend<'_> {
    fn execute(&mut self, request: BoundaryRequest<'_>) -> Result<BoundaryCompletion, TrialError> {
        let profile = if request.phase == TrialPhase::Grader {
            self.grader_profile
        } else {
            self.profile
        };
        #[cfg(windows)]
        let (grader_entrypoint, grader_input, grader_result, grader_test_manifest) = (
            WINDOWS_GRADER_ENTRYPOINT,
            r"C:\kit-trusted-input",
            r"C:\build\.kit-grader-result.json",
            r"C:\kit-trusted-input\grader-test-manifest.json",
        );
        #[cfg(not(windows))]
        let (grader_entrypoint, grader_input, grader_result, grader_test_manifest) = (
            GRADER_ENTRYPOINT,
            "/kit-trusted-input",
            "/build/.kit-grader-result.json",
            "/kit-trusted-input/grader-test-manifest.json",
        );
        let (command, build, temp, trusted_input, suffix) = match request.phase {
            TrialPhase::Agent => (
                self.agent_command.to_vec(),
                &self.allocation.writable_path,
                &self.allocation.agent_temp,
                None,
                "agent",
            ),
            TrialPhase::Grader => {
                let reserved = self.allocation.grader_output.join(GRADER_RESULT_NAME);
                match fs::symlink_metadata(&reserved) {
                    Ok(_) => return Err(TrialError::ReservedOutputCreatedByAgent),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(TrialError::Io(error)),
                }
                let artifacts = snapshot_tree(
                    &self.allocation.writable_path,
                    &self.allocation.agent_snapshot,
                    MAX_OUTPUT_BYTES.min(self.profile.resources().output_bytes),
                )
                .map_err(TrialError::Io)?;
                let artifact_digest = artifact_digest(&artifacts)?;
                self.agent_artifact_digest = Some(artifact_digest.clone());
                stage_grader_inputs(
                    self.grader_inputs,
                    &self.allocation.grader_input,
                    &self.allocation.agent_snapshot,
                )?;
                stage_usage_receipt(
                    self.usage_receipt,
                    self.verified_usage,
                    &self.allocation.grader_input.join("usage-receipt.json"),
                )?;
                let auth_key = random_bytes(32)?;
                write_new_synced(
                    &self.allocation.grader_input.join("artifact-auth-key"),
                    &auth_key,
                )?;
                let test_nonce = self
                    .grader_test_probe
                    .map(|probe| {
                        stage_grader_test_manifest(
                            probe,
                            &self
                                .allocation
                                .grader_input
                                .join("grader-test-manifest.json"),
                            &auth_key,
                            self.manifest.manifest_bytes_digest(),
                            &artifact_digest,
                        )
                    })
                    .transpose()?;
                self.grader_auth_key = Some(auth_key);
                let mut command = vec![
                    OsString::from(grader_entrypoint),
                    OsString::from(format!("--input={grader_input}")),
                    OsString::from(format!("--result={grader_result}")),
                    OsString::from(format!("--trial-id={}", self.manifest.trial_id())),
                    OsString::from(format!(
                        "--manifest-digest={}",
                        self.manifest.manifest_bytes_digest()
                    )),
                    OsString::from(format!("--agent-artifact-digest={artifact_digest}")),
                    OsString::from(format!("--usage-receipt={grader_input}/usage-receipt.json")),
                    OsString::from(format!(
                        "--artifact-auth-key={grader_input}/artifact-auth-key"
                    )),
                ];
                if let Some(nonce) = test_nonce {
                    command.push(OsString::from(format!(
                        "--test-manifest={grader_test_manifest}"
                    )));
                    command.push(OsString::from(format!("--test-nonce={nonce}")));
                }
                (
                    command,
                    &self.allocation.grader_output,
                    &self.allocation.grader_temp,
                    Some(self.allocation.grader_input.as_path()),
                    "grader",
                )
            }
        };
        #[cfg(windows)]
        if production_trial_route(self.profile) == ExecutionRoute::TrustedWindowsComposite {
            return self.execute_windows(
                profile,
                &request,
                command,
                build,
                temp,
                trusted_input,
                suffix,
            );
        }
        let plan = container::prepare_trial(
            profile,
            self.workspace,
            build,
            temp,
            &format!("{}-{suffix}", request.identity.instance_id),
            request.image_digest,
            command.iter().cloned(),
            container::TrialExecutionPins {
                instance_id: &request.identity.instance_id,
                rootfs_lease_id: &request.identity.rootfs_layer_id,
                writable_lease_id: &request.identity.writable_layer_id,
                trusted_read_only_input: trusted_input,
            },
        )
        .map_err(map_container_error)?;
        let report = match (self.owner, &self.cancellation) {
            (ProcessOwnership::Attempt(_), Some((coordinator, workspace))) => {
                match self.secret_broker {
                    Some((authorization, broker)) if !profile.credentials().is_empty() => plan
                        .run_registered_sequence_with_secret_broker(
                            self.owner,
                            authorization,
                            broker,
                            coordinator,
                            workspace.clone(),
                            self.process_registry.clone(),
                            request.phase == TrialPhase::Agent,
                        ),
                    _ => plan.run_registered(
                        self.owner,
                        coordinator,
                        workspace.clone(),
                        self.process_registry.clone(),
                        request.phase == TrialPhase::Agent,
                    ),
                }
            }
            (ProcessOwnership::Attempt(_), None) => {
                return Err(TrialError::Executor(
                    "attempt-owned production trials require cancellation coordination".to_owned(),
                ));
            }
            (ProcessOwnership::DaemonService(_), None) => match self.secret_broker {
                Some((authorization, broker)) if !profile.credentials().is_empty() => plan
                    .run_with_secret_broker(
                        self.owner,
                        authorization,
                        broker,
                        self.process_registry.clone(),
                    ),
                _ => plan.run_observed(self.owner, self.process_registry.clone()),
            },
            (ProcessOwnership::DaemonService(_), Some(_)) => {
                return Err(TrialError::Executor(
                    "daemon-owned production trials cannot use attempt cancellation context"
                        .to_owned(),
                ));
            }
        }
        .map_err(map_execution_error)?;
        Ok(BoundaryCompletion {
            phase: request.phase,
            route: ExecutionRoute::TrustedContainerHelper,
            image_digest: report.evidence.resolved_image_digest,
            boundary_id: report.evidence.boundary_id,
            instance_id: report.evidence.instance_id,
            rootfs_layer_id: report.evidence.rootfs_lease_id,
            writable_layer_id: report.evidence.writable_lease_id,
            plan_digest: report.evidence.plan_digest,
            invocation_digest: report.evidence.invocation_digest,
            runtime_identity: report.evidence.runtime_identity,
            helper_identity: report.evidence.helper_identity,
            permitted_profile_digest: request.permitted_profile_digest.to_owned(),
            survivor_processes: 0,
            quiescent: report.evidence.quiescent,
            outcome: report.outcome.into(),
        })
    }
}

impl ProductionBackend<'_> {
    #[cfg(windows)]
    fn execute_windows(
        &mut self,
        profile: &ExecutorProfile,
        request: &BoundaryRequest<'_>,
        command: Vec<OsString>,
        build: &Path,
        temp: &Path,
        trusted_input: Option<&Path>,
        suffix: &str,
    ) -> Result<BoundaryCompletion, TrialError> {
        let mut arguments = command.into_iter();
        let program = arguments
            .next()
            .ok_or_else(|| TrialError::Executor("Windows trial command is empty".to_owned()))?;
        let source_target = profile
            .mounts()
            .iter()
            .find(|mount| mount.role == MountRole::Source)
            .expect("validated profile has a source mount")
            .target
            .clone();
        let mut command = WindowsCommand::new(program)
            .current_dir(source_target)
            .mount_source(MountRole::Source, &self.workspace.path)
            .mount_source(MountRole::Build, build)
            .mount_source(MountRole::Temp, temp)
            .image_digest(request.image_digest)
            .storage_identity(
                &request.identity.instance_id,
                &request.identity.rootfs_layer_id,
                &request.identity.writable_layer_id,
            );
        for argument in arguments {
            command = command.arg(argument);
        }
        if let Some(path) = trusted_input {
            command =
                command.extra_mount("trusted_input", path, r"C:\kit-trusted-input", "read_only");
        }
        let record = temp.join(format!(".kit-windows-{suffix}-boundary"));
        let deadline = std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(
                profile.resources().wall_time_millis,
            ))
            .ok_or_else(|| TrialError::Executor("Windows trial deadline overflow".to_owned()))?;
        let mut process = match (self.owner, &self.cancellation) {
            (ProcessOwnership::Attempt(owner), Some((coordinator, workspace))) => {
                spawn_attempt_registered_sequence(
                    profile,
                    &command,
                    profile.resources(),
                    owner,
                    coordinator,
                    workspace.clone(),
                    self.process_registry.clone(),
                    |boundary: &crate::executor::process::tree::PersistedBoundary| {
                        write_new_synced(&record, boundary.encode().as_bytes())
                    },
                    deadline,
                    request.phase == TrialPhase::Agent,
                    self.secret_broker
                        .map(|(authorization, broker)| (self.workspace, authorization, broker)),
                )
            }
            (ProcessOwnership::Attempt(_), None) => {
                return Err(TrialError::Executor(
                    "attempt-owned Windows trials require cancellation coordination".to_owned(),
                ));
            }
            (owner @ ProcessOwnership::DaemonService(_), None) => match self.secret_broker {
                Some((authorization, broker)) if !self.profile.credentials().is_empty() => {
                    spawn_daemon_observed_with_secret_broker(
                        profile,
                        self.workspace,
                        &command,
                        profile.resources(),
                        owner,
                        authorization,
                        broker,
                        self.process_registry.clone(),
                        |boundary: &crate::executor::process::tree::PersistedBoundary| {
                            write_new_synced(&record, boundary.encode().as_bytes())
                        },
                        deadline,
                    )
                }
                _ => spawn_daemon_observed(
                    profile,
                    &command,
                    profile.resources(),
                    owner,
                    self.process_registry.clone(),
                    |boundary: &crate::executor::process::tree::PersistedBoundary| {
                        write_new_synced(&record, boundary.encode().as_bytes())
                    },
                    deadline,
                ),
            },
            (ProcessOwnership::DaemonService(_), Some(_)) => {
                return Err(TrialError::Executor(
                    "daemon-owned Windows trials cannot use attempt cancellation context"
                        .to_owned(),
                ));
            }
        }
        .map_err(|error| TrialError::Executor(error.to_string()))?;
        process
            .wait(deadline)
            .map_err(|error| TrialError::Executor(error.to_string()))?;
        let outcome = match process.record().state() {
            ProcessState::Exited { success: true, .. } => BoundaryOutcome::Success,
            ProcessState::Exited {
                code: Some(code), ..
            } => BoundaryOutcome::Exit(code),
            ProcessState::Exited { signal, .. } => BoundaryOutcome::Signal(signal.unwrap_or(0)),
            ProcessState::Started => {
                return Err(TrialError::Executor(
                    "Windows trial returned without an exit record".to_owned(),
                ));
            }
        };
        let plan_digest = process.plan_digest().to_owned();
        let invocation_digest = format!(
            "blake3:{}",
            blake3::hash(format!("{plan_digest}\0{}\0{suffix}", request.trial_id).as_bytes())
                .to_hex()
        );
        Ok(BoundaryCompletion {
            phase: request.phase,
            route: ExecutionRoute::TrustedWindowsComposite,
            image_digest: request.image_digest.to_owned(),
            boundary_id: process.boundary_id().to_owned(),
            instance_id: request.identity.instance_id.clone(),
            rootfs_layer_id: request.identity.rootfs_layer_id.clone(),
            writable_layer_id: request.identity.writable_layer_id.clone(),
            plan_digest,
            invocation_digest,
            runtime_identity: process.runtime_identity().to_owned(),
            helper_identity: process.helper_identity().to_owned(),
            permitted_profile_digest: request.permitted_profile_digest.to_owned(),
            survivor_processes: 0,
            quiescent: true,
            outcome,
        })
    }

    fn finish_boundary_sequence(&self) -> Result<(), TrialError> {
        let (ProcessOwnership::Attempt(owner), Some((coordinator, _))) =
            (self.owner, &self.cancellation)
        else {
            return Ok(());
        };
        coordinator
            .finish_boundary_sequence(owner)
            .map_err(|error| TrialError::Executor(error.to_string()))
    }
}

fn validate_workspace(
    manifest: &ImmutableTrialManifest,
    workspace: &AcquisitionResult,
) -> Result<(), TrialError> {
    if workspace.base_commit != manifest.repository_commit()
        || workspace.dirty_content != DirtyContent::SourceClean
    {
        return Err(TrialError::WorkspaceCommitMismatch);
    }
    let canonical = fs::canonicalize(&workspace.path).map_err(TrialError::Io)?;
    let metadata = fs::symlink_metadata(&workspace.path).map_err(TrialError::Io)?;
    if canonical != workspace.path || !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(TrialError::UnsafePath(workspace.path.clone()));
    }
    let head = git_output_bounded(
        &workspace.path,
        ["rev-parse", "--verify", "HEAD^{commit}"],
        128,
    )?;
    if String::from_utf8(head)
        .ok()
        .map(|value| value.trim().to_owned())
        .as_deref()
        != Some(workspace.base_commit.as_str())
        || !git_workspace_is_clean(&workspace.path)?
    {
        return Err(TrialError::WorkspaceCommitMismatch);
    }
    Ok(())
}

fn git_output_bounded<const N: usize>(
    repository: &Path,
    arguments: [&str; N],
    limit: usize,
) -> Result<Vec<u8>, TrialError> {
    let mut child = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(repository)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(TrialError::Io)?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .expect("Git stdout is piped")
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(TrialError::Io)?;
    if bytes.len() > limit {
        let _ = child.kill();
        let _ = child.wait();
        return Err(TrialError::WorkspaceIdentityOutputTooLarge);
    }
    if !child.wait().map_err(TrialError::Io)?.success() {
        return Err(TrialError::WorkspaceCommitMismatch);
    }
    Ok(bytes)
}

fn git_workspace_is_clean(repository: &Path) -> Result<bool, TrialError> {
    let output = git_output_bounded(
        repository,
        [
            "status",
            "--porcelain=v2",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        1,
    );
    match output {
        Ok(bytes) => Ok(bytes.is_empty()),
        Err(TrialError::WorkspaceIdentityOutputTooLarge) => Ok(false),
        Err(error) => Err(error),
    }
}

fn validate_grader_inputs(
    manifest: &ImmutableTrialManifest,
    workspace: &AcquisitionResult,
    inputs: GraderInputs<'_>,
) -> Result<(), TrialError> {
    let gold = manifest.gold_patch_digest();
    let harness = manifest.harness_config_digest();
    if inputs.harness_commit != manifest.grader_harness_commit() {
        return Err(TrialError::TrustedCommitPinMismatch("grader harness"));
    }
    for (actual, expected, name) in [
        (
            inputs.specification.expected_sha256,
            manifest.specification_digest(),
            "specification",
        ),
        (
            inputs.scaffold.expected_sha256,
            manifest.scaffold_digest(),
            "scaffold",
        ),
        (
            inputs.hidden_tests.expected_sha256,
            manifest.hidden_tests_digest(),
            "hidden tests",
        ),
        (inputs.gold_patch.expected_sha256, gold, "gold patch"),
        (
            inputs.acceptance_rules.expected_sha256,
            manifest.acceptance_digest(),
            "acceptance rules",
        ),
        (
            inputs.harness_config.expected_sha256,
            harness,
            "harness config",
        ),
    ] {
        if actual != expected {
            return Err(TrialError::TrustedInputPinMismatch(name));
        }
    }
    let mut total = 0_u64;
    for (name, input) in inputs.named() {
        let (digest, bytes) = hash_trusted_input(input.source, workspace)?;
        total = total
            .checked_add(bytes)
            .ok_or(TrialError::TrustedInputTooLarge)?;
        if total > MAX_INPUT_BYTES {
            return Err(TrialError::TrustedInputTooLarge);
        }
        if digest != input.expected_sha256 {
            return Err(TrialError::TrustedInputPinMismatch(name));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HiddenCanaryManifest {
    schema_version: u16,
    #[serde(rename = "checks")]
    _checks: Vec<serde_json::Value>,
    canaries: Vec<String>,
}

fn hidden_manifest_canaries(source: TrustedInputSource<'_>) -> Result<Vec<Vec<u8>>, TrialError> {
    let bytes = match source {
        TrustedInputSource::Bytes(bytes) => bytes.to_vec(),
        TrustedInputSource::Path(path) => read_regular_bounded(path, MAX_INPUT_BYTES as usize)?,
    };
    let manifest: HiddenCanaryManifest = serde_json::from_slice(&bytes)
        .map_err(|error| TrialError::InvalidGraderResult(error.to_string()))?;
    let mut unique = BTreeSet::new();
    let canaries = manifest
        .canaries
        .into_iter()
        .map(String::into_bytes)
        .collect::<Vec<_>>();
    if manifest.schema_version != 1
        || canaries.is_empty()
        || canaries.iter().any(|canary| {
            canary.is_empty() || canary.len() > 1024 || !unique.insert(canary.clone())
        })
    {
        return Err(TrialError::SensitiveArtifact);
    }
    Ok(canaries)
}

fn hash_trusted_input(
    source: TrustedInputSource<'_>,
    workspace: &AcquisitionResult,
) -> Result<(String, u64), TrialError> {
    match source {
        TrustedInputSource::Bytes(bytes) => {
            let len = bytes.len() as u64;
            if len > MAX_INPUT_BYTES {
                return Err(TrialError::TrustedInputTooLarge);
            }
            Ok((sha256(bytes), len))
        }
        TrustedInputSource::Path(path) => {
            let canonical = fs::canonicalize(path).map_err(TrialError::Io)?;
            let before = fs::symlink_metadata(path).map_err(TrialError::Io)?;
            if canonical != path
                || !before.is_file()
                || before.file_type().is_symlink()
                || canonical.starts_with(&workspace.managed_root)
                || workspace.managed_root.starts_with(&canonical)
                || before.len() > MAX_INPUT_BYTES
            {
                return Err(TrialError::UnsafePath(path.to_owned()));
            }
            let mut file = open_read_no_follow(path).map_err(TrialError::Io)?;
            let opened = file.metadata().map_err(TrialError::Io)?;
            if !same_filesystem_object(&before, &opened) {
                return Err(TrialError::PathIdentityChanged(path.to_owned()));
            }
            let (digest, bytes) = stream_hash(&mut file, MAX_INPUT_BYTES)?;
            let after = fs::symlink_metadata(path).map_err(TrialError::Io)?;
            if !same_filesystem_object(&before, &after) {
                return Err(TrialError::PathIdentityChanged(path.to_owned()));
            }
            Ok((digest, bytes))
        }
    }
}

fn stage_grader_inputs(
    inputs: GraderInputs<'_>,
    destination: &Path,
    agent_snapshot: &Path,
) -> Result<(), TrialError> {
    for (name, input) in inputs.named() {
        let target = destination.join(name);
        let digest = copy_trusted_input(input.source, &target)?;
        if digest != input.expected_sha256 {
            return Err(TrialError::TrustedInputPinMismatch(name));
        }
    }
    let output = destination.join("agent-output");
    create_owner_directory(&output)?;
    snapshot_tree(
        agent_snapshot,
        &output,
        MAX_OUTPUT_BYTES - MAX_GRADER_RESULT_BYTES,
    )?;
    Ok(())
}

fn copy_trusted_input(source: TrustedInputSource<'_>, target: &Path) -> Result<String, TrialError> {
    let mut output = new_owner_file(target)?;
    let mut hasher = Sha256::new();
    match source {
        TrustedInputSource::Bytes(bytes) => {
            if bytes.len() as u64 > MAX_INPUT_BYTES {
                return Err(TrialError::TrustedInputTooLarge);
            }
            output.write_all(bytes)?;
            hasher.update(bytes);
        }
        TrustedInputSource::Path(path) => {
            let before = fs::symlink_metadata(path)?;
            let mut input = open_read_no_follow(path)?;
            let opened = input.metadata()?;
            if !before.is_file()
                || before.file_type().is_symlink()
                || !same_filesystem_object(&before, &opened)
            {
                return Err(TrialError::PathIdentityChanged(path.to_owned()));
            }
            copy_hash_bounded(&mut input, &mut output, &mut hasher, MAX_INPUT_BYTES)?;
            let after = fs::symlink_metadata(path)?;
            if !same_filesystem_object(&before, &after) {
                return Err(TrialError::PathIdentityChanged(path.to_owned()));
            }
        }
    }
    output.sync_all()?;
    let mut permissions = output.metadata()?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(target, permissions)?;
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

struct GraderReceiptVerification<'a> {
    receipt: &'a TrialUsageReceipt,
    store: &'a dyn TrialUsageReceiptStore,
    usage: &'a VerifiedTrialUsage,
    auth_key: &'a [u8],
}

fn read_grader_result(
    path: &Path,
    manifest: &ImmutableTrialManifest,
    artifact_digest: &str,
    verification: GraderReceiptVerification<'_>,
) -> Result<ValidatedGraderResult, TrialError> {
    let GraderReceiptVerification {
        receipt,
        store: receipt_store,
        usage: verified_usage,
        auth_key,
    } = verification;
    let bytes = read_regular_bounded(path, MAX_GRADER_RESULT_BYTES as usize)?;
    let result: ValidatedGraderResult = serde_json::from_slice(&bytes)
        .map_err(|error| TrialError::InvalidGraderResult(error.to_string()))?;
    if result.schema_version != 1
        || result.trial_id != manifest.trial_id()
        || result.manifest_digest != manifest.manifest_bytes_digest()
        || result.agent_artifact_digest != artifact_digest
        || result.report.schema_version != 1
        || result.component_pins != manifest.component_pins()
    {
        return Err(TrialError::GraderResultBindingMismatch);
    }
    let root = path
        .parent()
        .ok_or(TrialError::GraderResultBindingMismatch)?
        .join(".kit-grader-artifacts");
    validate_grader_channels(&root, &result, manifest, artifact_digest, auth_key)?;
    let applied_patch =
        read_grader_channel(&root, &result.artifacts.applied_patch, MAX_OUTPUT_BYTES)?;
    let final_tree = read_grader_channel(&root, &result.artifacts.final_tree, MAX_OUTPUT_BYTES)?;
    let checks = read_grader_channel(&root, &result.artifacts.checks, MAX_OUTPUT_BYTES)?;
    let usage_bytes = read_grader_channel(
        &root,
        &result.artifacts.usage_report,
        MAX_GRADER_RESULT_BYTES,
    )?;
    let hidden_checks = decrypt_hidden_checks(
        auth_key,
        &read_grader_channel(&root, &result.artifacts.hidden_checks, MAX_OUTPUT_BYTES)?,
    )?;
    if result.report.patch_digest != sha256(&applied_patch)
        || result.report.final_tree_digest != sha256(&final_tree)
        || result.report.checks_digest != sha256(&checks)
        || !valid_sha256(&result.report.hidden.digest)
        || result.report.hidden.count != hidden_checks.len() as u64
        || result.report.hidden.digest
            != sha256(
                &serde_json::to_vec(&hidden_checks)
                    .map_err(|error| TrialError::Serialization(error.to_string()))?,
            )
        || match result.report.hidden.verdict {
            GraderVerdict::Pass => hidden_checks.iter().any(|check| !check.passed),
            GraderVerdict::Fail => hidden_checks.iter().all(|check| check.passed),
            GraderVerdict::Error => false,
        }
    {
        return Err(TrialError::GraderResultBindingMismatch);
    }
    let usage: GraderUsageReport = serde_json::from_slice(&usage_bytes)
        .map_err(|error| TrialError::InvalidGraderResult(error.to_string()))?;
    if usage.schema_version != 1
        || usage.usage != verified_usage.usage
        || usage.provider_request_ids != verified_usage.provider_request_ids
        || usage.usage_receipt != *receipt
        || usage.usage_evidence_digest != usage_evidence_digest(verified_usage)?
    {
        return Err(TrialError::GraderResultBindingMismatch);
    }
    manifest.validate_usage(usage.usage)?;
    validate_provider_request_ids(&usage.provider_request_ids)?;
    let echoed = receipt_store.verify(&usage.usage_receipt, manifest.trial_id())?;
    validate_verified_usage(manifest, &echoed)?;
    if echoed != *verified_usage {
        return Err(TrialError::GraderResultBindingMismatch);
    }
    Ok(result)
}

fn decrypt_hidden_checks(
    auth_key: &[u8],
    encrypted: &[u8],
) -> Result<Vec<RestrictedHiddenCheck>, TrialError> {
    const HEADER: &[u8] = b"kit-hidden-checks-v1\0";
    let encrypted = encrypted
        .strip_prefix(HEADER)
        .ok_or(TrialError::GraderResultBindingMismatch)?;
    let (nonce, ciphertext) = encrypted
        .split_at_checked(12)
        .filter(|(_, ciphertext)| ciphertext.len() >= 16)
        .ok_or(TrialError::GraderResultBindingMismatch)?;
    let key = Sha256::digest(auth_key);
    let plaintext = ChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| TrialError::GraderResultBindingMismatch)?
        .decrypt(nonce.into(), ciphertext)
        .map_err(|_| TrialError::GraderResultBindingMismatch)?;
    serde_json::from_slice(&plaintext).map_err(|_| TrialError::GraderResultBindingMismatch)
}

fn validate_grader_channels(
    root: &Path,
    result: &ValidatedGraderResult,
    manifest: &ImmutableTrialManifest,
    agent_artifact_digest: &str,
    auth_key: &[u8],
) -> Result<(), TrialError> {
    let mut handles = BTreeSet::new();
    let mut total = 0_u64;
    for (name, channel) in result.artifacts.named() {
        let expected_class = match name {
            "applied_patch" => "diff",
            "final_tree" => "file",
            "events" => "index",
            "logs" => "log",
            "hidden_checks" => "restricted_encrypted",
            "checks" | "agent_output" | "usage_report" => "report",
            _ => return Err(TrialError::GraderResultBindingMismatch),
        };
        if channel.class != expected_class
            || !safe_component(&channel.handle)
            || !handles.insert(&channel.handle)
            || channel.digest.len() != 71
            || channel.length > MAX_OUTPUT_BYTES
            || channel.authentication
                != grader_channel_authentication(
                    manifest.manifest_bytes_digest(),
                    agent_artifact_digest,
                    name,
                    channel,
                    auth_key,
                )?
        {
            return Err(TrialError::GraderResultBindingMismatch);
        }
        total = total
            .checked_add(channel.length)
            .ok_or(TrialError::GraderResultBindingMismatch)?;
        validate_grader_channel_file(root, channel)?;
    }
    if total > MAX_OUTPUT_BYTES || result.artifacts.logs.length > MAX_GRADER_RESULT_BYTES {
        return Err(TrialError::GraderResultBindingMismatch);
    }
    Ok(())
}

fn grader_channel_authentication(
    manifest_digest: &str,
    agent_artifact_digest: &str,
    name: &str,
    channel: &GraderArtifactHandle,
    auth_key: &[u8],
) -> Result<String, TrialError> {
    #[derive(Serialize)]
    struct Binding<'a> {
        schema_version: u16,
        manifest_digest: &'a str,
        agent_artifact_digest: &'a str,
        name: &'a str,
        class: &'a str,
        handle: &'a str,
        digest: &'a str,
        length: u64,
        auth_key_digest: String,
    }
    serde_json::to_vec(&Binding {
        schema_version: 1,
        manifest_digest,
        agent_artifact_digest,
        name,
        class: &channel.class,
        handle: &channel.handle,
        digest: &channel.digest,
        length: channel.length,
        auth_key_digest: sha256(auth_key),
    })
    .map(|bytes| sha256(&bytes))
    .map_err(|error| TrialError::Serialization(error.to_string()))
}

fn read_grader_channel(
    root: &Path,
    channel: &GraderArtifactHandle,
    limit: u64,
) -> Result<Vec<u8>, TrialError> {
    let limit = usize::try_from(limit).map_err(|_| TrialError::GraderResultBindingMismatch)?;
    let bytes = read_regular_bounded(&root.join(&channel.handle), limit)?;
    if bytes.len() as u64 != channel.length || sha256(&bytes) != channel.digest {
        return Err(TrialError::GraderResultBindingMismatch);
    }
    Ok(bytes)
}

fn validate_grader_channel_file(
    root: &Path,
    channel: &GraderArtifactHandle,
) -> Result<(), TrialError> {
    let path = root.join(&channel.handle);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != channel.length
    {
        return Err(TrialError::GraderResultBindingMismatch);
    }
    let mut file = open_read_no_follow(&path)?;
    if !same_filesystem_object(&metadata, &file.metadata()?) {
        return Err(TrialError::GraderResultBindingMismatch);
    }
    let (digest, bytes) = stream_hash(&mut file, channel.length)?;
    if bytes != channel.length || digest != channel.digest {
        return Err(TrialError::GraderResultBindingMismatch);
    }
    Ok(())
}

fn stage_usage_receipt(
    receipt: &TrialUsageReceipt,
    evidence: &VerifiedTrialUsage,
    path: &Path,
) -> Result<(), TrialError> {
    #[derive(Serialize)]
    struct Echo<'a> {
        schema_version: u16,
        receipt: &'a TrialUsageReceipt,
        evidence_digest: String,
    }
    let bytes = serde_json::to_vec(&Echo {
        schema_version: 1,
        receipt,
        evidence_digest: usage_evidence_digest(evidence)?,
    })
    .map_err(|error| TrialError::Serialization(error.to_string()))?;
    if bytes.len() > MAX_GRADER_RESULT_BYTES as usize {
        return Err(TrialError::InvalidUsageReceipt);
    }
    write_new_synced(path, &bytes)?;
    Ok(())
}

fn stage_grader_test_manifest(
    probe: GraderTestProbe,
    path: &Path,
    auth_key: &[u8],
    manifest_digest: &str,
    agent_artifact_digest: &str,
) -> Result<String, TrialError> {
    #[derive(Serialize)]
    struct Binding<'a> {
        schema_version: u16,
        nonce: &'a str,
        manifest_digest: &'a str,
        agent_artifact_digest: &'a str,
        canary_index: usize,
        channel: GraderTestChannel,
        encoding: GraderTestEncoding,
    }
    #[derive(Serialize)]
    struct Manifest<'a> {
        #[serde(flatten)]
        binding: Binding<'a>,
        authentication: String,
    }

    let nonce = random_hex()?;
    let binding = Binding {
        schema_version: 1,
        nonce: &nonce,
        manifest_digest,
        agent_artifact_digest,
        canary_index: 0,
        channel: probe.channel,
        encoding: probe.encoding,
    };
    let binding_bytes = serde_json::to_vec(&binding)
        .map_err(|error| TrialError::Serialization(error.to_string()))?;
    let mut authentication = Sha256::new();
    authentication.update(b"kit-grader-test-manifest-v1\0");
    authentication.update(auth_key);
    authentication.update([0]);
    authentication.update(&binding_bytes);
    let bytes = serde_json::to_vec(&Manifest {
        binding,
        authentication: format!("sha256:{:x}", authentication.finalize()),
    })
    .map_err(|error| TrialError::Serialization(error.to_string()))?;
    write_new_synced(path, &bytes)?;
    Ok(nonce)
}

fn usage_evidence_digest(evidence: &VerifiedTrialUsage) -> Result<String, TrialError> {
    serde_json::to_vec(evidence)
        .map(|bytes| sha256(&bytes))
        .map_err(|error| TrialError::Serialization(error.to_string()))
}

fn validate_verified_usage(
    manifest: &ImmutableTrialManifest,
    evidence: &VerifiedTrialUsage,
) -> Result<(), TrialError> {
    let expected_fence = manifest
        .wire
        .identity
        .attempt
        .checked_add(1)
        .ok_or(TrialError::Manifest(ManifestError::AttemptOverflow))?;
    let task_digest = sha256(
        &serde_json::to_vec(&manifest.wire.task)
            .map_err(|error| TrialError::Serialization(error.to_string()))?,
    );
    if evidence.binding.trial_id != manifest.trial_id()
        || evidence.binding.trial_digest != manifest.manifest_bytes_digest()
        || evidence.binding.task_digest != task_digest
        || evidence.binding.model_digest != manifest.wire.environment.model.model_digest
        || evidence.binding.config_digest != manifest.config_digest()
        || evidence.binding.attempt_fence != expected_fence
        || evidence.event_high_watermark == 0
        || evidence.terminal_version == 0
        || evidence.durable_event_positions.is_empty()
        || evidence
            .durable_event_positions
            .windows(2)
            .any(|positions| positions[0] >= positions[1])
        || evidence.durable_event_positions[0] == 0
    {
        return Err(TrialError::UsageReceiptMismatch);
    }
    validate_provider_request_ids(&evidence.provider_request_ids)?;
    manifest.validate_usage(evidence.usage)
}

fn validate_provider_request_ids(ids: &[String]) -> Result<(), TrialError> {
    if ids.len() > 10_000
        || ids.iter().any(|id| !valid_id(id))
        || ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
    {
        return Err(TrialError::InvalidProviderRequestIds);
    }
    Ok(())
}

fn scan_outward_artifacts(
    allocation: &TrialAllocation,
    canaries: &[&[u8]],
) -> Result<(), TrialError> {
    let leases = canaries
        .iter()
        .filter(|canary| !canary.is_empty())
        .map(|canary| SecretLease::new(canary.to_vec()))
        .collect::<Vec<_>>();
    if leases.is_empty() {
        return Ok(());
    }
    let redactor = CaptureRedactor::new(&leases);
    let mut scanner = redactor.scanner();
    scan_outward_tree(&allocation.agent_snapshot, &mut scanner)?;
    scan_outward_tree(&allocation.grader_output, &mut scanner)?;
    if scanner.found() {
        return Err(TrialError::SensitiveArtifact);
    }
    Ok(())
}

fn scan_outward_tree(root: &Path, scanner: &mut SensitiveDataScanner) -> Result<(), TrialError> {
    let mut pending = vec![root.to_owned()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
                return Err(TrialError::SensitiveArtifact);
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                files.push(entry.path());
            }
            if files.len() + pending.len() > MAX_ARTIFACT_FILES + MAX_ARTIFACT_DIRECTORIES {
                return Err(TrialError::ArtifactMetadataTooLarge);
            }
        }
    }
    files.sort();
    let mut buffer = [0_u8; 64 * 1024];
    for path in files {
        let mut file = open_read_no_follow(&path)?;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            scanner.push(&buffer[..read]);
            buffer[..read].fill(0);
            if scanner.found() {
                return Err(TrialError::SensitiveArtifact);
            }
        }
    }
    Ok(())
}

fn artifact_digest(artifacts: &[TrialArtifact]) -> Result<String, TrialError> {
    let bytes = serde_json::to_vec(artifacts)
        .map_err(|error| TrialError::Serialization(error.to_string()))?;
    if bytes.len() > MAX_ARTIFACT_METADATA_BYTES {
        return Err(TrialError::ArtifactMetadataTooLarge);
    }
    Ok(sha256(&bytes))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TrialArtifact {
    pub path: String,
    pub mode: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Copy)]
enum RestrictedArtifactPolicy {
    ValidatedGraderResult,
}

fn persist_restricted_artifact(
    policy: RestrictedArtifactPolicy,
    path: &Path,
    bytes: &[u8],
) -> io::Result<()> {
    match policy {
        RestrictedArtifactPolicy::ValidatedGraderResult
            if bytes.len() <= MAX_GRADER_RESULT_BYTES as usize =>
        {
            write_new_synced(path, bytes)
        }
        RestrictedArtifactPolicy::ValidatedGraderResult => {
            Err(io::Error::other("grader result exceeds artifact policy"))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TrialRecord {
    pub schema_version: u16,
    pub trial_id: String,
    pub manifest_identity_digest: String,
    pub manifest_bytes_digest: String,
    pub component_pins: TrialComponentPins,
    pub agent_image_digest: String,
    pub grader_image_digest: String,
    pub hidden_tests_digest: String,
    pub acceptance_digest: String,
    pub gold_patch_digest: String,
    pub harness_config_digest: String,
    pub vm_contract_digest: String,
    pub grader_vm_contract_digest: String,
    pub workspace_revision_digest: String,
    pub workspace_dirty_state_digest: String,
    pub identity: FreshTrialIdentity,
    pub agent: BoundaryCompletion,
    pub grader: BoundaryCompletion,
    pub agent_artifact_digest: String,
    pub grader_result: ValidatedGraderResult,
    pub helper_evidence_binding_digest: String,
    pub artifacts: Vec<TrialArtifact>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrialComponentPins {
    pub pins: BTreeMap<String, String>,
}

#[allow(clippy::too_many_arguments)]
fn persist_trial(
    manifest: &ImmutableTrialManifest,
    workspace: &AcquisitionResult,
    allocation: &TrialAllocation,
    vm_contract_digest: String,
    grader_vm_contract_digest: String,
    boundaries: BoundaryPair,
    agent_artifact_digest: String,
    grader_result: ValidatedGraderResult,
    profile: &ExecutorProfile,
    record_root: &Path,
) -> Result<DurableTrial, TrialError> {
    let nonce = random_hex()?;
    let staging = record_root.join(format!(".trial-{nonce}.tmp"));
    let reservation =
        reserve_trial_directory(&staging, &nonce, "record-staging").map_err(TrialError::Io)?;
    let result = (|| {
        let artifact_root = staging.join("artifacts");
        let agent_root = artifact_root.join("agent");
        create_owner_directory(&artifact_root)
            .and_then(|()| create_owner_directory(&agent_root))?;
        let mut artifacts = snapshot_tree(
            &allocation.agent_snapshot,
            &agent_root,
            profile
                .resources()
                .output_bytes
                .min(MAX_OUTPUT_BYTES - MAX_GRADER_RESULT_BYTES),
        )?;
        if artifact_digest(&artifacts)? != agent_artifact_digest {
            return Err(TrialError::AgentSnapshotChanged);
        }
        let grader_root = artifact_root.join("grader");
        create_owner_directory(&grader_root)?;
        for (_, channel) in grader_result.artifacts.named() {
            let source = allocation
                .grader_output
                .join(".kit-grader-artifacts")
                .join(&channel.handle);
            let target = grader_root.join(&channel.handle);
            let mut input = open_read_no_follow(&source)?;
            let metadata = input.metadata()?;
            if !metadata.is_file() || metadata.len() != channel.length {
                return Err(TrialError::GraderResultBindingMismatch);
            }
            let mut output = new_owner_file(&target)?;
            let mut hasher = Sha256::new();
            let bytes = copy_hash_bounded(&mut input, &mut output, &mut hasher, channel.length)?;
            output.sync_all()?;
            if bytes != channel.length
                || format!("sha256:{:x}", hasher.finalize()) != channel.digest
            {
                return Err(TrialError::GraderResultBindingMismatch);
            }
            artifacts.push(TrialArtifact {
                path: format!("artifacts/grader/{}", channel.handle),
                mode: ARTIFACT_MODE_REGULAR.to_owned(),
                sha256: channel.digest.clone(),
                bytes,
            });
        }
        let grader_bytes = serde_json::to_vec(&grader_result)
            .map_err(|error| TrialError::Serialization(error.to_string()))?;
        if grader_bytes.len() > MAX_GRADER_RESULT_BYTES as usize {
            return Err(TrialError::InvalidGraderResult(
                "result exceeds bound".to_owned(),
            ));
        }
        let grader_path = artifact_root.join("grader-result.json");
        persist_restricted_artifact(
            RestrictedArtifactPolicy::ValidatedGraderResult,
            &grader_path,
            &grader_bytes,
        )?;
        artifacts.push(TrialArtifact {
            path: "artifacts/grader-result.json".to_owned(),
            mode: ARTIFACT_MODE_REGULAR.to_owned(),
            sha256: sha256(&grader_bytes),
            bytes: grader_bytes.len() as u64,
        });
        artifacts.sort_by(|left, right| left.path.cmp(&right.path));

        let helper_evidence_binding_digest = helper_evidence_binding(
            manifest.manifest_bytes_digest(),
            &vm_contract_digest,
            &grader_vm_contract_digest,
            &boundaries,
            &agent_artifact_digest,
            &grader_result,
        )?;
        let record = TrialRecord {
            schema_version: RECORD_SCHEMA_VERSION,
            trial_id: manifest.trial_id().to_owned(),
            manifest_identity_digest: manifest.identity_digest().to_owned(),
            manifest_bytes_digest: manifest.manifest_bytes_digest().to_owned(),
            component_pins: manifest.component_pins(),
            agent_image_digest: manifest.agent_image_digest().to_owned(),
            grader_image_digest: manifest.grader_image_digest().to_owned(),
            hidden_tests_digest: manifest.hidden_tests_digest().to_owned(),
            acceptance_digest: manifest.acceptance_digest().to_owned(),
            gold_patch_digest: manifest.gold_patch_digest().to_owned(),
            harness_config_digest: manifest.harness_config_digest().to_owned(),
            vm_contract_digest,
            grader_vm_contract_digest,
            workspace_revision_digest: workspace.workspace_revision.hash.as_str().to_owned(),
            workspace_dirty_state_digest: workspace.initial_dirty_state.as_str().to_owned(),
            identity: allocation.identity.clone(),
            agent: boundaries.agent,
            grader: boundaries.grader,
            agent_artifact_digest,
            grader_result,
            helper_evidence_binding_digest,
            artifacts,
        };
        let mut bytes = serde_json::to_vec(&record)
            .map_err(|error| TrialError::Serialization(error.to_string()))?;
        bytes.push(b'\n');
        write_new_synced(&staging.join("record.json"), &bytes)?;
        sync_tree(&staging)?;
        let destination = record_root.join(format!(
            "{}-{}",
            manifest.trial_id(),
            allocation.identity.instance_id
        ));
        match fs::symlink_metadata(&destination) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(TrialError::AllocationCollision),
            Err(error) => return Err(TrialError::Io(error)),
        }
        fs::rename(&staging, &destination)?;
        fs::File::open(record_root)?.sync_all()?;
        Ok(DurableTrial {
            record_path: destination.join("record.json"),
            record_digest: sha256(&bytes),
            record,
        })
    })();
    match result {
        Ok(trial) => Ok(trial),
        Err(primary) => match cleanup_reservation(&reservation) {
            Ok(()) => Err(primary),
            Err(cleanup) => Err(TrialError::CleanupAfterFailure {
                primary: primary.to_string(),
                cleanup: cleanup.to_string(),
            }),
        },
    }
}

fn helper_evidence_binding(
    manifest_digest: &str,
    vm_contract_digest: &str,
    grader_vm_contract_digest: &str,
    boundaries: &BoundaryPair,
    artifact_digest: &str,
    grader_result: &ValidatedGraderResult,
) -> Result<String, TrialError> {
    #[derive(Serialize)]
    struct Binding<'a> {
        manifest_digest: &'a str,
        vm_contract_digest: &'a str,
        grader_vm_contract_digest: &'a str,
        agent: &'a BoundaryCompletion,
        grader: &'a BoundaryCompletion,
        artifact_digest: &'a str,
        grader_result: &'a ValidatedGraderResult,
    }
    let bytes = serde_json::to_vec(&Binding {
        manifest_digest,
        vm_contract_digest,
        grader_vm_contract_digest,
        agent: &boundaries.agent,
        grader: &boundaries.grader,
        artifact_digest,
        grader_result,
    })
    .map_err(|error| TrialError::Serialization(error.to_string()))?;
    Ok(sha256(&bytes))
}

fn snapshot_tree(source: &Path, destination: &Path, limit: u64) -> io::Result<Vec<TrialArtifact>> {
    let mut pending = vec![(source.to_owned(), PathBuf::new(), 0_usize)];
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut directory_count = 1_usize;
    while let Some((directory, relative, depth)) = pending.pop() {
        if depth > MAX_ARTIFACT_DEPTH {
            return Err(io::Error::other(
                "trial output exceeded directory depth bound",
            ));
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&directory)? {
            entries.push(entry?);
            if entries.len() > MAX_ARTIFACT_FILES + MAX_ARTIFACT_DIRECTORIES {
                return Err(io::Error::other("trial output exceeded entry bound"));
            }
        }
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries.into_iter().rev() {
            let name = entry.file_name();
            let name = name
                .to_str()
                .filter(|name| safe_component(name))
                .ok_or_else(|| io::Error::other("unsafe trial output path"))?;
            if name == ALLOCATION_MARKER
                || (relative.as_os_str().is_empty() && name == GRADER_RESULT_NAME)
            {
                continue;
            }
            let child_relative = relative.join(name);
            if child_relative.as_os_str().as_encoded_bytes().len() > MAX_ARTIFACT_PATH_BYTES {
                return Err(io::Error::other("trial output path exceeded bound"));
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() || !(metadata.is_dir() || metadata.is_file()) {
                return Err(io::Error::other(
                    "trial output contains a link or special file",
                ));
            }
            if metadata.is_dir() {
                directory_count += 1;
                if directory_count > MAX_ARTIFACT_DIRECTORIES {
                    return Err(io::Error::other("trial output exceeded directory bound"));
                }
                directories.push(child_relative.clone());
                pending.push((entry.path(), child_relative, depth + 1));
            } else {
                if files.len() >= MAX_ARTIFACT_FILES {
                    return Err(io::Error::other("trial output exceeded file count bound"));
                }
                files.push((entry.path(), child_relative));
            }
        }
    }
    files.sort_by(|left, right| left.1.cmp(&right.1));
    directories.sort();
    let mut total = 0_u64;
    let mut artifacts = Vec::with_capacity(files.len() + directories.len());
    let mut metadata_bytes = 0_usize;
    for relative in directories {
        let target = destination.join(&relative);
        if let Some(parent) = target.parent() {
            create_owner_parents(destination, parent)?;
        }
        create_owner_directory(&target)?;
        let path = format!("artifacts/agent/{}", relative.to_string_lossy());
        metadata_bytes = metadata_bytes
            .checked_add(path.len() + ARTIFACT_MODE_DIRECTORY.len() + 96)
            .ok_or_else(|| io::Error::other("artifact metadata size overflow"))?;
        if metadata_bytes > MAX_ARTIFACT_METADATA_BYTES {
            return Err(io::Error::other("artifact metadata exceeded bound"));
        }
        artifacts.push(TrialArtifact {
            path,
            mode: ARTIFACT_MODE_DIRECTORY.to_owned(),
            sha256: sha256(b""),
            bytes: 0,
        });
    }
    for (source, relative) in files {
        let before = fs::symlink_metadata(&source)?;
        let mode = artifact_file_mode(&before);
        let mut input = open_read_no_follow(&source)?;
        let opened = input.metadata()?;
        if !before.is_file()
            || before.file_type().is_symlink()
            || !same_artifact_metadata(&before, &opened)
        {
            return Err(io::Error::other("trial artifact identity changed"));
        }
        let target = destination.join(&relative);
        if let Some(parent) = target.parent() {
            create_owner_parents(destination, parent)?;
        }
        let mut output = new_owner_file(&target)?;
        let mut hasher = Sha256::new();
        let bytes = copy_hash_bounded(
            &mut input,
            &mut output,
            &mut hasher,
            limit.saturating_sub(total),
        )?;
        set_artifact_file_mode(&target, mode)?;
        output.sync_all()?;
        let after = fs::symlink_metadata(&source)?;
        if !same_artifact_metadata(&before, &after) {
            return Err(io::Error::other("trial artifact identity changed"));
        }
        total = total
            .checked_add(bytes)
            .ok_or_else(|| io::Error::other("trial output size overflow"))?;
        if total > limit {
            return Err(io::Error::other("trial output exceeded its artifact bound"));
        }
        let path = format!("artifacts/agent/{}", relative.to_string_lossy());
        metadata_bytes = metadata_bytes
            .checked_add(path.len() + mode.len() + 96)
            .ok_or_else(|| io::Error::other("artifact metadata size overflow"))?;
        if metadata_bytes > MAX_ARTIFACT_METADATA_BYTES {
            return Err(io::Error::other("artifact metadata exceeded bound"));
        }
        artifacts.push(TrialArtifact {
            path,
            mode: mode.to_owned(),
            sha256: format!("sha256:{:x}", hasher.finalize()),
            bytes,
        });
    }
    artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(artifacts)
}

#[cfg(unix)]
fn artifact_file_mode(metadata: &fs::Metadata) -> &'static str {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o111 == 0 {
        ARTIFACT_MODE_REGULAR
    } else {
        ARTIFACT_MODE_EXECUTABLE
    }
}

#[cfg(not(unix))]
fn artifact_file_mode(_metadata: &fs::Metadata) -> &'static str {
    ARTIFACT_MODE_REGULAR
}

#[cfg(unix)]
fn set_artifact_file_mode(path: &Path, mode: &str) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if mode == ARTIFACT_MODE_EXECUTABLE {
            0o700
        } else {
            0o600
        }),
    )
}

#[cfg(not(unix))]
fn set_artifact_file_mode(_path: &Path, _mode: &str) -> io::Result<()> {
    Ok(())
}

fn same_artifact_metadata(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_filesystem_object(left, right) && artifact_file_mode(left) == artifact_file_mode(right)
}

fn read_regular_bounded(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit as u64 {
        return Err(io::Error::other("artifact is not a bounded regular file"));
    }
    let file = open_read_no_follow(path)?;
    if !same_filesystem_object(&metadata, &file.metadata()?) {
        return Err(io::Error::other("artifact identity changed while opening"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(limit));
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::other(
            "artifact exceeded its bound while reading",
        ));
    }
    if !same_filesystem_object(&metadata, &fs::symlink_metadata(path)?) {
        return Err(io::Error::other("artifact identity changed while reading"));
    }
    Ok(bytes)
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = new_owner_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn create_owner_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn create_owner_parents(root: &Path, parent: &Path) -> io::Result<()> {
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| io::Error::other("artifact parent escaped destination"))?;
    let mut current = root.to_owned();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(io::Error::other("unsafe artifact parent"));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(io::Error::other("artifact parent is not a directory")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_owner_directory(&current)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn new_owner_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn copy_hash_bounded(
    input: &mut impl Read,
    output: &mut impl Write,
    hasher: &mut Sha256,
    limit: u64,
) -> io::Result<u64> {
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = checked_stream_total(total, read as u64, limit)?;
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    Ok(total)
}

fn checked_stream_total(total: u64, chunk: u64, limit: u64) -> io::Result<u64> {
    let total = total
        .checked_add(chunk)
        .ok_or_else(|| io::Error::other("stream size overflow"))?;
    if total > limit {
        return Err(io::Error::other("stream exceeded bound"));
    }
    Ok(total)
}

fn stream_hash(input: &mut impl Read, limit: u64) -> io::Result<(String, u64)> {
    let mut sink = io::sink();
    let mut hasher = Sha256::new();
    let bytes = copy_hash_bounded(input, &mut sink, &mut hasher, limit)?;
    Ok((format!("sha256:{:x}", hasher.finalize()), bytes))
}

fn open_read_no_follow(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x20000);
    }
    #[cfg(all(
        unix,
        any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        )
    ))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x100);
    }
    options.open(path)
}

#[cfg(unix)]
fn filesystem_identity(metadata: &fs::Metadata) -> Option<FilesystemIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(FilesystemIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn filesystem_identity(_metadata: &fs::Metadata) -> Option<FilesystemIdentity> {
    None
}

fn same_filesystem_object(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    filesystem_identity(left).is_some_and(|identity| Some(identity) == filesystem_identity(right))
}

fn sync_tree(root: &Path) -> io::Result<()> {
    let mut directories = vec![root.to_owned()];
    let mut index = 0;
    while index < directories.len() {
        for entry in fs::read_dir(&directories[index])? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                directories.push(entry.path());
            }
        }
        index += 1;
    }
    for directory in directories.into_iter().rev() {
        fs::File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn validate_record_root(
    root: &Path,
    workspace: &AcquisitionResult,
    allocation: &TrialAllocation,
) -> Result<PathBuf, TrialError> {
    let canonical = fs::canonicalize(root).map_err(TrialError::Io)?;
    if canonical != root
        || !fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_dir())
        || canonical.starts_with(&workspace.path)
        || workspace.path.starts_with(&canonical)
        || canonical.starts_with(&allocation.writable_path)
    {
        return Err(TrialError::UnsafePath(root.to_owned()));
    }
    Ok(canonical)
}

fn validate_manifest(manifest: &TrialManifestWire) -> Result<(), ManifestError> {
    if manifest.schema_version != "1.1"
        || manifest.kind != "trial"
        || manifest.task.schema_version != "1.0"
        || manifest.task.kind != "task"
        || manifest.environment.schema_version != "1.1"
        || manifest.environment.kind != "environment"
        || manifest.budget.schema_version != "1.0"
        || manifest.budget.kind != "budget"
        || manifest.cache_condition.schema_version != "1.0"
        || manifest.cache_condition.kind != "cache_condition"
        || manifest.grader.schema_version != "1.0"
        || manifest.grader.kind != "grader"
    {
        return Err(ManifestError::UnsupportedVersionOrKind);
    }
    for value in [
        &manifest.trial_id,
        &manifest.identity.randomization_id,
        &manifest.identity.task_id,
        &manifest.identity.environment_id,
        &manifest.identity.budget_id,
        &manifest.identity.cache_condition_id,
        &manifest.identity.grader_id,
        &manifest.identity.config_id,
        &manifest.task.task_id,
        &manifest.task.task_version,
        &manifest.environment.environment_id,
        &manifest.budget.budget_id,
        &manifest.cache_condition.cache_condition_id,
        &manifest.grader.grader_id,
        &manifest.grader.grader_version,
    ] {
        if !valid_id(value) {
            return Err(ManifestError::InvalidId(value.clone()));
        }
    }
    if manifest.identity.task_id != manifest.task.task_id
        || manifest.identity.environment_id != manifest.environment.environment_id
        || manifest.identity.budget_id != manifest.budget.budget_id
        || manifest.identity.cache_condition_id != manifest.cache_condition.cache_condition_id
        || manifest.identity.grader_id != manifest.grader.grader_id
    {
        return Err(ManifestError::ComponentIdentityMismatch);
    }
    let canonical = format!(
        "kit-trial-identity-v1\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
        manifest.identity.randomization_id,
        manifest.identity.attempt,
        manifest.identity.task_id,
        manifest.identity.environment_id,
        manifest.identity.budget_id,
        manifest.identity.cache_condition_id,
        manifest.identity.grader_id,
        manifest.identity.config_id,
    );
    if manifest.identity.canonical_digest != sha256(canonical.as_bytes()) {
        return Err(ManifestError::CanonicalIdentityMismatch);
    }
    for digest in [
        &manifest.task.specification_digest,
        &manifest.task.scaffold_digest,
        &manifest.environment.image_digest,
        &manifest.environment.model.model_digest,
        &manifest.environment.model.settings_digest,
        &manifest.environment.model.provider_capability_digest,
        &manifest.environment.components.prompt_digest,
        &manifest.environment.components.tools_digest,
        &manifest.environment.components.router_digest,
        &manifest.environment.components.retry_policy_digest,
        &manifest.environment.components.verifier_digest,
        &manifest.grader.image_digest,
        &manifest.grader.hidden_tests_digest,
        &manifest.grader.acceptance_digest,
        &manifest.grader.gold_patch_digest,
        &manifest.grader.harness_config_digest,
    ] {
        if !valid_sha256(digest) {
            return Err(ManifestError::InvalidDigest((*digest).clone()));
        }
    }
    for cache in [
        &manifest.cache_condition.prompt,
        &manifest.cache_condition.infrastructure,
    ] {
        if let CacheStateWire::Warm { state_digest } = cache
            && !valid_sha256(state_digest)
        {
            return Err(ManifestError::InvalidDigest(state_digest.clone()));
        }
    }
    if !matches!(
        manifest.environment.model.reasoning_effort.as_str(),
        "none" | "low" | "medium" | "high"
    ) || !valid_id(&manifest.environment.model.provider)
        || manifest.environment.model.name.is_empty()
        || manifest.environment.model.snapshot.is_empty()
    {
        return Err(ManifestError::InvalidModel);
    }
    let commit = match &manifest.task.repository {
        RepositoryWire::Https { url, commit } => {
            if !valid_repository_url(url, "https://") {
                return Err(ManifestError::InvalidRepository);
            }
            commit
        }
        RepositoryWire::Ssh { url, commit } => {
            if !valid_repository_url(url, "ssh://") {
                return Err(ManifestError::InvalidRepository);
            }
            commit
        }
        RepositoryWire::LocalFixture {
            fixture,
            commit,
            fixture_grant,
        } => {
            if !valid_id(fixture) || !valid_id(fixture_grant) {
                return Err(ManifestError::InvalidRepository);
            }
            commit
        }
    };
    if !valid_commit(commit) || !valid_commit(&manifest.grader.harness_commit) {
        return Err(ManifestError::InvalidCommit);
    }
    let limits = &manifest.budget.limits;
    if limits.cpu_seconds == 0
        || limits.memory_bytes == 0
        || limits.disk_bytes == 0
        || limits.processes == 0
        || limits.wall_seconds == 0
        || limits.cpu_seconds > MAX_CPU_SECONDS
        || limits.memory_bytes > MAX_MEMORY_BYTES
        || limits.disk_bytes > MAX_DISK_BYTES
        || limits.processes > MAX_PIDS
        || limits.wall_seconds > MAX_WALL_SECONDS
        || limits.turns == 0
        || limits.tokens == 0
        || !limits.dollars_usd.is_finite()
        || limits.dollars_usd <= 0.0
        || limits.dollars_usd > u64::MAX as f64 / 1_000_000.0
    {
        return Err(ManifestError::UnboundedExecutionBudget);
    }
    if limits.network_bytes != 0 {
        return Err(ManifestError::NetworkBudgetForbidden);
    }
    Ok(())
}

fn valid_repository_url(value: &str, scheme: &str) -> bool {
    let Some(rest) = value.strip_prefix(scheme) else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty()
        || authority.contains('@')
        || authority
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || value.contains(['?', '#'])
    {
        return false;
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((host, suffix)) = bracketed.split_once(']') else {
            return false;
        };
        let port = if suffix.is_empty() {
            None
        } else if let Some(port) = suffix.strip_prefix(':') {
            Some(port)
        } else {
            return false;
        };
        (host, port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if host.contains(':') {
            return false;
        }
        (host, Some(port))
    } else {
        (authority, None)
    };
    if port.is_some_and(|port| port.parse::<u16>().map_or(true, |port| port == 0)) {
        return false;
    }
    if host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.is_empty()
        || !host.is_ascii()
    {
        return false;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_unspecified())
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            !(ip.is_loopback() || ip.is_unspecified() || ip.is_unique_local())
        }
        Err(_) => host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        }),
    }
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.bytes().any(|byte| byte.is_ascii_control())
        && !value.contains('/')
        && !value.contains('\\')
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && (value.as_bytes()[0].is_ascii_lowercase() || value.as_bytes()[0].is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn valid_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn random_hex() -> Result<String, TrialError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| TrialError::RandomnessUnavailable)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn random_bytes(length: usize) -> Result<Vec<u8>, TrialError> {
    let mut bytes = vec![0_u8; length];
    getrandom::fill(&mut bytes).map_err(|_| TrialError::RandomnessUnavailable)?;
    Ok(bytes)
}

fn random_hex_io() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn map_container_error(error: ContainerError) -> TrialError {
    match error {
        ContainerError::NotAvailable(error) => TrialError::Unavailable(error),
        error => TrialError::Executor(error.to_string()),
    }
}

fn map_execution_error(error: ExecutionError) -> TrialError {
    match error {
        ExecutionError::NotAvailable(error) => TrialError::Unavailable(error),
        error => TrialError::Executor(error.to_string()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestError {
    Json(String),
    UnsupportedVersionOrKind,
    InvalidId(String),
    InvalidDigest(String),
    InvalidCommit,
    InvalidRepository,
    InvalidModel,
    ComponentIdentityMismatch,
    CanonicalIdentityMismatch,
    UnboundedExecutionBudget,
    NetworkBudgetForbidden,
    BudgetOverflow(&'static str),
    AttemptOverflow,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "invalid Phase 0 trial manifest: {error}"),
            Self::UnsupportedVersionOrKind => {
                formatter.write_str("unsupported Phase 0 manifest version or kind")
            }
            Self::InvalidId(id) => write!(formatter, "invalid Phase 0 manifest id {id}"),
            Self::InvalidDigest(digest) => write!(formatter, "invalid pinned digest {digest}"),
            Self::InvalidCommit => formatter.write_str("invalid pinned commit"),
            Self::InvalidRepository => formatter.write_str("invalid repository source"),
            Self::InvalidModel => formatter.write_str("invalid model snapshot"),
            Self::ComponentIdentityMismatch => {
                formatter.write_str("trial identity does not match embedded components")
            }
            Self::CanonicalIdentityMismatch => {
                formatter.write_str("trial canonical identity digest mismatch")
            }
            Self::UnboundedExecutionBudget => {
                formatter.write_str("trial execution budget must be finite and non-zero")
            }
            Self::NetworkBudgetForbidden => {
                formatter.write_str("trial runner supports only deny-by-default network")
            }
            Self::BudgetOverflow(name) => write!(formatter, "trial {name} budget overflows"),
            Self::AttemptOverflow => formatter.write_str("trial attempt fence overflows"),
        }
    }
}

impl std::error::Error for ManifestError {}

#[derive(Debug)]
pub enum TrialError {
    Manifest(ManifestError),
    Profile(ProfileError),
    VmContract(VmContractError),
    Unavailable(NotAvailable),
    Executor(String),
    BoundaryIdentityMismatch(TrialPhase),
    BoundaryNotQuiescent(TrialPhase),
    BoundaryFailed(TrialPhase, BoundaryOutcome),
    ReservedOutputCreatedByAgent,
    WorkspaceCommitMismatch,
    WorkspaceIdentityOutputTooLarge,
    TrustedInputPinMismatch(&'static str),
    TrustedCommitPinMismatch(&'static str),
    TrustedInputTooLarge,
    PathIdentityChanged(PathBuf),
    MissingAgentSnapshot,
    AgentSnapshotChanged,
    InvalidGraderResult(String),
    GraderResultBindingMismatch,
    UsageUnavailable(&'static str, UsageUnavailableReason),
    UsageBudgetExceeded,
    InvalidUsageReceipt,
    UsageReceiptMismatch,
    InvalidProviderRequestIds,
    SensitiveArtifact,
    ArtifactMetadataTooLarge,
    UnsafePath(PathBuf),
    AllocationCollision,
    RandomnessUnavailable,
    Serialization(String),
    Cleanup(io::Error),
    CleanupAfterPersistence {
        record_path: PathBuf,
        cleanup: String,
    },
    CleanupAfterFailure {
        primary: String,
        cleanup: String,
    },
    Io(io::Error),
}

impl fmt::Display for TrialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(error) => error.fmt(formatter),
            Self::Profile(error) => error.fmt(formatter),
            Self::VmContract(error) => error.fmt(formatter),
            Self::Unavailable(error) => error.fmt(formatter),
            Self::Executor(error) => write!(formatter, "isolated trial executor failed: {error}"),
            Self::BoundaryIdentityMismatch(phase) => {
                write!(
                    formatter,
                    "{phase:?} boundary identity or image digest mismatch"
                )
            }
            Self::BoundaryNotQuiescent(phase) => {
                write!(formatter, "{phase:?} boundary is not quiescent")
            }
            Self::BoundaryFailed(phase, outcome) => {
                write!(formatter, "{phase:?} boundary failed with {outcome:?}")
            }
            Self::ReservedOutputCreatedByAgent => {
                formatter.write_str("agent created the grader-reserved output path")
            }
            Self::WorkspaceCommitMismatch => {
                formatter.write_str("workspace commit does not match the immutable trial manifest")
            }
            Self::WorkspaceIdentityOutputTooLarge => {
                formatter.write_str("workspace identity command exceeded its trusted bound")
            }
            Self::TrustedInputPinMismatch(name) => {
                write!(
                    formatter,
                    "trusted {name} bytes do not match the manifest pin"
                )
            }
            Self::TrustedCommitPinMismatch(name) => {
                write!(
                    formatter,
                    "trusted {name} commit does not match the manifest pin"
                )
            }
            Self::TrustedInputTooLarge => formatter.write_str("trusted grader inputs exceed bound"),
            Self::PathIdentityChanged(path) => {
                write!(
                    formatter,
                    "trusted path identity changed: {}",
                    path.display()
                )
            }
            Self::MissingAgentSnapshot => formatter.write_str("agent snapshot was not produced"),
            Self::AgentSnapshotChanged => {
                formatter.write_str("agent snapshot changed after grading")
            }
            Self::InvalidGraderResult(error) => write!(formatter, "invalid grader result: {error}"),
            Self::GraderResultBindingMismatch => {
                formatter.write_str("grader result is not bound to this trial and artifact")
            }
            Self::UsageUnavailable(name, reason) => {
                write!(
                    formatter,
                    "required {name} usage is unavailable: {reason:?}"
                )
            }
            Self::UsageBudgetExceeded => formatter.write_str("trial model usage exceeded budget"),
            Self::InvalidUsageReceipt => formatter.write_str("trial usage receipt is invalid"),
            Self::UsageReceiptMismatch => {
                formatter.write_str("trial usage receipt does not match durable accounting")
            }
            Self::InvalidProviderRequestIds => {
                formatter.write_str("provider request IDs are invalid")
            }
            Self::SensitiveArtifact => {
                formatter.write_str("outward trial artifact contained restricted material")
            }
            Self::ArtifactMetadataTooLarge => {
                formatter.write_str("artifact metadata exceeds trusted bound")
            }
            Self::UnsafePath(path) => write!(formatter, "unsafe trial path {}", path.display()),
            Self::AllocationCollision => formatter.write_str("cannot allocate a fresh trial layer"),
            Self::RandomnessUnavailable => {
                formatter.write_str("cryptographic trial identity randomness is unavailable")
            }
            Self::Serialization(error) => {
                write!(formatter, "trial record serialization failed: {error}")
            }
            Self::Cleanup(error) => write!(formatter, "trial cleanup failed: {error}"),
            Self::CleanupAfterPersistence {
                record_path,
                cleanup,
            } => write!(
                formatter,
                "trial persisted at {} but cleanup failed: {cleanup}",
                record_path.display()
            ),
            Self::CleanupAfterFailure { primary, cleanup } => {
                write!(formatter, "{primary}; trial cleanup also failed: {cleanup}")
            }
            Self::Io(error) => write!(formatter, "trial persistence failed: {error}"),
        }
    }
}

impl std::error::Error for TrialError {}

impl From<io::Error> for TrialError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt_binding(manifest: &ImmutableTrialManifest) -> TrialUsageReceiptBinding {
        TrialUsageReceiptBinding {
            run_id: "run_00000000000000000000000001".to_owned(),
            trial_id: manifest.trial_id().to_owned(),
            trial_digest: manifest.manifest_bytes_digest().to_owned(),
            task_digest: sha256(&serde_json::to_vec(&manifest.wire.task).unwrap()),
            model_digest: manifest.wire.environment.model.model_digest.clone(),
            config_digest: manifest.config_digest(),
            attempt_id: "attempt_00000000000000000000000001".to_owned(),
            attempt_fence: manifest.wire.identity.attempt + 1,
            scheduler_principal_id: "principal_00000000000000000000000001".to_owned(),
            scheduler_idempotency_key: "trial-test".to_owned(),
        }
    }

    fn temporary_directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kit-trial-unit-{name}-{}-{}",
            std::process::id(),
            random_hex().unwrap()
        ));
        create_owner_directory(&path).unwrap();
        fs::canonicalize(path).unwrap()
    }

    #[test]
    fn grader_channel_stream_limit_accepts_512_mib_and_rejects_one_byte_more() {
        let mut total = 0;
        for _ in 0..8192 {
            total = checked_stream_total(total, 64 * 1024, MAX_OUTPUT_BYTES).unwrap();
        }
        assert_eq!(total, MAX_OUTPUT_BYTES);
        assert!(checked_stream_total(total, 1, MAX_OUTPUT_BYTES).is_err());
    }

    #[test]
    fn usage_receipt_evidence_mismatch_unavailable_and_overbudget_are_nonpass() {
        let manifest = ImmutableTrialManifest::from_phase0_bytes(include_bytes!(
            "../../../eval/manifests/examples/trial.json"
        ))
        .unwrap();
        let binding = receipt_binding(&manifest);
        let evidence = |usage| VerifiedTrialUsage {
            binding: binding.clone(),
            provider_request_ids: vec!["provider-request-1".to_owned()],
            durable_event_positions: vec![41, 42],
            event_high_watermark: 42,
            terminal_version: 1,
            usage,
        };
        assert!(validate_verified_usage(&manifest, &evidence(TrialUsage::ZERO)).is_ok());

        let mut unavailable = TrialUsage::ZERO;
        unavailable.input_tokens =
            UsageMeasure::Unavailable(UsageUnavailableReason::SchedulerEvidenceMissing);
        assert!(matches!(
            validate_verified_usage(&manifest, &evidence(unavailable)),
            Err(TrialError::UsageUnavailable(_, _))
        ));

        let mut overbudget = TrialUsage::ZERO;
        overbudget.output_tokens = UsageMeasure::Measured(u64::MAX);
        assert!(matches!(
            validate_verified_usage(&manifest, &evidence(overbudget)),
            Err(TrialError::UsageBudgetExceeded)
        ));

        let mut mismatched = evidence(TrialUsage::ZERO);
        mismatched.binding.attempt_fence += 1;
        assert!(matches!(
            validate_verified_usage(&manifest, &mismatched),
            Err(TrialError::UsageReceiptMismatch)
        ));
    }

    #[test]
    fn grader_profile_enforces_minimum_of_manifest_and_harness_bounds() {
        let manifest = ImmutableTrialManifest::from_phase0_bytes(include_bytes!(
            "../../../eval/manifests/examples/trial.json"
        ))
        .unwrap();
        let base = manifest.profile().unwrap();
        let grader = constrained_grader_profile(
            &manifest,
            GraderResourceBounds {
                memory_bytes: base.resources().memory_bytes / 2,
                output_bytes: base.resources().output_bytes / 2,
                wall_time_millis: base.resources().wall_time_millis / 2,
            },
        )
        .unwrap();
        assert_eq!(
            grader.resources().memory_bytes,
            base.resources().memory_bytes / 2
        );
        assert_eq!(
            grader.resources().output_bytes,
            base.resources().output_bytes / 2
        );
        assert_eq!(
            grader.resources().wall_time_millis,
            base.resources().wall_time_millis / 2
        );
        assert_ne!(grader.digest(), base.digest());
    }

    #[cfg(unix)]
    #[test]
    fn partial_allocation_collision_removes_only_owned_paths() {
        let parent = temporary_directory("collision");
        let nonce = "a".repeat(32);
        let collision = parent.join(format!("trial-grader-temp-{nonce}"));
        create_owner_directory(&collision).unwrap();
        fs::write(collision.join("sentinel"), b"unowned").unwrap();
        assert!(matches!(
            TrialAllocation::allocate_with_nonce(&parent, &nonce),
            Err(TrialError::AllocationCollision)
        ));
        assert_eq!(fs::read(collision.join("sentinel")).unwrap(), b"unowned");
        assert!(!parent.join(format!("trial-writable-{nonce}")).exists());
        assert!(!parent.join(format!("trial-agent-temp-{nonce}")).exists());
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn grader_result_rejects_unknown_and_mismatched_fields() {
        let manifest = ImmutableTrialManifest::from_phase0_bytes(include_bytes!(
            "../../../eval/manifests/examples/trial.json"
        ))
        .unwrap();
        let root = temporary_directory("grader-result");
        let path = root.join("result.json");
        fs::write(
            &path,
            format!(
                "{{\"schema_version\":1,\"trial_id\":\"wrong\",\"manifest_digest\":\"{}\",\"agent_artifact_digest\":\"sha256:{}\",\"verdict\":\"pass\",\"opaque\":true}}",
                manifest.manifest_bytes_digest(),
                "0".repeat(64)
            ),
        )
        .unwrap();
        struct Missing;
        impl TrialUsageReceiptStore for Missing {
            fn verify(
                &self,
                _: &TrialUsageReceipt,
                _: &str,
            ) -> Result<VerifiedTrialUsage, TrialError> {
                Err(TrialError::UsageReceiptMismatch)
            }
        }
        let receipt = TrialUsageReceipt::parse("test-usage-receipt").unwrap();
        let binding = receipt_binding(&manifest);
        let evidence = VerifiedTrialUsage {
            binding: binding.clone(),
            provider_request_ids: Vec::new(),
            durable_event_positions: vec![1],
            event_high_watermark: 1,
            terminal_version: 1,
            usage: TrialUsage::ZERO,
        };
        assert!(matches!(
            read_grader_result(
                &path,
                &manifest,
                &format!("sha256:{}", "0".repeat(64)),
                GraderReceiptVerification {
                    receipt: &receipt,
                    store: &Missing,
                    usage: &evidence,
                    auth_key: b"private-test-artifact-key",
                },
            ),
            Err(TrialError::InvalidGraderResult(_))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn container_bound_errors_cannot_be_accepted_as_trial_results() {
        use crate::executor::backends::container::limits::{BoundError, ResourceIdentity};

        assert!(matches!(
            map_execution_error(ExecutionError::Bound(BoundError::new(
                ResourceIdentity::Cpu,
                1,
                Some(2),
                "trusted monitor",
            ))),
            TrialError::Executor(_)
        ));
    }

    #[test]
    fn production_trial_route_reaches_windows_composite_for_windows_profiles() {
        let resources = ResourceLimits::new(1, 1, 1, 1, 1, 1, 1, 1);
        let windows = ExecutorProfile::new(ProfileSpec::isolated(
            TrustTier::Restricted,
            Platform::Windows,
            Architecture::X86_64,
            resources,
        ))
        .unwrap();
        let linux = ExecutorProfile::new(ProfileSpec::isolated(
            TrustTier::Restricted,
            Platform::Linux,
            Architecture::X86_64,
            resources,
        ))
        .unwrap();
        assert_eq!(
            production_trial_route(&windows),
            ExecutionRoute::TrustedWindowsComposite
        );
        assert_eq!(
            production_trial_route(&linux),
            ExecutionRoute::TrustedContainerHelper
        );
    }

    #[test]
    fn regular_artifact_mode_is_normalized() {
        let root = temporary_directory("regular-mode");
        let source = root.join("source");
        let destination = root.join("destination");
        create_owner_directory(&source).unwrap();
        create_owner_directory(&destination).unwrap();
        fs::write(source.join("result"), b"bytes").unwrap();

        let artifacts = snapshot_tree(&source, &destination, 1024).unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].mode, ARTIFACT_MODE_REGULAR);
        assert_eq!(fs::read(destination.join("result")).unwrap(), b"bytes");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn executable_agent_tree_reaches_grader_input_with_exact_shape() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary_directory("grader-executable");
        let source = root.join("source");
        let snapshot = root.join("snapshot");
        let grader_input = root.join("grader-input");
        for path in [&source, &snapshot, &grader_input] {
            create_owner_directory(path).unwrap();
        }
        create_owner_directory(&source.join("bin")).unwrap();
        create_owner_directory(&source.join("empty")).unwrap();
        fs::write(source.join("README"), b"result").unwrap();
        let executable = source.join("bin/grader-input");
        fs::write(&executable, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let artifacts = snapshot_tree(&source, &snapshot, 1024).unwrap();
        assert_eq!(
            artifacts
                .iter()
                .find(|artifact| artifact.path.ends_with("bin/grader-input"))
                .unwrap()
                .mode,
            ARTIFACT_MODE_EXECUTABLE
        );
        assert!(artifacts.iter().any(|artifact| {
            artifact.path.ends_with("empty") && artifact.mode == ARTIFACT_MODE_DIRECTORY
        }));

        const EMPTY: &str =
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let input = TrustedInput {
            source: TrustedInputSource::Bytes(b""),
            expected_sha256: EMPTY,
        };
        stage_grader_inputs(
            GraderInputs {
                specification: input,
                scaffold: input,
                hidden_tests: input,
                gold_patch: input,
                acceptance_rules: input,
                harness_config: input,
                harness_commit: "unused-in-staging",
            },
            &grader_input,
            &snapshot,
        )
        .unwrap();

        let restored = grader_input.join("agent-output");
        assert_eq!(fs::read(restored.join("README")).unwrap(), b"result");
        assert_eq!(
            fs::read(restored.join("bin/grader-input")).unwrap(),
            b"#!/bin/sh\n"
        );
        assert!(restored.join("empty").is_dir());
        assert_ne!(
            fs::metadata(restored.join("bin/grader-input"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn mode_only_changes_alter_agent_artifact_digest() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary_directory("mode-digest");
        let source = root.join("source");
        let first = root.join("first");
        let second = root.join("second");
        for path in [&source, &first, &second] {
            create_owner_directory(path).unwrap();
        }
        let path = source.join("tool");
        fs::write(&path, b"same bytes").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let regular = snapshot_tree(&source, &first, 1024).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let executable = snapshot_tree(&source, &second, 1024).unwrap();

        assert_eq!(regular[0].sha256, executable[0].sha256);
        assert_ne!(regular[0].mode, executable[0].mode);
        assert_ne!(
            artifact_digest(&regular).unwrap(),
            artifact_digest(&executable).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
