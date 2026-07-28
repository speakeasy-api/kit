use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString, c_void},
    fmt,
    fs::File,
    io::{self, Read},
    mem,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::{Path, PathBuf},
    process::Command,
    ptr,
    sync::Arc,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, FILETIME, GetLastError, HANDLE,
        HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::SECURITY_ATTRIBUTES,
    System::{
        JobObjects::{
            CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
            JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation, OpenJobObjectW,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        Pipes::CreatePipe,
        SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE},
        Threading::{
            CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcessId,
            GetExitCodeProcess, GetProcessId, GetProcessTimes, InitializeProcThreadAttributeList,
            OpenProcess, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, ResumeThread,
            STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
            WaitForSingleObject,
        },
    },
};

use crate::{
    domain::{
        ids::{CommandId, ProcessId},
        lifecycle::{AttemptOwnership, ProcessClaim, ProcessOwnership},
    },
    executor::{
        cancel::{CancellationIntent, SqliteCancellationCoordinator, WorkspaceIdentity},
        process::own::{
            BackendBoundaryToken, CaptureThreads, OutputBudget, ProcessOutput, ProcessRecord,
            ProcessRegistryRegistration, settle_custody,
        },
        process::tree::{
            BoundaryControl, BoundaryIdentity, BoundaryKind, BoundaryPersistence, Containment,
            Inspection, Ownership, PersistedBoundary,
        },
        profile::{ExecutorProfile, MountRole, ResourceLimits},
        secrets::{
            ExecutorSecretBroker, PreparationError, PreparedSecrets, SecretAuthorization,
            SecretSpawnPlan, resolve_for_spawn,
        },
        terminal::ConPtyBinding,
    },
    telemetry::redact::{CaptureBoundary, CapturePersistencePolicy, CaptureRedactor},
    workspace::acquire::AcquisitionResult,
};

use super::runtime::{LaunchMount, LaunchPlan, RuntimeBoundary, SpawnAttestation, TerminalPlan};

#[derive(Debug)]
pub enum JobError {
    InvalidCommand(&'static str),
    InvalidLimit(&'static str),
    OwnershipMismatch,
    PlatformUnavailable(io::Error),
    Io(io::Error),
    OutcomeUnknown(String),
    CredentialBrokerUnavailable,
    Secret(PreparationError),
}

fn runtime_spawn_error(error: super::runtime::RuntimeUnavailable) -> JobError {
    match error.reason {
        super::runtime::UnavailableReason::CredentialBrokerUnavailable => {
            JobError::CredentialBrokerUnavailable
        }
        super::runtime::UnavailableReason::OutcomeUnknown => JobError::OutcomeUnknown(error.detail),
        _ => JobError::PlatformUnavailable(io::Error::other(error.to_string())),
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommand(detail) => write!(formatter, "invalid Windows command: {detail}"),
            Self::InvalidLimit(limit) => write!(formatter, "invalid Windows Job limit: {limit}"),
            Self::OwnershipMismatch => formatter.write_str("Windows Job ownership/fence mismatch"),
            Self::PlatformUnavailable(error) => write!(formatter, "platform_unavailable: {error}"),
            Self::Io(error) => write!(formatter, "Windows Job operation failed: {error}"),
            Self::OutcomeUnknown(detail) => write!(formatter, "outcome_unknown: {detail}"),
            Self::CredentialBrokerUnavailable => {
                formatter.write_str("CredentialBrokerUnavailable: Windows credential profile requires an authorized dedicated helper channel")
            }
            Self::Secret(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for JobError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PlatformUnavailable(error) | Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for JobError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Explicit command specification for native `CreateProcessW` launch. The
/// environment is always clear-and-set; ambient daemon variables never leak.
#[derive(Clone, Debug)]
pub struct WindowsCommand {
    program: PathBuf,
    arguments: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
    current_dir: Option<PathBuf>,
    mount_sources: BTreeMap<MountRole, PathBuf>,
    image_digest: Option<String>,
    storage_identity: Option<(String, String, String)>,
    extra_mounts: Vec<LaunchMount>,
}

impl WindowsCommand {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            current_dir: None,
            mount_sources: BTreeMap::new(),
            image_digest: None,
            storage_identity: None,
            extra_mounts: Vec::new(),
        }
    }

    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    pub fn current_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(directory.into());
        self
    }

    pub fn mount_source(mut self, role: MountRole, source: impl Into<PathBuf>) -> Self {
        self.mount_sources.insert(role, source.into());
        self
    }

    pub fn image_digest(mut self, digest: impl Into<String>) -> Self {
        self.image_digest = Some(digest.into());
        self
    }

    pub fn storage_identity(
        mut self,
        instance_id: impl Into<String>,
        rootfs_layer_id: impl Into<String>,
        writable_layer_id: impl Into<String>,
    ) -> Self {
        self.storage_identity = Some((
            instance_id.into(),
            rootfs_layer_id.into(),
            writable_layer_id.into(),
        ));
        self
    }

    pub fn extra_mount(
        mut self,
        role: impl Into<String>,
        source: impl Into<PathBuf>,
        target: impl Into<String>,
        access: impl Into<String>,
    ) -> Self {
        self.extra_mounts.push(LaunchMount {
            role: role.into(),
            source: source.into().to_string_lossy().into_owned(),
            target: target.into(),
            access: access.into(),
        });
        self
    }

    pub(crate) fn from_std(command: &Command) -> Result<Self, JobError> {
        let mut converted = Self::new(command.get_program());
        converted
            .arguments
            .extend(command.get_args().map(OsString::from));
        converted.current_dir = command.get_current_dir().map(Path::to_owned);
        for (key, value) in command.get_envs() {
            if let Some(value) = value {
                converted
                    .environment
                    .insert(key.to_owned(), value.to_owned());
            }
        }
        validate_command(&converted)?;
        Ok(converted)
    }

    fn launch_plan(
        &self,
        profile: &ExecutorProfile,
        terminal: TerminalPlan,
    ) -> Result<LaunchPlan, JobError> {
        validate_command(self)?;
        let text = |value: &OsStr| {
            value
                .to_str()
                .map(str::to_owned)
                .ok_or(JobError::InvalidCommand(
                    "helper protocol requires valid Unicode command data",
                ))
        };
        Ok(LaunchPlan {
            program: text(self.program.as_os_str())?,
            argv: self
                .arguments
                .iter()
                .map(|value| text(value))
                .collect::<Result<_, _>>()?,
            environment: self
                .environment
                .iter()
                .map(|(key, value)| Ok((text(key)?, text(value)?)))
                .collect::<Result<_, JobError>>()?,
            current_directory: self
                .current_dir
                .as_deref()
                .map(|value| text(value.as_os_str()))
                .transpose()?,
            environment_policy: "clear_and_set",
            mounts: profile
                .mounts()
                .iter()
                .map(|mount| {
                    let target = text(mount.target.as_os_str())?;
                    let source = self
                        .mount_sources
                        .get(&mount.role)
                        .map_or_else(|| Ok(target.clone()), |source| text(source.as_os_str()))?;
                    Ok(LaunchMount {
                        role: serde_json::to_string(&mount.role)
                            .expect("profile mount roles serialize")
                            .trim_matches('"')
                            .to_owned(),
                        source,
                        target,
                        access: serde_json::to_string(&mount.access)
                            .expect("profile mount access serializes")
                            .trim_matches('"')
                            .to_owned(),
                    })
                })
                .chain(self.extra_mounts.iter().cloned().map(Ok))
                .collect::<Result<_, JobError>>()?,
            image_digest: self.image_digest.clone(),
            instance_id: self
                .storage_identity
                .as_ref()
                .map(|identity| identity.0.clone()),
            rootfs_layer_id: self
                .storage_identity
                .as_ref()
                .map(|identity| identity.1.clone()),
            writable_layer_id: self
                .storage_identity
                .as_ref()
                .map(|identity| identity.2.clone()),
            terminal,
        })
    }
}

#[derive(Debug)]
pub enum Recovery {
    Reopened(Job),
    OutcomeUnknown { detail: String },
}

#[derive(Debug)]
pub struct Job {
    handle: Option<std::os::windows::io::OwnedHandle>,
    ownership: Ownership,
    identity: BoundaryIdentity,
    limits: JobLimits,
}

pub struct ProductionProcess {
    job: Job,
    runtime: RuntimeBoundary,
    process: JobProcess,
    record: ProcessRecord,
    registration: ProcessRegistryRegistration,
    coordinator: Option<SqliteCancellationCoordinator>,
    owner: Option<AttemptOwnership>,
    request_id: Option<CommandId>,
    more_boundaries: bool,
    capture: Option<CaptureThreads>,
    deadline: Instant,
    terminal: bool,
    custody: Option<PreparedSecrets>,
}

impl ProductionProcess {
    fn confirm_quiescence(&self) -> Result<(), JobError> {
        match (&self.coordinator, self.owner, self.request_id) {
            (Some(coordinator), Some(owner), Some(request_id)) => coordinator
                .confirm_quiescence(owner, request_id, self.more_boundaries)
                .map_err(|error| JobError::OutcomeUnknown(error.to_string())),
            (None, None, None) => Ok(()),
            _ => Err(JobError::OutcomeUnknown(
                "partial Windows cancellation registration".to_owned(),
            )),
        }
    }

    pub const fn record(&self) -> &ProcessRecord {
        &self.record
    }

    pub fn boundary_id(&self) -> &str {
        self.runtime.identity().locator()
    }

    pub fn plan_digest(&self) -> &str {
        self.runtime.plan_digest()
    }

    pub fn helper_identity(&self) -> &str {
        self.runtime.helper_identity()
    }

    pub fn runtime_identity(&self) -> &str {
        self.runtime.runtime_identity()
    }

    pub fn wait(&mut self, deadline: Instant) -> Result<ProcessOutput, JobError> {
        let deadline = deadline.min(self.deadline);
        let code = match self.job.wait(&self.process, deadline) {
            Ok(code) => code,
            Err(JobError::Io(error)) if error.kind() == io::ErrorKind::TimedOut => {
                return self.timeout();
            }
            Err(error) => return self.unknown(error),
        };
        if let Err(error) = self.job.wait_empty(deadline) {
            if error.kind() == io::ErrorKind::TimedOut {
                return self.timeout();
            }
            return self.unknown(JobError::Io(error));
        }
        if let Err(error) = self
            .runtime
            .wait_and_reap(deadline)
            .and_then(|()| inspect_runtime_empty(&mut self.runtime, deadline))
        {
            return self.unknown(JobError::Io(error));
        }
        self.record.exited_on_windows(code);
        let output = match self
            .capture
            .take()
            .ok_or_else(|| JobError::Io(io::Error::other("process output was already collected")))?
            .finish_by(deadline)
        {
            Ok(output) => output,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => return self.timeout(),
            Err(error) => return self.unknown(JobError::Io(error)),
        };
        if let Err(error) = self.confirm_quiescence() {
            return self.unknown(error);
        }
        if let Err(error) = self
            .registration
            .registry
            .exited(self.registration.context, &self.record)
        {
            return self.unknown(JobError::Io(error));
        }
        self.terminal = true;
        settle_custody(&mut self.custody, LifecycleState::Quiescent);
        Ok(output)
    }

    pub fn active_processes(&self) -> io::Result<u32> {
        self.job.active_processes()
    }

    fn timeout(&mut self) -> Result<ProcessOutput, JobError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let job_terminated = self.job.terminate();
        let runtime_terminated = self.runtime.kill_boundary(deadline);
        if let Err(error) = job_terminated.and(runtime_terminated) {
            return self.unknown(JobError::Io(error));
        }
        if let Err(error) = self.job.wait_empty(deadline) {
            return self.unknown(JobError::Io(error));
        }
        if let Err(error) = self
            .runtime
            .wait_and_reap(deadline)
            .and_then(|()| inspect_runtime_empty(&mut self.runtime, deadline))
        {
            return self.unknown(JobError::Io(error));
        }
        let code = match self.process.wait(deadline) {
            Ok(code) => code,
            Err(error) => return self.unknown(JobError::Io(error)),
        };
        self.record.exited_on_windows(code);
        if let Err(error) = self.confirm_quiescence() {
            return self.unknown(error);
        }
        if let Some(capture) = self.capture.take() {
            capture.cancel();
        }
        if let Err(error) = self
            .registration
            .registry
            .exited(self.registration.context, &self.record)
        {
            return self.unknown(JobError::Io(error));
        }
        self.terminal = true;
        settle_custody(&mut self.custody, LifecycleState::Quiescent);
        Err(JobError::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "process exceeded its wall-time limit",
        )))
    }

    fn unknown(&mut self, error: JobError) -> Result<ProcessOutput, JobError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let terminated = self.job.terminate();
        let runtime_terminated = self.runtime.kill_boundary(deadline);
        let emptied = if terminated.is_ok() {
            self.job.wait_empty(deadline)
        } else {
            Err(io::Error::other(
                "Windows Job handle retired after termination failure",
            ))
        };
        if terminated.is_err() || emptied.is_err() {
            self.job.retire();
        }
        let reaped = self.process.wait(deadline);
        let runtime_reaped = self.runtime.wait_and_reap(deadline);
        let runtime_inspected = inspect_runtime_empty(&mut self.runtime, deadline);
        self.process.close_conpty();
        if let Some(capture) = self.capture.take() {
            capture.cancel();
        }
        let _ = self
            .registration
            .registry
            .outcome_unknown(self.registration.context, self.record.process_id());
        self.terminal = true;
        let quiescent = terminated.is_ok()
            && runtime_terminated.is_ok()
            && emptied.is_ok()
            && reaped.is_ok()
            && runtime_reaped.is_ok()
            && runtime_inspected.is_ok();
        settle_custody(
            &mut self.custody,
            if quiescent {
                LifecycleState::Quiescent
            } else {
                LifecycleState::OutcomeUnknown
            },
        );
        terminated.map_err(JobError::Io)?;
        runtime_terminated.map_err(JobError::Io)?;
        emptied.map_err(JobError::Io)?;
        reaped.map_err(JobError::Io)?;
        runtime_reaped.map_err(JobError::Io)?;
        runtime_inspected.map_err(JobError::Io)?;
        Err(error)
    }
}

impl Drop for ProductionProcess {
    fn drop(&mut self) {
        if !self.terminal {
            let deadline = Instant::now() + Duration::from_secs(5);
            let job_terminated = self.job.terminate();
            let job_empty = if job_terminated.is_ok() {
                self.job.wait_empty(deadline)
            } else {
                Err(io::Error::other(
                    "Windows Job handle retired after termination failure",
                ))
            };
            if job_empty.is_err() {
                self.job.retire();
            }
            let runtime_terminated = self.runtime.kill_boundary(deadline);
            let process_reaped = self.process.wait(deadline);
            let runtime_reaped = self.runtime.wait_and_reap(deadline);
            let runtime_empty = inspect_runtime_empty(&mut self.runtime, deadline);
            self.process.close_conpty();
            if let Some(capture) = self.capture.take() {
                capture.cancel();
            }
            let _ = self
                .registration
                .registry
                .outcome_unknown(self.registration.context, self.record.process_id());
            let quiescent = job_terminated.is_ok()
                && job_empty.is_ok()
                && runtime_terminated.is_ok()
                && process_reaped.is_ok()
                && runtime_reaped.is_ok()
                && runtime_empty.is_ok();
            settle_custody(
                &mut self.custody,
                if quiescent {
                    LifecycleState::Quiescent
                } else {
                    LifecycleState::OutcomeUnknown
                },
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_attempt_registered(
    profile: &ExecutorProfile,
    command: &WindowsCommand,
    limits: ResourceLimits,
    owner: AttemptOwnership,
    coordinator: &SqliteCancellationCoordinator,
    workspace: WorkspaceIdentity,
    registration: ProcessRegistryRegistration,
    persistence: impl BoundaryPersistence,
    deadline: Instant,
) -> Result<ProductionProcess, JobError> {
    spawn_attempt_registered_sequence(
        profile,
        command,
        limits,
        owner,
        coordinator,
        workspace,
        registration,
        persistence,
        deadline,
        false,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_attempt_registered_with_secret_broker(
    profile: &ExecutorProfile,
    workspace_acquisition: &AcquisitionResult,
    command: &WindowsCommand,
    limits: ResourceLimits,
    owner: AttemptOwnership,
    authorization: SecretAuthorization,
    broker: &dyn ExecutorSecretBroker,
    coordinator: &SqliteCancellationCoordinator,
    workspace: WorkspaceIdentity,
    registration: ProcessRegistryRegistration,
    persistence: impl BoundaryPersistence,
    deadline: Instant,
) -> Result<ProductionProcess, JobError> {
    spawn_attempt_registered_sequence(
        profile,
        command,
        limits,
        owner,
        coordinator,
        workspace,
        registration,
        persistence,
        deadline,
        false,
        Some((workspace_acquisition, authorization, broker)),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_attempt_registered_sequence(
    profile: &ExecutorProfile,
    command: &WindowsCommand,
    limits: ResourceLimits,
    owner: AttemptOwnership,
    coordinator: &SqliteCancellationCoordinator,
    workspace: WorkspaceIdentity,
    registration: ProcessRegistryRegistration,
    mut persistence: impl BoundaryPersistence,
    deadline: Instant,
    more_boundaries: bool,
    secret_broker: Option<(
        &AcquisitionResult,
        SecretAuthorization,
        &dyn ExecutorSecretBroker,
    )>,
) -> Result<ProductionProcess, JobError> {
    spawn_registered(
        profile,
        command,
        limits,
        ProcessOwnership::Attempt(owner),
        Some((owner, coordinator, workspace, more_boundaries)),
        registration,
        persistence,
        deadline,
        secret_broker,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_daemon_observed(
    profile: &ExecutorProfile,
    command: &WindowsCommand,
    limits: ResourceLimits,
    owner: ProcessOwnership,
    registration: ProcessRegistryRegistration,
    persistence: impl BoundaryPersistence,
    deadline: Instant,
) -> Result<ProductionProcess, JobError> {
    if !matches!(owner, ProcessOwnership::DaemonService(_)) {
        return Err(JobError::OwnershipMismatch);
    }
    spawn_registered(
        profile,
        command,
        limits,
        owner,
        None,
        registration,
        persistence,
        deadline,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_daemon_observed_with_secret_broker(
    profile: &ExecutorProfile,
    workspace: &AcquisitionResult,
    command: &WindowsCommand,
    limits: ResourceLimits,
    owner: ProcessOwnership,
    authorization: SecretAuthorization,
    broker: &dyn ExecutorSecretBroker,
    registration: ProcessRegistryRegistration,
    persistence: impl BoundaryPersistence,
    deadline: Instant,
) -> Result<ProductionProcess, JobError> {
    if !matches!(owner, ProcessOwnership::DaemonService(_)) {
        return Err(JobError::OwnershipMismatch);
    }
    spawn_registered(
        profile,
        command,
        limits,
        owner,
        None,
        registration,
        persistence,
        deadline,
        Some((workspace, authorization, broker)),
    )
}

#[allow(clippy::too_many_arguments)]
fn spawn_registered(
    profile: &ExecutorProfile,
    command: &WindowsCommand,
    limits: ResourceLimits,
    process_owner: ProcessOwnership,
    cancellation: Option<(
        AttemptOwnership,
        &SqliteCancellationCoordinator,
        WorkspaceIdentity,
        bool,
    )>,
    registration: ProcessRegistryRegistration,
    mut persistence: impl BoundaryPersistence,
    deadline: Instant,
    secret_broker: Option<(
        &AcquisitionResult,
        SecretAuthorization,
        &dyn ExecutorSecretBroker,
    )>,
) -> Result<ProductionProcess, JobError> {
    let wants_conpty =
        registration.terminal_transport() == crate::executor::terminal::TerminalTransport::Pty;
    if !profile.credentials().is_empty() && secret_broker.is_none() {
        return Err(JobError::CredentialBrokerUnavailable);
    }
    Job::probe_operations(profile.resources(), wants_conpty)?;
    let output_budget = OutputBudget::new(limits.output_bytes).map_err(JobError::Io)?;
    let wall_deadline = Instant::now()
        .checked_add(Duration::from_millis(limits.wall_time_millis))
        .ok_or(JobError::InvalidLimit(
            "wall time overflows monotonic clock",
        ))?;
    let deadline = deadline.min(wall_deadline);
    if Instant::now() >= deadline {
        return Err(JobError::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "wall-time limit elapsed before Windows launch",
        )));
    }
    let process_id =
        ProcessId::generate().map_err(|error| JobError::Io(io::Error::other(error.to_string())))?;
    let claim = ProcessClaim::new(process_id, process_owner);
    let ownership = Ownership::new(
        serde_json::to_string(&process_owner)
            .map_err(|error| JobError::Io(io::Error::other(error.to_string())))?,
        process_id.to_string(),
    )
    .map_err(|error| JobError::Io(io::Error::other(error.to_string())))?;
    let mut custody = match secret_broker {
        Some((workspace, authorization, broker)) => SecretSpawnPlan::for_windows(
            profile,
            workspace,
            "windows-composite-run",
            super::HELPER_PATH,
        )
        .map_err(JobError::Secret)?
        .map(|plan| {
            let mut helper = Command::new(super::HELPER_PATH);
            resolve_for_spawn(
                plan.authorize(authorization),
                claim,
                &mut helper,
                broker,
                deadline,
            )
            .map_err(JobError::Secret)
        })
        .transpose()?,
        None => None,
    };
    let evidence = match secret_broker {
        Some((_, _, broker)) => {
            super::runtime::probe_for_terminal_with_secret_broker(profile, wants_conpty, broker)
        }
        None => super::runtime::probe_for_terminal(profile, wants_conpty),
    }
    .map_err(runtime_spawn_error)?;
    let request_id = cancellation
        .is_some()
        .then(CommandId::generate)
        .transpose()
        .map_err(|error| JobError::Io(io::Error::other(error.to_string())))?;
    let conpty = if wants_conpty {
        match registration.prepare_conpty(claim, &format!("windows-composite-pending-{process_id}"))
        {
            Ok(binding) => Some(binding),
            Err(error) => {
                return Err(JobError::Io(error));
            }
        }
    } else {
        None
    };
    let pty_output = match conpty
        .as_ref()
        .map(ConPtyBinding::output_reader)
        .transpose()
    {
        Ok(output) => output,
        Err(error) => {
            registration.abort_conpty(process_id);
            return Err(JobError::Io(error));
        }
    };
    if conpty.is_some() {
        if let Err(error) = registration.registry.set_terminal_capture_policy(
            registration.context,
            process_id,
            custody.as_ref().map_or_else(
                CapturePersistencePolicy::no_secrets,
                PreparedSecrets::capture_policy,
            ),
        ) {
            registration.abort_conpty(process_id);
            return Err(JobError::Io(error));
        }
    }
    let pending = match spawn_composite_suspended(
        profile,
        &ownership,
        command,
        limits,
        evidence,
        custody.as_ref(),
        conpty.as_ref(),
        deadline,
    ) {
        Ok(spawned) => spawned,
        Err(error) => {
            settle_custody(
                &mut custody,
                if matches!(error, JobError::OutcomeUnknown(_)) {
                    LifecycleState::OutcomeUnknown
                } else {
                    LifecycleState::Quiescent
                },
            );
            let _ = registration
                .registry
                .outcome_unknown(registration.context, process_id);
            registration.abort_conpty(process_id);
            return Err(error);
        }
    };
    let (mut job, mut runtime, mut process) = match pending.register_and_resume(
        ownership.clone(),
        |composite| {
            persistence.persist(composite)?;
            if let Some((owner, coordinator, workspace, _)) = &cancellation {
                let intent = CancellationIntent::new(
                    request_id.expect("attempt cancellation has a request ID"),
                    *owner,
                    claim,
                    composite.clone(),
                    workspace.clone(),
                    Duration::from_secs(5),
                )
                .map_err(|error| io::Error::other(error.to_string()))?;
                coordinator
                    .register_claim(&intent)
                    .map_err(|error| io::Error::other(error.to_string()))?;
            }
            registration.registry.prepared(
                registration.context,
                claim,
                composite,
                registration.terminal_config(),
            )
        },
        deadline,
    ) {
        Ok(spawned) => spawned,
        Err(error) => {
            settle_custody(
                &mut custody,
                if matches!(error, JobError::OutcomeUnknown(_)) {
                    LifecycleState::OutcomeUnknown
                } else {
                    LifecycleState::Quiescent
                },
            );
            registration.abort_conpty(process_id);
            let _ = registration
                .registry
                .outcome_unknown(registration.context, process_id);
            return Err(error);
        }
    };
    let capture = match pty_output.map_or_else(
        || {
            let stdout = process
                .take_stdout()
                .ok_or_else(|| io::Error::other("Windows launcher did not provide stdout"))?;
            let stderr = process
                .take_stderr()
                .ok_or_else(|| io::Error::other("Windows launcher did not provide stderr"))?;
            CaptureThreads::from_readers(stdout, stderr, output_budget)
        },
        |reader| {
            CaptureThreads::from_pty(
                reader,
                output_budget,
                Arc::clone(&registration.registry),
                registration.context,
                process_id,
                custody.as_ref().map_or_else(
                    || CaptureRedactor::new(&[]).start(CaptureBoundary::TerminalMetadata),
                    |custody| custody.start_capture(CaptureBoundary::TerminalMetadata),
                ),
                registration.terminal_config().retention.max_bytes,
            )
        },
    ) {
        Ok(capture) => capture,
        Err(error) => {
            registration.abort_conpty(process_id);
            let _ = registration
                .registry
                .outcome_unknown(registration.context, process_id);
            return Err(cleanup_post_resume_failure(
                &mut job,
                &mut runtime,
                &mut process,
                None,
                &mut custody,
                JobError::Io(error),
            ));
        }
    };
    let token = match BackendBoundaryToken::from_hex(job.identity().ownership_token()) {
        Ok(token) => token,
        Err(error) => {
            registration.abort_conpty(process_id);
            let _ = registration
                .registry
                .outcome_unknown(registration.context, process_id);
            return Err(cleanup_post_resume_failure(
                &mut job,
                &mut runtime,
                &mut process,
                Some(capture),
                &mut custody,
                JobError::Io(error),
            ));
        }
    };
    let record = ProcessRecord::started(claim, process.id(), token);
    if let Err(error) = registration.registry.started(registration.context, &record) {
        registration.abort_conpty(process_id);
        let _ = registration
            .registry
            .outcome_unknown(registration.context, process_id);
        return Err(cleanup_post_resume_failure(
            &mut job,
            &mut runtime,
            &mut process,
            Some(capture),
            &mut custody,
            JobError::Io(error),
        ));
    }
    Ok(ProductionProcess {
        job,
        runtime,
        process,
        record,
        registration,
        coordinator: cancellation
            .as_ref()
            .map(|(_, coordinator, _, _)| (*coordinator).clone()),
        owner: cancellation.as_ref().map(|(owner, _, _, _)| *owner),
        request_id,
        more_boundaries: cancellation
            .as_ref()
            .is_some_and(|(_, _, _, more_boundaries)| *more_boundaries),
        capture: Some(capture),
        deadline,
        terminal: false,
        custody,
    })
}

#[derive(Debug)]
enum CleanupProof {
    Proven,
    OutcomeUnknown { layer: &'static str, detail: String },
}

impl CleanupProof {
    fn from_result<T>(layer: &'static str, result: io::Result<T>) -> Self {
        match result {
            Ok(_) => Self::Proven,
            Err(error) => Self::OutcomeUnknown {
                layer,
                detail: error.to_string(),
            },
        }
    }

    const fn is_proven(&self) -> bool {
        matches!(self, Self::Proven)
    }

    fn append_failure(&self, failures: &mut Vec<String>) {
        if let Self::OutcomeUnknown { layer, detail } = self {
            failures.push(format!("{layer}: {detail}"));
        }
    }
}

#[derive(Debug)]
struct RuntimeCleanupEvidence {
    helper_abort: CleanupProof,
    runtime_reaped: CleanupProof,
    runtime_absent: CleanupProof,
    recovery_identity: String,
}

#[derive(Debug)]
struct PostResumeCleanupEvidence {
    job_terminated: CleanupProof,
    process_reaped: CleanupProof,
    runtime: RuntimeCleanupEvidence,
    job_empty: CleanupProof,
}

impl PostResumeCleanupEvidence {
    fn lifecycle(&self) -> LifecycleState {
        if self.job_terminated.is_proven()
            && self.process_reaped.is_proven()
            && self.runtime.helper_abort.is_proven()
            && self.runtime.runtime_reaped.is_proven()
            && self.runtime.runtime_absent.is_proven()
            && self.job_empty.is_proven()
        {
            LifecycleState::Quiescent
        } else {
            LifecycleState::OutcomeUnknown
        }
    }

    fn outcome(self, error: JobError) -> JobError {
        if self.lifecycle() == LifecycleState::Quiescent {
            return error;
        }
        let mut failures = Vec::new();
        self.job_terminated.append_failure(&mut failures);
        self.process_reaped.append_failure(&mut failures);
        self.runtime.helper_abort.append_failure(&mut failures);
        self.runtime.runtime_reaped.append_failure(&mut failures);
        self.runtime.runtime_absent.append_failure(&mut failures);
        self.job_empty.append_failure(&mut failures);
        JobError::OutcomeUnknown(format!(
            "{error}; post-resume cleanup was not proven: {}; recovery_identity={}",
            failures.join("; "),
            self.runtime.recovery_identity
        ))
    }
}

fn cleanup_post_resume_failure(
    job: &mut Job,
    runtime: &mut RuntimeBoundary,
    process: &mut JobProcess,
    capture: Option<CaptureThreads>,
    custody: &mut Option<PreparedSecrets>,
    error: JobError,
) -> JobError {
    let deadline = Instant::now() + Duration::from_secs(5);
    process.close_conpty();
    let job_terminated = CleanupProof::from_result("Job terminate", job.terminate());
    let runtime = cleanup_runtime(runtime);
    let process_reaped = CleanupProof::from_result("process reap", process.wait(deadline));
    if let Some(capture) = capture {
        capture.cancel();
    }
    let job_empty = CleanupProof::from_result("Job empty proof", job.wait_empty(deadline));
    let evidence = PostResumeCleanupEvidence {
        job_terminated,
        process_reaped,
        runtime,
        job_empty,
    };
    let lifecycle = evidence.lifecycle();
    if lifecycle != LifecycleState::Quiescent {
        job.retire();
    }
    settle_post_resume_failure(custody, evidence, error)
}

fn settle_post_resume_failure(
    custody: &mut Option<PreparedSecrets>,
    evidence: PostResumeCleanupEvidence,
    error: JobError,
) -> JobError {
    let lifecycle = evidence.lifecycle();
    settle_custody(custody, lifecycle);
    evidence.outcome(error)
}

fn cleanup_runtime(runtime: &mut RuntimeBoundary) -> RuntimeCleanupEvidence {
    let identity = runtime.identity();
    let recovery_identity = format!(
        "{}:{}:{}",
        identity.locator(),
        identity.ownership_token(),
        identity.start_identity()
    );
    match runtime.abort_evidence() {
        Ok(evidence) => RuntimeCleanupEvidence {
            helper_abort: CleanupProof::Proven,
            runtime_reaped: if evidence.direct_child_reaped {
                CleanupProof::Proven
            } else {
                CleanupProof::OutcomeUnknown {
                    layer: "runtime reap proof",
                    detail: "helper did not prove its direct child was reaped".to_owned(),
                }
            },
            runtime_absent: if evidence.boundary_absent && evidence.survivors == 0 {
                CleanupProof::Proven
            } else {
                CleanupProof::OutcomeUnknown {
                    layer: "runtime absence proof",
                    detail: format!(
                        "boundary_absent={}, survivors={}",
                        evidence.boundary_absent, evidence.survivors
                    ),
                }
            },
            recovery_identity,
        },
        Err(error) => RuntimeCleanupEvidence {
            helper_abort: CleanupProof::OutcomeUnknown {
                layer: "helper abort",
                detail: error.to_string(),
            },
            runtime_reaped: CleanupProof::OutcomeUnknown {
                layer: "runtime reap proof",
                detail: "helper abort returned no evidence".to_owned(),
            },
            runtime_absent: CleanupProof::OutcomeUnknown {
                layer: "runtime absence proof",
                detail: "helper abort returned no evidence".to_owned(),
            },
            recovery_identity,
        },
    }
}

fn inspect_runtime_empty(runtime: &mut RuntimeBoundary, deadline: Instant) -> io::Result<()> {
    let inspection = runtime.inspect(deadline)?;
    if inspection.survivors == Some(0) && inspection.quiescent {
        Ok(())
    } else {
        Err(io::Error::other(
            "Windows container/VM boundary did not prove zero survivors",
        ))
    }
}

pub(crate) struct PendingComposite {
    job: Option<Job>,
    process: Option<JobProcess>,
    attestation: Option<SpawnAttestation>,
}

impl PendingComposite {
    pub(crate) fn register_and_resume(
        mut self,
        ownership: Ownership,
        mut register: impl FnMut(&PersistedBoundary) -> io::Result<()>,
        deadline: Instant,
    ) -> Result<(Job, RuntimeBoundary, JobProcess), JobError> {
        let prepared = (|| {
            let composite = PersistedBoundary::windows_composite(
                ownership,
                self.job
                    .as_ref()
                    .expect("pending composite has a Job")
                    .identity()
                    .clone(),
                self.attestation
                    .as_ref()
                    .expect("pending composite has an attestation")
                    .runtime()
                    .identity()
                    .clone(),
            )
            .map_err(|error| io::Error::other(error.to_string()))?;
            register(&composite)?;
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "wall-time limit elapsed before Windows process resume",
                ));
            }
            self.attestation
                .as_mut()
                .expect("pending composite has an attestation")
                .runtime_mut()
                .release(deadline)
        })();
        if let Err(error) = prepared {
            return Err(self.abort(JobError::Io(error)));
        }
        self.process
            .as_mut()
            .expect("pending composite has a process")
            .mark_resumed();
        let runtime = self
            .attestation
            .take()
            .expect("pending composite has an attestation")
            .disarm();
        Ok((
            self.job.take().expect("pending composite has a Job"),
            runtime,
            self.process
                .take()
                .expect("pending composite has a process"),
        ))
    }

    fn abort(&mut self, error: JobError) -> JobError {
        let recovery = self
            .attestation
            .as_ref()
            .map(SpawnAttestation::recovery_identity)
            .unwrap_or_else(|| "missing".to_owned());
        if let Some(process) = self.process.as_mut() {
            process.close_conpty();
        }
        let job_terminated = self.job.as_ref().map(Job::terminate).transpose();
        // Consume the guard before cleanup so Drop cannot retry a partially consumed runtime.
        let helper_aborted = self
            .attestation
            .take()
            .map(|mut attestation| attestation.abort_and_verify())
            .transpose();
        let deadline = Instant::now() + Duration::from_secs(5);
        let process_reaped = self
            .process
            .as_mut()
            .map(|process| process.wait(deadline))
            .transpose();
        let job_empty = self
            .job
            .as_ref()
            .map(|job| job.wait_empty(deadline))
            .transpose();
        pending_cleanup_outcome(
            error,
            &recovery,
            [
                job_terminated,
                helper_aborted,
                process_reaped.map(drop),
                job_empty,
            ],
        )
    }
}

fn pending_cleanup_outcome(
    error: JobError,
    recovery: &str,
    cleanup: [io::Result<()>; 4],
) -> JobError {
    match cleanup.into_iter().find_map(Result::err) {
        Some(cleanup) => JobError::OutcomeUnknown(format!(
            "{error}; post-attestation cleanup was not proven: {cleanup}; recovery_identity={recovery}"
        )),
        None => error,
    }
}

impl Drop for PendingComposite {
    fn drop(&mut self) {
        if self.attestation.is_some() {
            let _ = self.abort(JobError::OutcomeUnknown(
                "pending Windows composite ownership was dropped".to_owned(),
            ));
        }
    }
}

pub(crate) fn spawn_composite_suspended(
    profile: &ExecutorProfile,
    ownership: &Ownership,
    command: &WindowsCommand,
    limits: ResourceLimits,
    evidence: super::runtime::RuntimeEvidence,
    secrets: Option<&PreparedSecrets>,
    conpty: Option<&ConPtyBinding>,
    deadline: Instant,
) -> Result<PendingComposite, JobError> {
    if profile.resources() != limits {
        return Err(JobError::InvalidLimit(
            "launch limits must match the canonical execution profile",
        ));
    }
    let terminal = match conpty {
        Some(binding) => binding
            .with_handle(|handle| TerminalPlan::ConPty {
                // The helper duplicates only this explicit attribute handle from this process.
                source_process_id: unsafe { GetCurrentProcessId() },
                inherited_handles: vec![handle as usize as u64],
            })
            .map_err(JobError::Io)?,
        None => TerminalPlan::Pipes,
    };
    let launch = command.launch_plan(profile, terminal)?;
    let nonce = random_hex()?;
    let attestation = RuntimeBoundary::spawn_suspended(
        profile, ownership, &launch, evidence, secrets, &nonce, deadline,
    )
    .map_err(runtime_spawn_error)?;
    let prepared = (|| {
        if attestation.root_pid == 0
            || (conpty.is_some()
                && (attestation.stdout_handle.is_some() || attestation.stderr_handle.is_some()))
            || (conpty.is_none()
                && (attestation.stdout_handle.is_none() || attestation.stderr_handle.is_none()))
        {
            return Err(JobError::OutcomeUnknown(
                "trusted helper returned an invalid terminal/process handle set".to_owned(),
            ));
        }
        let process_handle = owned_handle(attestation.process_handle)?;
        let thread_handle = owned_handle(attestation.thread_handle)?;
        let job_handle = owned_handle(attestation.job_handle)?;
        let stdout = attestation
            .stdout_handle
            .map(owned_handle)
            .transpose()?
            .map(File::from);
        let stderr = attestation
            .stderr_handle
            .map(owned_handle)
            .transpose()?
            .map(File::from);
        let identity = BoundaryIdentity::new(
            BoundaryKind::WindowsJobObject,
            attestation.job_locator.clone(),
            attestation.job_token.clone(),
            attestation.job_start_identity.clone(),
        )
        .map_err(|error| JobError::OutcomeUnknown(error.to_string()))?;
        let job = Job {
            handle: Some(job_handle),
            ownership: ownership.clone(),
            identity,
            limits: JobLimits::new(limits)?,
        };
        let process = JobProcess {
            process: process_handle,
            thread: thread_handle,
            process_id: attestation.root_pid,
            resumed: false,
            conpty: conpty.cloned(),
            stdout,
            stderr,
        };
        if unsafe { GetProcessId(process.process.as_raw_handle() as HANDLE) } != process.process_id
        {
            return Err(JobError::OutcomeUnknown(
                "duplicated root handle PID does not match helper attestation".to_owned(),
            ));
        }
        if process_creation_time(process.process.as_raw_handle() as HANDLE)?
            != RootEvidence::parse(job.identity.start_identity())?.creation_time_100ns
        {
            return Err(JobError::OutcomeUnknown(
                "duplicated root handle creation time does not match helper attestation".to_owned(),
            ));
        }
        job.verify_limits()
            .map_err(|error| JobError::OutcomeUnknown(error.to_string()))?;
        let mut assigned = 0;
        if unsafe {
            IsProcessInJob(
                process.process.as_raw_handle() as HANDLE,
                job.raw_handle(),
                &mut assigned,
            )
        } == 0
            || assigned == 0
        {
            return Err(JobError::OutcomeUnknown(
                "duplicated suspended root is not assigned to the attested Job".to_owned(),
            ));
        }
        Ok((job, process))
    })();
    match prepared {
        Ok((job, process)) => Ok(PendingComposite {
            job: Some(job),
            process: Some(process),
            attestation: Some(attestation),
        }),
        Err(error) => {
            let mut attestation = attestation;
            match attestation.abort_and_verify() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(JobError::OutcomeUnknown(format!(
                    "{error}; post-attestation cleanup was not proven: {cleanup}; recovery_identity={}",
                    attestation.recovery_identity()
                ))),
            }
        }
    }
}

fn owned_handle(value: u64) -> Result<std::os::windows::io::OwnedHandle, JobError> {
    let value = usize::try_from(value)
        .map_err(|_| JobError::OutcomeUnknown("duplicated handle does not fit usize".to_owned()))?;
    if value == 0 {
        return Err(JobError::OutcomeUnknown(
            "trusted helper returned a null handle".to_owned(),
        ));
    }
    // SAFETY: the trusted helper duplicated this owned handle into the current process.
    Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(value as *mut c_void) })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct JobLimits {
    cpu_user_100ns: i64,
    memory_bytes: usize,
    active_processes: u32,
}

impl Job {
    fn raw_handle(&self) -> HANDLE {
        self.handle
            .as_ref()
            .expect("retired Windows Job is not reused")
            .as_raw_handle() as HANDLE
    }

    pub(crate) fn retire(&mut self) {
        self.handle.take();
    }

    pub(crate) fn probe_operations(
        limits: ResourceLimits,
        require_conpty: bool,
    ) -> Result<(), JobError> {
        let ownership = Ownership::new("kit-job-probe", "assignment-conpty")
            .expect("static probe ownership is valid");
        let binding = require_conpty
            .then(|| {
                ConPtyBinding::probe(
                    crate::executor::terminal::TerminalSize::new(80, 24)
                        .expect("static terminal dimensions are valid"),
                )
            })
            .transpose()?;
        let mut output = binding
            .as_ref()
            .map(ConPtyBinding::output_reader)
            .transpose()?;
        let outer = Self::create(ownership.clone(), limits)?;
        let mut job = Self::create(ownership.clone(), limits)?;
        let job_handles = [outer.raw_handle(), job.raw_handle()];
        let process = job.spawn_with_jobs(
            &ownership,
            &WindowsCommand::new(r"C:\Windows\System32\cmd.exe")
                .arg("/d")
                .arg("/c")
                .arg("exit 0"),
            binding.as_ref(),
            |boundary: &PersistedBoundary| {
                if boundary.identity.start_identity().starts_with("v1:") {
                    Ok(())
                } else {
                    Err(io::Error::other("probe root identity was not bound"))
                }
            },
            &job_handles,
        )?;
        if job.wait(&process, Instant::now() + Duration::from_secs(5))? != 0 {
            return Err(JobError::Io(io::Error::other(
                "Windows Job assignment/ConPTY probe child failed",
            )));
        }
        if let Some(output) = &mut output {
            let mut drained = Vec::new();
            output.read_to_end(&mut drained)?;
        }
        job.wait_empty(Instant::now() + Duration::from_secs(5))?;
        Ok(())
    }

    pub fn create(ownership: Ownership, limits: ResourceLimits) -> Result<Self, JobError> {
        if !limits.finite() {
            return Err(JobError::InvalidLimit(
                "all limits must be finite and non-zero",
            ));
        }
        let token = random_hex()?;
        let name = format!("Local\\kit-job-{token}");
        let name_wide = wide(&name);
        // SAFETY: attributes are null and the name is a live NUL-terminated UTF-16 buffer.
        let raw = unsafe { CreateJobObjectW(ptr::null(), name_wide.as_ptr()) };
        if raw.is_null() {
            return Err(JobError::PlatformUnavailable(io::Error::last_os_error()));
        }
        // SAFETY: GetLastError has no pointer arguments and reads thread-local state.
        let last_error = unsafe { GetLastError() };
        // SAFETY: CreateJobObjectW returned a new owned handle.
        let handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw.cast()) };
        if last_error == ERROR_ALREADY_EXISTS {
            return Err(JobError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "random Windows Job identity already exists",
            )));
        }
        let limits = JobLimits::new(limits)?;
        set_limits(handle.as_raw_handle() as HANDLE, limits)?;
        let identity = BoundaryIdentity::new(
            BoundaryKind::WindowsJobObject,
            name,
            token,
            limits.unbound_identity(),
        )
        .map_err(|error| JobError::Io(io::Error::other(error.to_string())))?;
        Ok(Self {
            handle: Some(handle),
            ownership,
            identity,
            limits,
        })
    }

    pub fn recover(
        persisted: &PersistedBoundary,
        expected_ownership: &Ownership,
    ) -> Result<Recovery, JobError> {
        if persisted.identity.kind() != BoundaryKind::WindowsJobObject
            || &persisted.ownership != expected_ownership
        {
            return Err(JobError::OwnershipMismatch);
        }
        let expected_name = format!("Local\\kit-job-{}", persisted.identity.ownership_token());
        if persisted.identity.locator() != expected_name {
            return Err(JobError::OutcomeUnknown(
                "persisted Windows Job identity is not self-authenticating".to_owned(),
            ));
        }
        let evidence = RootEvidence::parse(persisted.identity.start_identity())?;
        let root = match open_root(evidence.process_id, evidence.creation_time_100ns) {
            Ok(root) => root,
            Err(JobError::OutcomeUnknown(detail)) => {
                return Ok(Recovery::OutcomeUnknown { detail });
            }
            Err(error) => return Err(error),
        };
        let name = wide(&expected_name);
        let access = JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE;
        // SAFETY: the name is NUL terminated and no inheritable handle is requested.
        let raw = unsafe { OpenJobObjectW(access, 0, name.as_ptr()) };
        if raw.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
                return Ok(Recovery::OutcomeUnknown {
                    detail: "named Job disappeared after daemon loss; KILL_ON_JOB_CLOSE was armed, but exit outcome cannot be reconstructed".to_owned(),
                });
            }
            return Err(JobError::OutcomeUnknown(format!(
                "persisted Windows Job could not be reopened: {error}"
            )));
        }
        // SAFETY: OpenJobObjectW returned a new owned handle.
        let handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw.cast()) };
        let job = Self {
            handle: Some(handle),
            ownership: persisted.ownership.clone(),
            identity: persisted.identity.clone(),
            limits: evidence.limits,
        };
        job.verify_limits().map_err(|error| {
            JobError::OutcomeUnknown(format!(
                "reopened Windows Job limit evidence failed: {error}"
            ))
        })?;
        let mut assigned = 0;
        // SAFETY: both handles are live and assigned is writable.
        if unsafe {
            IsProcessInJob(
                root.as_raw_handle() as HANDLE,
                job.raw_handle(),
                &mut assigned,
            )
        } == 0
            || assigned == 0
        {
            return Err(JobError::OutcomeUnknown(
                "persisted root is not a member of the reopened Windows Job".to_owned(),
            ));
        }
        Ok(Recovery::Reopened(job))
    }

    /// Creates the child suspended, assigns it to this Job, durably records the
    /// boundary, and only then resumes the primary thread.
    pub fn spawn(
        &mut self,
        ownership: &Ownership,
        command: &WindowsCommand,
        conpty: Option<&ConPtyBinding>,
        persistence: impl BoundaryPersistence,
    ) -> Result<JobProcess, JobError> {
        let job_handles = [self.raw_handle()];
        self.spawn_with_jobs(ownership, command, conpty, persistence, &job_handles)
    }

    fn spawn_with_jobs(
        &mut self,
        ownership: &Ownership,
        command: &WindowsCommand,
        conpty: Option<&ConPtyBinding>,
        mut persistence: impl BoundaryPersistence,
        job_handles: &[HANDLE],
    ) -> Result<JobProcess, JobError> {
        if ownership != &self.ownership {
            return Err(JobError::OwnershipMismatch);
        }
        if !self.identity.start_identity().starts_with("unbound:") {
            return Err(JobError::InvalidCommand(
                "a Job can launch only one root process",
            ));
        }
        let mut process = create_suspended(command, job_handles, conpty)?;
        let creation_time = process_creation_time(process.process.as_raw_handle() as HANDLE)?;
        self.identity = BoundaryIdentity::new(
            BoundaryKind::WindowsJobObject,
            self.identity.locator(),
            self.identity.ownership_token(),
            RootEvidence {
                process_id: process.process_id,
                creation_time_100ns: creation_time,
                limits: self.limits,
            }
            .encode(),
        )
        .map_err(|error| JobError::Io(io::Error::other(error.to_string())))?;
        let persisted = PersistedBoundary {
            ownership: self.ownership.clone(),
            identity: self.identity.clone(),
        };
        if let Err(error) = persistence.persist(&persisted) {
            let _ = self.terminate();
            process.terminate_and_wait();
            return Err(JobError::Io(io::Error::new(
                error.kind(),
                format!("durable Windows Job persistence failed: {error}"),
            )));
        }
        // SAFETY: this is the live primary thread returned suspended by CreateProcessW.
        if unsafe { ResumeThread(process.thread.as_raw_handle() as HANDLE) } == u32::MAX {
            let error = io::Error::last_os_error();
            let _ = self.terminate();
            process.terminate_and_wait();
            return Err(JobError::Io(error));
        }
        process.resumed = true;
        Ok(process)
    }

    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: the Job handle is live; termination applies atomically to all assigned members.
        if unsafe { TerminateJobObject(self.raw_handle(), 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn wait(&self, process: &JobProcess, deadline: Instant) -> Result<u32, JobError> {
        process.wait(deadline).map_err(JobError::Io)
    }

    pub fn active_processes(&self) -> io::Result<u32> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: the Job is live and accounting is a correctly sized writable value.
        if unsafe {
            QueryInformationJobObject(
                self.raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                mem::size_of_val(&accounting) as u32,
                ptr::null_mut(),
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(accounting.ActiveProcesses)
        }
    }

    fn verify_limits(&self) -> io::Result<()> {
        let actual = query_limits(self.raw_handle())?;
        if actual == self.limits {
            Ok(())
        } else {
            Err(io::Error::other(
                "Windows Job limits do not match persisted evidence",
            ))
        }
    }

    fn wait_empty(&self, deadline: Instant) -> io::Result<()> {
        loop {
            if self.active_processes()? == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Windows Job still has active processes",
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl BoundaryControl for Job {
    fn identity(&self) -> &BoundaryIdentity {
        &self.identity
    }

    fn containment(&self) -> Containment {
        Containment::Complete
    }

    fn release(&mut self, _deadline: Instant) -> io::Result<()> {
        Ok(())
    }

    fn kill_boundary(&mut self, deadline: Instant) -> io::Result<()> {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Windows Job kill deadline elapsed",
            ));
        }
        self.terminate()
    }

    fn wait_and_reap(&mut self, deadline: Instant) -> io::Result<()> {
        self.wait_empty(deadline)
    }

    fn inspect(&mut self, _deadline: Instant) -> io::Result<Inspection> {
        let active = self.active_processes()?;
        Ok(Inspection {
            identity: self.identity.clone(),
            survivors: Some(active),
            quiescent: active == 0,
        })
    }
}

pub struct JobProcess {
    process: std::os::windows::io::OwnedHandle,
    thread: std::os::windows::io::OwnedHandle,
    process_id: u32,
    resumed: bool,
    conpty: Option<ConPtyBinding>,
    stdout: Option<File>,
    stderr: Option<File>,
}

impl JobProcess {
    pub const fn id(&self) -> u32 {
        self.process_id
    }

    pub fn take_stdout(&mut self) -> Option<File> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<File> {
        self.stderr.take()
    }

    pub(crate) fn mark_resumed(&mut self) {
        self.resumed = true;
    }

    pub fn wait(&self, deadline: Instant) -> io::Result<u32> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let millis = remaining.as_millis().clamp(1, 50) as u32;
            // SAFETY: process is a live synchronizable process handle.
            match unsafe { WaitForSingleObject(self.process.as_raw_handle() as HANDLE, millis) } {
                WAIT_OBJECT_0 => {
                    let mut code = 0;
                    // SAFETY: the process is signalled and code is writable.
                    if unsafe {
                        GetExitCodeProcess(self.process.as_raw_handle() as HANDLE, &mut code)
                    } == 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    if let Some(conpty) = &self.conpty {
                        conpty.close();
                    }
                    return Ok(code);
                }
                WAIT_TIMEOUT if Instant::now() < deadline => {}
                WAIT_TIMEOUT => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Windows process wait deadline elapsed",
                    ));
                }
                _ => return Err(io::Error::last_os_error()),
            }
        }
    }

    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: process is live and owned by this value.
        if unsafe { TerminateProcess(self.process.as_raw_handle() as HANDLE, 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(crate) fn close_conpty(&self) {
        if let Some(conpty) = &self.conpty {
            conpty.close();
        }
    }

    fn terminate_and_wait(&mut self) {
        let _ = self.terminate();
        // SAFETY: bounded wait on our live process handle.
        let _ = unsafe { WaitForSingleObject(self.process.as_raw_handle() as HANDLE, 5_000) };
        if let Some(conpty) = &self.conpty {
            conpty.close();
        }
    }
}

impl Drop for JobProcess {
    fn drop(&mut self) {
        if !self.resumed {
            self.terminate_and_wait();
        }
    }
}

struct AttributeList {
    storage: Vec<usize>,
    initialized: bool,
    jobs: Box<[HANDLE]>,
    inherited_handles: Box<[HANDLE]>,
}

impl AttributeList {
    fn new(
        jobs: &[HANDLE],
        pseudo_console: Option<windows_sys::Win32::System::Console::HPCON>,
        inherited_handles: Vec<HANDLE>,
    ) -> io::Result<Self> {
        if jobs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows process requires at least one Job",
            ));
        }
        let attribute_count =
            1 + u32::from(pseudo_console.is_some()) + u32::from(!inherited_handles.is_empty());
        let mut bytes = 0;
        // SAFETY: null first call is the documented size query.
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), attribute_count, 0, &mut bytes)
        };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let words = bytes.div_ceil(mem::size_of::<usize>());
        let mut list = Self {
            storage: vec![0; words],
            initialized: false,
            jobs: jobs.to_vec().into_boxed_slice(),
            inherited_handles: inherited_handles.into_boxed_slice(),
        };
        let pointer = list.pointer();
        // SAFETY: storage is pointer-aligned and has the exact queried byte capacity.
        if unsafe { InitializeProcThreadAttributeList(pointer, attribute_count, 0, &mut bytes) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        list.initialized = true;
        // Creation-time assignment removes the crash window between process creation and Job
        // assignment. The boxed array keeps the attribute value stable through CreateProcessW.
        if unsafe {
            UpdateProcThreadAttribute(
                pointer,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                list.jobs.as_ptr().cast(),
                mem::size_of_val(list.jobs.as_ref()),
                ptr::null_mut(),
                ptr::null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if let Some(pseudo_console) = pseudo_console {
            let (value, size) = pseudoconsole_attribute(pseudo_console);
            // PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE takes the HPCON value itself, not a pointer to
            // storage containing that value. The binding lease keeps it live through launch.
            if unsafe {
                UpdateProcThreadAttribute(
                    pointer,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    value,
                    size,
                    ptr::null_mut(),
                    ptr::null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        if !list.inherited_handles.is_empty() {
            // The list contains only the three standard handles required by the child.
            if unsafe {
                UpdateProcThreadAttribute(
                    pointer,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    list.inherited_handles.as_ptr().cast(),
                    mem::size_of_val(list.inherited_handles.as_ref()),
                    ptr::null_mut(),
                    ptr::null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(list)
    }

    fn pointer(&mut self) -> windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: this list was initialized once and is deleted once before storage drops.
            unsafe { DeleteProcThreadAttributeList(self.pointer()) };
        }
    }
}

fn create_suspended(
    command: &WindowsCommand,
    jobs: &[HANDLE],
    conpty: Option<&ConPtyBinding>,
) -> Result<JobProcess, JobError> {
    validate_command(command)?;
    match conpty {
        Some(binding) => binding
            .with_handle(|handle| create_suspended_with_handle(command, jobs, conpty, Some(handle)))
            .map_err(JobError::Io)?,
        None => create_suspended_with_handle(command, jobs, None, None),
    }
}

fn create_suspended_with_handle(
    command: &WindowsCommand,
    jobs: &[HANDLE],
    conpty: Option<&ConPtyBinding>,
    pseudo_console: Option<windows_sys::Win32::System::Console::HPCON>,
) -> Result<JobProcess, JobError> {
    let application = wide_os(command.program.as_os_str());
    let mut command_line = command_line(command);
    let environment = environment_block(&command.environment)?;
    let current_dir = command.current_dir.as_deref().map(wide_path);
    let pipes = conpty.is_none().then(ChildPipes::new).transpose()?;
    let inherited_handles = pipes
        .as_ref()
        .map_or_else(Vec::new, ChildPipes::child_handles);
    let mut attributes = AttributeList::new(jobs, pseudo_console, inherited_handles)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attributes.pointer();
    if let Some(pipes) = &pipes {
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = pipes.stdin.as_raw_handle() as HANDLE;
        startup.StartupInfo.hStdOutput = pipes.stdout.as_raw_handle() as HANDLE;
        startup.StartupInfo.hStdError = pipes.stderr.as_raw_handle() as HANDLE;
    }
    let mut information = PROCESS_INFORMATION::default();
    let flags = CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
    // SAFETY: every pointer references a live NUL-terminated/multi-string buffer for the call;
    // STARTUPINFOEX and its optional attribute list remain live through CreateProcessW.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            i32::from(pipes.is_some()),
            flags,
            environment.as_ptr().cast(),
            current_dir
                .as_ref()
                .map_or(ptr::null(), |value| value.as_ptr()),
            &startup.StartupInfo as *const _,
            &mut information,
        )
    };
    if created == 0 {
        return Err(JobError::Io(io::Error::last_os_error()));
    }
    let (stdout, stderr) = pipes.map_or((None, None), ChildPipes::into_readers);
    // SAFETY: CreateProcessW returned two distinct owned handles.
    let process = unsafe {
        JobProcess {
            process: std::os::windows::io::OwnedHandle::from_raw_handle(
                information.hProcess.cast(),
            ),
            thread: std::os::windows::io::OwnedHandle::from_raw_handle(information.hThread.cast()),
            process_id: information.dwProcessId,
            resumed: false,
            conpty: conpty.cloned(),
            stdout,
            stderr,
        }
    };
    Ok(process)
}

fn pseudoconsole_attribute(
    handle: windows_sys::Win32::System::Console::HPCON,
) -> (*mut c_void, usize) {
    (handle as *mut c_void, mem::size_of_val(&handle))
}

struct ChildPipes {
    stdin: std::os::windows::io::OwnedHandle,
    stdout: std::os::windows::io::OwnedHandle,
    stderr: std::os::windows::io::OwnedHandle,
    stdout_reader: File,
    stderr_reader: File,
}

impl ChildPipes {
    fn new() -> io::Result<Self> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };
        let (stdin, stdin_writer) = inheritable_pipe(&attributes)?;
        clear_inherit(stdin_writer.as_raw_handle() as HANDLE)?;
        drop(stdin_writer);
        let (stdout_reader, stdout) = inheritable_pipe(&attributes)?;
        clear_inherit(stdout_reader.as_raw_handle() as HANDLE)?;
        let (stderr_reader, stderr) = inheritable_pipe(&attributes)?;
        clear_inherit(stderr_reader.as_raw_handle() as HANDLE)?;
        Ok(Self {
            stdin,
            stdout,
            stderr,
            stdout_reader: File::from(stdout_reader),
            stderr_reader: File::from(stderr_reader),
        })
    }

    fn child_handles(&self) -> Vec<HANDLE> {
        vec![
            self.stdin.as_raw_handle() as HANDLE,
            self.stdout.as_raw_handle() as HANDLE,
            self.stderr.as_raw_handle() as HANDLE,
        ]
    }

    fn into_readers(self) -> (Option<File>, Option<File>) {
        (Some(self.stdout_reader), Some(self.stderr_reader))
    }
}

fn inheritable_pipe(
    attributes: &SECURITY_ATTRIBUTES,
) -> io::Result<(
    std::os::windows::io::OwnedHandle,
    std::os::windows::io::OwnedHandle,
)> {
    let mut read = ptr::null_mut();
    let mut write = ptr::null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, attributes, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe returned two distinct owned handles.
    Ok(unsafe {
        (
            std::os::windows::io::OwnedHandle::from_raw_handle(read.cast()),
            std::os::windows::io::OwnedHandle::from_raw_handle(write.cast()),
        )
    })
}

fn clear_inherit(handle: HANDLE) -> io::Result<()> {
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_limits(handle: HANDLE, limits: JobLimits) -> Result<(), JobError> {
    let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_JOB_TIME;
    information.BasicLimitInformation.ActiveProcessLimit = limits.active_processes;
    information.BasicLimitInformation.PerJobUserTimeLimit = limits.cpu_user_100ns;
    information.JobMemoryLimit = limits.memory_bytes;
    // SAFETY: handle is a live Job and information is a correctly sized immutable value.
    if unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            (&information as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            mem::size_of_val(&information) as u32,
        )
    } == 0
    {
        Err(JobError::PlatformUnavailable(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

impl JobLimits {
    fn new(limits: ResourceLimits) -> Result<Self, JobError> {
        Ok(Self {
            memory_bytes: usize::try_from(limits.memory_bytes)
                .map_err(|_| JobError::InvalidLimit("memory does not fit this architecture"))?,
            cpu_user_100ns: limits
                .cpu_millis
                .checked_mul(10_000)
                .and_then(|value| i64::try_from(value).ok())
                .ok_or(JobError::InvalidLimit(
                    "CPU user time overflows Windows units",
                ))?,
            active_processes: limits.pids,
        })
    }

    fn unbound_identity(self) -> String {
        format!(
            "unbound:{}:{}:{}",
            self.cpu_user_100ns, self.memory_bytes, self.active_processes
        )
    }
}

#[derive(Clone, Copy)]
struct RootEvidence {
    process_id: u32,
    creation_time_100ns: u64,
    limits: JobLimits,
}

impl RootEvidence {
    fn encode(self) -> String {
        format!(
            "v1:{}:{}:{}:{}:{}",
            self.process_id,
            self.creation_time_100ns,
            self.limits.cpu_user_100ns,
            self.limits.memory_bytes,
            self.limits.active_processes
        )
    }

    fn parse(value: &str) -> Result<Self, JobError> {
        let fields = value.split(':').collect::<Vec<_>>();
        if fields.len() != 6 || fields[0] != "v1" {
            return Err(JobError::OutcomeUnknown(
                "persisted Windows Job has no bound root evidence".to_owned(),
            ));
        }
        let invalid =
            || JobError::OutcomeUnknown("invalid persisted Windows Job evidence".to_owned());
        Ok(Self {
            process_id: fields[1].parse().map_err(|_| invalid())?,
            creation_time_100ns: fields[2].parse().map_err(|_| invalid())?,
            limits: JobLimits {
                cpu_user_100ns: fields[3].parse().map_err(|_| invalid())?,
                memory_bytes: fields[4].parse().map_err(|_| invalid())?,
                active_processes: fields[5].parse().map_err(|_| invalid())?,
            },
        })
    }
}

fn open_root(
    process_id: u32,
    expected_creation_time: u64,
) -> Result<std::os::windows::io::OwnedHandle, JobError> {
    // Validate the PID before opening the named Job. If the root is gone, descendants cannot
    // authenticate a same-name object and recovery must not touch it.
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            process_id,
        )
    };
    if raw.is_null() {
        return Err(JobError::OutcomeUnknown(
            "persisted Windows Job root is gone; descendant identity cannot be proven".to_owned(),
        ));
    }
    // SAFETY: OpenProcess returned a new owned handle.
    let handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw.cast()) };
    if process_creation_time(raw)? != expected_creation_time {
        return Err(JobError::OutcomeUnknown(
            "persisted Windows Job root PID was reused".to_owned(),
        ));
    }
    Ok(handle)
}

fn process_creation_time(process: HANDLE) -> Result<u64, JobError> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: process is live and all FILETIME outputs are writable.
    if unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(JobError::Io(io::Error::last_os_error()));
    }
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn query_limits(handle: HANDLE) -> io::Result<JobLimits> {
    let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: handle is live and information is a correctly sized writable value.
    if unsafe {
        QueryInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            (&mut information as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            mem::size_of_val(&information) as u32,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let required = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_JOB_TIME;
    let forbidden = JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
    if information.BasicLimitInformation.LimitFlags & required != required
        || information.BasicLimitInformation.LimitFlags & forbidden != 0
    {
        return Err(io::Error::other("Windows Job required limit flags changed"));
    }
    Ok(JobLimits {
        cpu_user_100ns: information.BasicLimitInformation.PerJobUserTimeLimit,
        memory_bytes: information.JobMemoryLimit,
        active_processes: information.BasicLimitInformation.ActiveProcessLimit,
    })
}

fn validate_command(command: &WindowsCommand) -> Result<(), JobError> {
    if !command.program.is_absolute() {
        return Err(JobError::InvalidCommand("program must be absolute"));
    }
    if command
        .program
        .as_os_str()
        .encode_wide()
        .chain(
            command
                .arguments
                .iter()
                .flat_map(|value| value.encode_wide()),
        )
        .any(|unit| unit == 0)
    {
        return Err(JobError::InvalidCommand(
            "program and arguments cannot contain NUL",
        ));
    }
    if command.current_dir.as_ref().is_some_and(|directory| {
        !directory.is_absolute() || directory.as_os_str().encode_wide().any(|unit| unit == 0)
    }) {
        return Err(JobError::InvalidCommand(
            "current directory must be absolute and cannot contain NUL",
        ));
    }
    let mut normalized_keys = BTreeSet::new();
    for (key, value) in &command.environment {
        let key = key.to_string_lossy();
        if key.is_empty() || key.contains(['=', '\0']) || value.encode_wide().any(|unit| unit == 0)
        {
            return Err(JobError::InvalidCommand("invalid environment entry"));
        }
        if !normalized_keys.insert(key.to_uppercase()) {
            return Err(JobError::InvalidCommand(
                "environment keys must be unique case-insensitively",
            ));
        }
    }
    Ok(())
}

fn command_line(command: &WindowsCommand) -> Vec<u16> {
    let mut encoded = quote(command.program.as_os_str());
    for argument in &command.arguments {
        encoded.push(' ' as u16);
        encoded.extend(quote(argument));
    }
    encoded.push(0);
    encoded
}

fn quote(value: &OsStr) -> Vec<u16> {
    let units = value.encode_wide().collect::<Vec<_>>();
    if !units.is_empty()
        && units
            .iter()
            .all(|unit| !matches!(*unit, 0x09 | 0x20 | 0x22))
    {
        return units;
    }
    let mut result = vec![0x22];
    let mut slashes = 0;
    for unit in units {
        if unit == 0x5c {
            slashes += 1;
        } else {
            if unit == 0x22 {
                result.extend(std::iter::repeat_n(0x5c, slashes + 1));
            }
            result.extend(std::iter::repeat_n(0x5c, slashes));
            slashes = 0;
            result.push(unit);
        }
    }
    result.extend(std::iter::repeat_n(0x5c, slashes * 2));
    result.push(0x22);
    result
}

fn environment_block(environment: &BTreeMap<OsString, OsString>) -> Result<Vec<u16>, JobError> {
    let mut entries = environment
        .iter()
        .map(|(key, value)| {
            let mut entry = key.encode_wide().collect::<Vec<_>>();
            entry.push('=' as u16);
            entry.extend(value.encode_wide());
            entry
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| String::from_utf16_lossy(entry).to_uppercase());
    let mut block = Vec::new();
    for entry in entries {
        block.extend(entry);
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    Ok(block)
}

fn random_hex() -> Result<String, JobError> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|error| {
        JobError::Io(io::Error::other(format!(
            "Job identity entropy failed: {error}"
        )))
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn wide_os(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn wide_path(value: &Path) -> Vec<u16> {
    wide_os(value.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    static CUSTODY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[derive(Clone, Copy)]
    enum PostResumeFault {
        HelperAbortUnknown,
        JobTerminate,
        ProcessReap,
        NonzeroSurvivors,
    }

    fn proven_cleanup() -> PostResumeCleanupEvidence {
        PostResumeCleanupEvidence {
            job_terminated: CleanupProof::Proven,
            process_reaped: CleanupProof::Proven,
            runtime: RuntimeCleanupEvidence {
                helper_abort: CleanupProof::Proven,
                runtime_reaped: CleanupProof::Proven,
                runtime_absent: CleanupProof::Proven,
                recovery_identity: "container-1:owner-1:v4:durable".to_owned(),
            },
            job_empty: CleanupProof::Proven,
        }
    }

    fn faulted_cleanup(fault: PostResumeFault) -> PostResumeCleanupEvidence {
        let mut evidence = proven_cleanup();
        match fault {
            PostResumeFault::HelperAbortUnknown => {
                evidence.runtime.helper_abort = CleanupProof::OutcomeUnknown {
                    layer: "helper abort",
                    detail: "injected unknown outcome".to_owned(),
                };
                evidence.runtime.runtime_reaped = CleanupProof::OutcomeUnknown {
                    layer: "runtime reap proof",
                    detail: "helper abort returned no evidence".to_owned(),
                };
                evidence.runtime.runtime_absent = CleanupProof::OutcomeUnknown {
                    layer: "runtime absence proof",
                    detail: "helper abort returned no evidence".to_owned(),
                };
            }
            PostResumeFault::JobTerminate => {
                evidence.job_terminated = CleanupProof::OutcomeUnknown {
                    layer: "Job terminate",
                    detail: "injected failure".to_owned(),
                };
            }
            PostResumeFault::ProcessReap => {
                evidence.process_reaped = CleanupProof::OutcomeUnknown {
                    layer: "process reap",
                    detail: "injected failure".to_owned(),
                };
            }
            PostResumeFault::NonzeroSurvivors => {
                evidence.runtime.runtime_absent = CleanupProof::OutcomeUnknown {
                    layer: "runtime absence proof",
                    detail: "boundary_absent=false, survivors=1".to_owned(),
                };
            }
        }
        evidence
    }

    #[test]
    fn pseudoconsole_attribute_passes_the_hpcon_value() {
        let handle = 0x1234_isize;
        let (value, size) = pseudoconsole_attribute(handle);
        assert_eq!(value as isize, handle);
        assert_eq!(size, mem::size_of_val(&handle));
        assert_ne!(value, (&handle as *const isize).cast_mut().cast());
    }

    #[test]
    fn successful_helper_abort_with_failed_job_cleanup_retains_exact_recovery_identity() {
        let recovery = "container-1:owner-1:v4:immutable-recovery-identity";
        let error = pending_cleanup_outcome(
            JobError::Io(io::Error::other("registration failed")),
            recovery,
            [
                Err(io::Error::other("Job terminate failed")),
                Ok(()),
                Err(io::Error::other("Job process reap failed")),
                Err(io::Error::other("Job empty proof failed")),
            ],
        );

        let JobError::OutcomeUnknown(detail) = error else {
            panic!("partial cleanup must be quarantined as outcome unknown");
        };
        assert_eq!(
            detail,
            "Windows Job operation failed: registration failed; post-attestation cleanup was not proven: Job terminate failed; recovery_identity=container-1:owner-1:v4:immutable-recovery-identity"
        );
    }

    #[test]
    fn helper_abort_unknown_stays_a_job_outcome_unknown() {
        let error = runtime_spawn_error(super::super::runtime::RuntimeUnavailable {
            reason: super::super::runtime::UnavailableReason::OutcomeUnknown,
            detail: "helper abort unknown; recovery_identity=container-1:owner-1:v4:durable"
                .to_owned(),
        });

        assert!(matches!(&error, JobError::OutcomeUnknown(_)));
        assert!(!error.to_string().contains("platform_unavailable"));
    }

    #[test]
    fn every_post_resume_fault_quarantines_custody() {
        let _guard = CUSTODY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut expected = crate::executor::process::own::quarantined_secret_custodies();
        for step in ["capture", "token", "registry-started"] {
            for fault in [
                PostResumeFault::HelperAbortUnknown,
                PostResumeFault::JobTerminate,
                PostResumeFault::ProcessReap,
                PostResumeFault::NonzeroSurvivors,
            ] {
                let mut custody = Some(PreparedSecrets::for_windows_channel_test(b"quarantine"));
                let error = settle_post_resume_failure(
                    &mut custody,
                    faulted_cleanup(fault),
                    JobError::Io(io::Error::other(format!("injected {step} failure"))),
                );

                assert!(custody.is_none());
                assert!(matches!(&error, JobError::OutcomeUnknown(_)));
                assert!(
                    error
                        .to_string()
                        .contains("recovery_identity=container-1:owner-1:v4:durable")
                );
                expected += 1;
                assert_eq!(
                    crate::executor::process::own::quarantined_secret_custodies(),
                    expected
                );
            }
        }
    }

    #[test]
    fn fully_proven_post_resume_cleanup_releases_custody_once() {
        let _guard = CUSTODY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = crate::executor::process::own::quarantined_secret_custodies();
        let mut custody = Some(PreparedSecrets::for_windows_channel_test(b"release"));

        let error = settle_post_resume_failure(
            &mut custody,
            proven_cleanup(),
            JobError::Io(io::Error::other("injected capture failure")),
        );
        assert!(custody.is_none());
        assert!(matches!(error, JobError::Io(_)));
        assert_eq!(
            crate::executor::process::own::quarantined_secret_custodies(),
            before
        );

        let _ = settle_post_resume_failure(
            &mut custody,
            proven_cleanup(),
            JobError::Io(io::Error::other("repeated settlement")),
        );
        assert_eq!(
            crate::executor::process::own::quarantined_secret_custodies(),
            before
        );
    }
}

use std::os::windows::io::FromRawHandle;
