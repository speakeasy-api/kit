#[cfg(unix)]
mod unix;

use serde::Serialize;
use std::{
    fmt,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use crate::{
    domain::{
        config::Grant,
        ids::{PrincipalId, ProcessId},
        lifecycle::{FencingToken, ProcessClaim, ProcessOwnership},
        secret::{SecretHandle, SecretLease},
    },
    executor::profile::{
        CredentialInjectionMode, ExecutionLabel, ExecutorProfile, Platform,
        RepositoryExecutionPolicy,
    },
    telemetry::redact::{
        CaptureBoundary, CapturePersistencePolicy, CaptureRedactor, SanitizedCapture,
        SanitizerProvenance,
    },
    workspace::acquire::{AcquisitionResult, GitMetadata},
};

use crate::executor::process::tree::Ownership;

const FIRST_SECRET_DESCRIPTOR: i32 = 100;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InjectionChannel {
    FileDescriptor(i32),
    MemoryFile { descriptor: i32, path: PathBuf },
    ScopedEnvironment { variable: String, descriptor: i32 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InjectionBinding {
    handle: SecretHandle,
    channel: InjectionChannel,
}

impl InjectionBinding {
    pub fn handle(&self) -> &SecretHandle {
        &self.handle
    }

    pub fn channel(&self) -> &InjectionChannel {
        &self.channel
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretBrokerError {
    Denied,
    Unavailable,
}

impl fmt::Display for SecretBrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Denied => "secret broker denied the executor context",
            Self::Unavailable => "secret broker is unavailable",
        })
    }
}

impl std::error::Error for SecretBrokerError {}

/// Executor-only broker contract. Unlike `SecretResolver`, every resolution is
/// authorized against the exact process and destination about to be spawned.
pub trait ExecutorSecretBroker {
    fn authorize_and_resolve(
        &self,
        handle: &SecretHandle,
        context: &ExecutorSecretContext<'_>,
    ) -> Result<SecretLease, SecretBrokerError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecretAuthorization {
    owner: ProcessOwnership,
    grant: Grant,
    expires_at: Instant,
}

impl SecretAuthorization {
    pub const fn new(owner: ProcessOwnership, grant: Grant, expires_at: Instant) -> Self {
        Self {
            owner,
            grant,
            expires_at,
        }
    }

    pub const fn process_spawn(owner: ProcessOwnership, expires_at: Instant) -> Self {
        Self::new(owner, Grant::ProcessSpawn, expires_at)
    }
}

pub struct ExecutorSecretContext<'a> {
    claim: ProcessClaim,
    principal: Option<PrincipalId>,
    acquisition_id: &'a str,
    workspace_identity: &'a str,
    profile_digest: &'a str,
    invocation_intent: &'a str,
    program: &'a Path,
    destination: &'a InjectionChannel,
    grant: Grant,
    fence: Option<FencingToken>,
    expires_at: Instant,
    deadline: Instant,
}

impl ExecutorSecretContext<'_> {
    pub const fn claim(&self) -> ProcessClaim {
        self.claim
    }

    pub const fn process_id(&self) -> ProcessId {
        self.claim.process_id
    }

    pub const fn owner(&self) -> ProcessOwnership {
        self.claim.owner
    }

    pub const fn principal(&self) -> Option<PrincipalId> {
        self.principal
    }

    pub const fn acquisition_id(&self) -> &str {
        self.acquisition_id
    }

    pub const fn workspace_identity(&self) -> &str {
        self.workspace_identity
    }

    pub const fn profile_digest(&self) -> &str {
        self.profile_digest
    }

    pub const fn invocation_intent(&self) -> &str {
        self.invocation_intent
    }

    pub const fn program(&self) -> &Path {
        self.program
    }

    pub const fn destination(&self) -> &InjectionChannel {
        self.destination
    }

    pub const fn grant(&self) -> Grant {
        self.grant
    }

    pub const fn fence(&self) -> Option<FencingToken> {
        self.fence
    }

    pub const fn expires_at(&self) -> Instant {
        self.expires_at
    }

    pub const fn deadline(&self) -> Instant {
        self.deadline
    }
}

impl fmt::Debug for ExecutorSecretContext<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutorSecretContext")
            .field("claim", &self.claim)
            .field("acquisition_id", &self.acquisition_id)
            .field("workspace_identity", &self.workspace_identity)
            .field("profile_digest", &self.profile_digest)
            .field("invocation_intent", &self.invocation_intent)
            .field("program", &self.program)
            .field("destination", &self.destination)
            .field("grant", &self.grant)
            .field("fence", &self.fence)
            .field("expires_at", &self.expires_at)
            .field("deadline", &self.deadline)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreparationError {
    UnsupportedPlatform,
    UnsupportedProfile,
    RepositoryCodeEnabled,
    SharedWorkspaceMetadata,
    InvalidWorkspace,
    InvalidHandle,
    AuthorizationDenied,
    AuthorizationExpired,
    BrokerDenied,
    BrokerUnavailable,
    EntropyUnavailable,
    EmptySecret,
    InvalidEnvironmentSecret,
    SecretTooLarge,
    OperatingSystem(String),
}

impl fmt::Display for PreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedPlatform => "secret injection is unsupported on this platform",
            Self::UnsupportedProfile => "secret injection requires a complete restricted boundary",
            Self::RepositoryCodeEnabled => {
                "secret injection requires disabled repository hooks and submodules"
            }
            Self::SharedWorkspaceMetadata => {
                "secret injection requires independent workspace metadata"
            }
            Self::InvalidWorkspace => "secret injection requires a live acquired workspace",
            Self::InvalidHandle => "executor credential is not a valid opaque secret handle",
            Self::AuthorizationDenied => "secret authorization does not match the owned process",
            Self::AuthorizationExpired => "secret authorization expired before spawn",
            Self::BrokerDenied => "secret broker denied the executor context",
            Self::BrokerUnavailable => "secret broker is unavailable",
            Self::EntropyUnavailable => "sanitizer provenance entropy is unavailable",
            Self::EmptySecret => "secret broker returned an empty value",
            Self::InvalidEnvironmentSecret => {
                "scoped environment secret contains an unsupported NUL byte"
            }
            Self::SecretTooLarge => "secret exceeds the 64 KiB helper-channel bound",
            Self::OperatingSystem(_) => "operating-system secret injection failed",
        })
    }
}

impl std::error::Error for PreparationError {}

#[derive(Clone)]
struct CredentialPlan {
    handle: SecretHandle,
    channel: InjectionChannel,
}

/// Unresolved metadata attached to a single-use owned spawn token.
pub(crate) struct SecretSpawnPlan {
    credentials: Vec<CredentialPlan>,
    acquisition_id: String,
    workspace_identity: String,
    profile_digest: String,
    invocation_intent: String,
    program: PathBuf,
    authorization: Option<SecretAuthorization>,
}

impl fmt::Debug for SecretSpawnPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretSpawnPlan")
            .field("credentials", &self.credentials.len())
            .field("acquisition_id", &self.acquisition_id)
            .field("workspace_identity", &self.workspace_identity)
            .field("profile_digest", &self.profile_digest)
            .field("invocation_intent", &self.invocation_intent)
            .field("program", &self.program)
            .field("authorized", &self.authorization.is_some())
            .finish()
    }
}

impl SecretSpawnPlan {
    pub(crate) fn for_container(
        profile: &ExecutorProfile,
        workspace: &AcquisitionResult,
        invocation_intent: impl Into<String>,
        program: impl Into<PathBuf>,
    ) -> Result<Option<Self>, PreparationError> {
        Self::for_platform(
            profile,
            workspace,
            invocation_intent,
            program,
            Platform::Linux,
            cfg!(target_os = "linux"),
        )
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn for_windows(
        profile: &ExecutorProfile,
        workspace: &AcquisitionResult,
        invocation_intent: impl Into<String>,
        program: impl Into<PathBuf>,
    ) -> Result<Option<Self>, PreparationError> {
        Self::for_platform(
            profile,
            workspace,
            invocation_intent,
            program,
            Platform::Windows,
            true,
        )
    }

    fn for_platform(
        profile: &ExecutorProfile,
        workspace: &AcquisitionResult,
        invocation_intent: impl Into<String>,
        program: impl Into<PathBuf>,
        platform: Platform,
        host_supported: bool,
    ) -> Result<Option<Self>, PreparationError> {
        if profile.credentials().is_empty() {
            return Ok(None);
        }
        validate_contract(profile, workspace, platform, host_supported)?;
        Ok(Some(Self {
            credentials: credential_plans(profile)?,
            acquisition_id: workspace.acquisition_id.as_str().to_owned(),
            workspace_identity: workspace.workspace_revision.hash.as_str().to_owned(),
            profile_digest: profile.digest().to_string(),
            invocation_intent: invocation_intent.into(),
            program: program.into(),
            authorization: None,
        }))
    }

    pub(crate) fn authorize(mut self, authorization: SecretAuthorization) -> Self {
        self.authorization = Some(authorization);
        self
    }

    pub(crate) fn helper_arguments(&self) -> Vec<String> {
        self.credentials
            .iter()
            .map(|credential| match &credential.channel {
                InjectionChannel::FileDescriptor(descriptor) => {
                    format!("--secret-binding=fd:{descriptor}")
                }
                InjectionChannel::MemoryFile { descriptor, path } => {
                    format!("--secret-binding=memfd:{descriptor}:{}", path.display())
                }
                InjectionChannel::ScopedEnvironment {
                    variable,
                    descriptor,
                } => {
                    format!("--secret-binding=env:{variable}:{descriptor}")
                }
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        modes: impl IntoIterator<Item = CredentialInjectionMode>,
        owner: ProcessOwnership,
        expires_at: Instant,
        program: impl Into<PathBuf>,
    ) -> Self {
        let credentials = modes
            .into_iter()
            .enumerate()
            .map(|(index, mode)| CredentialPlan {
                handle: SecretHandle::parse(&format!("test-{index}")).unwrap(),
                channel: channel(index, &mode).unwrap(),
            })
            .collect();
        Self {
            credentials,
            acquisition_id: "test-acquisition".to_owned(),
            workspace_identity: "test-workspace".to_owned(),
            profile_digest: "test-profile".to_owned(),
            invocation_intent: "test-spawn".to_owned(),
            program: program.into(),
            authorization: Some(SecretAuthorization::process_spawn(owner, expires_at)),
        }
    }
}

fn credential_plans(profile: &ExecutorProfile) -> Result<Vec<CredentialPlan>, PreparationError> {
    profile
        .credentials()
        .iter()
        .enumerate()
        .map(|(index, credential)| {
            Ok(CredentialPlan {
                handle: SecretHandle::parse(credential.handle.as_str())
                    .map_err(|_| PreparationError::InvalidHandle)?,
                channel: channel(index, &credential.mode)?,
            })
        })
        .collect()
}

fn channel(
    index: usize,
    mode: &CredentialInjectionMode,
) -> Result<InjectionChannel, PreparationError> {
    let descriptor = FIRST_SECRET_DESCRIPTOR
        .checked_add(i32::try_from(index).map_err(|_| PreparationError::InvalidHandle)?)
        .ok_or(PreparationError::InvalidHandle)?;
    Ok(match mode {
        CredentialInjectionMode::FileDescriptor => InjectionChannel::FileDescriptor(descriptor),
        CredentialInjectionMode::MemoryFile => InjectionChannel::MemoryFile {
            descriptor,
            path: Path::new("/proc/self/fd").join(descriptor.to_string()),
        },
        CredentialInjectionMode::ScopedEnvironment { variable } => {
            InjectionChannel::ScopedEnvironment {
                variable: variable.clone(),
                descriptor,
            }
        }
    })
}

fn validate_contract(
    profile: &ExecutorProfile,
    workspace: &AcquisitionResult,
    platform: Platform,
    host_supported: bool,
) -> Result<(), PreparationError> {
    if profile.repository() != RepositoryExecutionPolicy::DISABLED {
        return Err(PreparationError::RepositoryCodeEnabled);
    }
    if workspace.git_metadata != GitMetadata::Independent {
        return Err(PreparationError::SharedWorkspaceMetadata);
    }
    if !workspace.path.is_dir() {
        return Err(PreparationError::InvalidWorkspace);
    }
    if !matches!(
        (platform, profile.label()),
        (Platform::Linux, ExecutionLabel::Restricted)
            | (
                Platform::Windows,
                ExecutionLabel::Restricted | ExecutionLabel::Hostile
            )
    ) {
        return Err(PreparationError::UnsupportedProfile);
    }
    if profile.platform() != platform || !host_supported {
        return Err(PreparationError::UnsupportedPlatform);
    }
    Ok(())
}

/// Resolved material is private to the owned-spawn boundary and remains alive
/// until the boundary has been proved quiescent.
pub(crate) struct PreparedSecrets {
    leases: Vec<SecretLease>,
    bindings: Vec<InjectionBinding>,
    files: Vec<std::fs::File>,
    descriptor_mappings: Vec<(i32, i32)>,
    provenance: SanitizerProvenance,
}

impl fmt::Debug for PreparedSecrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedSecrets")
            .field("bindings", &self.bindings)
            .field("values", &"[REDACTED]")
            .finish()
    }
}

impl Drop for PreparedSecrets {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        {
            use std::io::{Seek, SeekFrom, Write};

            let zeros = [0_u8; 4096];
            for file in &mut self.files {
                let Ok(mut remaining) = file.metadata().map(|metadata| metadata.len()) else {
                    continue;
                };
                if file.seek(SeekFrom::Start(0)).is_err() {
                    continue;
                }
                while remaining != 0 {
                    let length = remaining.min(zeros.len() as u64) as usize;
                    if file.write_all(&zeros[..length]).is_err() {
                        break;
                    }
                    remaining -= length as u64;
                }
                let _ = file.sync_data();
            }
        }
    }
}

impl PreparedSecrets {
    pub(crate) fn bindings(&self) -> &[InjectionBinding] {
        &self.bindings
    }

    fn apply(&self, command: &mut Command) -> Result<(), PreparationError> {
        command.env_clear();

        #[cfg(unix)]
        {
            unix::configure_allowlist(command, self.descriptor_mappings.clone());
            Ok(())
        }

        #[cfg(windows)]
        return Ok(());

        #[cfg(not(any(unix, windows)))]
        Err(PreparationError::UnsupportedPlatform)
    }

    pub(crate) fn windows_channel_descriptor(
        &self,
        profile_digest: &str,
        ownership: &Ownership,
        nonce: &str,
    ) -> WindowsSecretChannelDescriptor {
        WindowsSecretChannelDescriptor {
            protocol: "kit-windows-secret-channel-v1",
            profile_digest: profile_digest.to_owned(),
            daemon_service: ownership.daemon_service().to_owned(),
            attempt: ownership.attempt().to_owned(),
            nonce: nonce.to_owned(),
            bindings: self
                .bindings
                .iter()
                .map(|binding| WindowsSecretBinding {
                    handle: binding.handle.identifier().to_owned(),
                    destination: injection_destination(&binding.channel),
                })
                .collect(),
        }
    }

    pub(crate) fn windows_channel(
        &self,
        descriptor: &WindowsSecretChannelDescriptor,
        plan_digest: &str,
    ) -> Result<WindowsSecretChannel, PreparationError> {
        let descriptor = serde_json::to_vec(descriptor)
            .map_err(|error| PreparationError::OperatingSystem(error.to_string()))?;
        let mut bytes = Vec::new();
        append_channel_field(&mut bytes, b"kit-windows-secret-channel-v1")?;
        append_channel_field(&mut bytes, plan_digest.as_bytes())?;
        append_channel_field(&mut bytes, blake3::hash(&descriptor).as_bytes())?;
        append_channel_field(&mut bytes, self.leases.len().to_string().as_bytes())?;
        for (binding, lease) in self.bindings.iter().zip(&self.leases) {
            if lease.expose().len() > 64 * 1024 {
                return Err(PreparationError::SecretTooLarge);
            }
            append_channel_field(&mut bytes, binding.handle.identifier().as_bytes())?;
            append_channel_field(
                &mut bytes,
                injection_destination(&binding.channel).as_bytes(),
            )?;
            append_channel_field(&mut bytes, lease.expose())?;
        }
        if bytes.len() > 1024 * 1024 {
            return Err(PreparationError::SecretTooLarge);
        }
        Ok(WindowsSecretChannel { bytes })
    }

    pub(crate) fn sanitize(&self, boundary: CaptureBoundary, value: &[u8]) -> SanitizedCapture {
        self.redactor().sanitize(boundary, value)
    }

    pub(crate) fn start_capture(&self, boundary: CaptureBoundary) -> SanitizedCapture {
        self.redactor().start(boundary)
    }

    pub(crate) fn start_capture_with_custody(
        &self,
        boundary: CaptureBoundary,
        custody: &crate::domain::secret::SecretCustody,
    ) -> SanitizedCapture {
        let (revision, shared) = custody.leases_with_revision();
        CaptureRedactor::combined_process_bound(&self.leases, &shared, self.provenance)
            .start(boundary)
            .with_custody_and_fixed_patterns(
                custody.clone(),
                revision,
                shared.len(),
                CaptureRedactor::process_bound(&self.leases, self.provenance)
                    .patterns()
                    .clone(),
            )
    }

    pub(crate) const fn capture_policy(&self) -> CapturePersistencePolicy {
        CapturePersistencePolicy::process_bound(self.provenance)
    }

    fn redactor(&self) -> CaptureRedactor<'_> {
        CaptureRedactor::process_bound(&self.leases, self.provenance)
    }

    #[cfg(test)]
    pub(crate) fn for_windows_channel_test(value: &[u8]) -> Self {
        let owner = ProcessOwnership::DaemonService(
            crate::domain::ids::DaemonServiceId::generate().unwrap(),
        );
        let claim = ProcessClaim::new(crate::domain::ids::ProcessId::generate().unwrap(), owner);
        Self {
            leases: vec![SecretLease::new(value)],
            bindings: vec![InjectionBinding {
                handle: SecretHandle::parse("provider_token").unwrap(),
                channel: InjectionChannel::FileDescriptor(FIRST_SECRET_DESCRIPTOR),
            }],
            files: Vec::new(),
            descriptor_mappings: Vec::new(),
            provenance: SanitizerProvenance::issue(claim, "test-profile", "windows-helper")
                .unwrap(),
        }
    }
}

pub(crate) fn resolve_for_spawn(
    plan: SecretSpawnPlan,
    claim: ProcessClaim,
    command: &mut Command,
    broker: &dyn ExecutorSecretBroker,
    deadline: Instant,
) -> Result<PreparedSecrets, PreparationError> {
    let authorization = plan
        .authorization
        .ok_or(PreparationError::AuthorizationDenied)?;
    let now = Instant::now();
    if authorization.owner != claim.owner || authorization.grant != Grant::ProcessSpawn {
        return Err(PreparationError::AuthorizationDenied);
    }
    if now >= authorization.expires_at || now >= deadline {
        return Err(PreparationError::AuthorizationExpired);
    }

    let (principal, fence) = match claim.owner {
        ProcessOwnership::Attempt(owner) => (Some(owner.principal_id), Some(owner.fencing_token)),
        ProcessOwnership::DaemonService(_) => (None, None),
    };
    let provenance =
        SanitizerProvenance::issue(claim, &plan.profile_digest, &plan.invocation_intent)
            .map_err(|_| PreparationError::EntropyUnavailable)?;
    let mut prepared = PreparedSecrets {
        leases: Vec::with_capacity(plan.credentials.len()),
        bindings: Vec::with_capacity(plan.credentials.len()),
        files: Vec::new(),
        descriptor_mappings: Vec::new(),
        provenance,
    };

    for credential in plan.credentials {
        if Instant::now() >= authorization.expires_at || Instant::now() >= deadline {
            return Err(PreparationError::AuthorizationExpired);
        }
        let context = ExecutorSecretContext {
            claim,
            principal,
            acquisition_id: &plan.acquisition_id,
            workspace_identity: &plan.workspace_identity,
            profile_digest: &plan.profile_digest,
            invocation_intent: &plan.invocation_intent,
            program: &plan.program,
            destination: &credential.channel,
            grant: authorization.grant,
            fence,
            expires_at: authorization.expires_at,
            deadline,
        };
        let lease = broker
            .authorize_and_resolve(&credential.handle, &context)
            .map_err(|error| match error {
                SecretBrokerError::Denied => PreparationError::BrokerDenied,
                SecretBrokerError::Unavailable => PreparationError::BrokerUnavailable,
            })?;
        if lease.expose().is_empty() {
            return Err(PreparationError::EmptySecret);
        }
        if matches!(
            &credential.channel,
            InjectionChannel::ScopedEnvironment { .. }
        ) && lease.expose().contains(&0)
        {
            return Err(PreparationError::InvalidEnvironmentSecret);
        }

        #[cfg(unix)]
        match &credential.channel {
            InjectionChannel::FileDescriptor(target)
            | InjectionChannel::MemoryFile {
                descriptor: target, ..
            }
            | InjectionChannel::ScopedEnvironment {
                descriptor: target, ..
            } => {
                let file = descriptor_file(lease.expose())?;
                prepared
                    .descriptor_mappings
                    .push((descriptor(&file), *target));
                prepared.files.push(file);
            }
        }
        prepared.leases.push(lease);
        prepared.bindings.push(InjectionBinding {
            handle: credential.handle,
            channel: credential.channel,
        });
    }
    prepared.apply(command)?;
    Ok(prepared)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WindowsSecretChannelDescriptor {
    protocol: &'static str,
    profile_digest: String,
    daemon_service: String,
    attempt: String,
    nonce: String,
    bindings: Vec<WindowsSecretBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct WindowsSecretBinding {
    handle: String,
    destination: String,
}

pub(crate) struct WindowsSecretChannel {
    bytes: Vec<u8>,
}

impl WindowsSecretChannel {
    #[cfg_attr(not(any(windows, test)), allow(dead_code))]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for WindowsSecretChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WindowsSecretChannel")
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

impl Drop for WindowsSecretChannel {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

fn injection_destination(channel: &InjectionChannel) -> String {
    match channel {
        InjectionChannel::FileDescriptor(descriptor) => format!("fd:{descriptor}"),
        InjectionChannel::MemoryFile { descriptor, path } => {
            format!("memfd:{descriptor}:{}", path.display())
        }
        InjectionChannel::ScopedEnvironment {
            variable,
            descriptor,
        } => format!("env:{variable}:{descriptor}"),
    }
}

fn append_channel_field(output: &mut Vec<u8>, value: &[u8]) -> Result<(), PreparationError> {
    let length = u32::try_from(value.len()).map_err(|_| PreparationError::SecretTooLarge)?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(value);
    Ok(())
}

#[cfg(unix)]
fn descriptor_file(value: &[u8]) -> Result<std::fs::File, PreparationError> {
    unix::descriptor_file(value)
        .map_err(|error| PreparationError::OperatingSystem(error.to_string()))
}

#[cfg(unix)]
fn descriptor(file: &std::fs::File) -> i32 {
    use std::os::unix::io::AsRawFd;
    file.as_raw_fd()
}

#[cfg(not(unix))]
fn descriptor_file(_value: &[u8]) -> Result<std::fs::File, PreparationError> {
    Err(PreparationError::UnsupportedPlatform)
}

#[cfg(not(unix))]
fn descriptor(_file: &std::fs::File) -> i32 {
    -1
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::domain::{
        config::Grant,
        ids::{DaemonServiceId, ProcessId},
        lifecycle::{ProcessClaim, ProcessOwnership},
    };
    use std::time::Duration;

    const CANARY: &[u8] = b"scoped-target-canary";

    struct Broker;

    impl ExecutorSecretBroker for Broker {
        fn authorize_and_resolve(
            &self,
            _handle: &SecretHandle,
            _context: &ExecutorSecretContext<'_>,
        ) -> Result<SecretLease, SecretBrokerError> {
            Ok(SecretLease::new(CANARY))
        }
    }

    #[test]
    fn scoped_secret_uses_descriptor_transport_and_enters_only_target_environment() {
        let owner = ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap());
        let expires_at = Instant::now() + Duration::from_secs(2);
        let plan = SecretSpawnPlan {
            credentials: vec![CredentialPlan {
                handle: SecretHandle::parse("scoped-secret").unwrap(),
                channel: InjectionChannel::ScopedEnvironment {
                    variable: "KIT_SCOPED_SECRET".to_owned(),
                    descriptor: FIRST_SECRET_DESCRIPTOR,
                },
            }],
            acquisition_id: "test-acquisition".to_owned(),
            workspace_identity: "test-workspace".to_owned(),
            profile_digest: "test-profile".to_owned(),
            invocation_intent: "helper-test".to_owned(),
            program: "/bin/sh".into(),
            authorization: Some(SecretAuthorization::new(
                owner,
                Grant::ProcessSpawn,
                expires_at,
            )),
        };
        let arguments = plan.helper_arguments();
        assert_eq!(arguments, ["--secret-binding=env:KIT_SCOPED_SECRET:100"]);
        assert!(!format!("{plan:?}").contains(std::str::from_utf8(CANARY).unwrap()));

        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "test -z \"${KIT_SCOPED_SECRET+x}\" || exit 90; test \"$1\" = --secret-binding=env:KIT_SCOPED_SECRET:100 || exit 91; KIT_SCOPED_SECRET=\"$(cat <&100)\" exec /bin/sh -c 'printf %s \"$KIT_SCOPED_SECRET\"'",
            "helper",
            arguments[0].as_str(),
        ]);
        command.env("KIT_SCOPED_SECRET", std::str::from_utf8(CANARY).unwrap());
        let prepared = resolve_for_spawn(
            plan,
            ProcessClaim::new(ProcessId::generate().unwrap(), owner),
            &mut command,
            &Broker,
            expires_at,
        )
        .unwrap();

        let canary = std::str::from_utf8(CANARY).unwrap();
        assert!(!format!("{command:?}").contains(canary));
        assert!(!format!("{prepared:?}").contains(canary));
        assert!(
            command
                .get_envs()
                .all(|(_, value)| value.is_none_or(|value| !value
                    .as_encoded_bytes()
                    .windows(CANARY.len())
                    .any(|v| v == CANARY)))
        );
        let output = command.output().unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(output.stdout, CANARY);
        let sanitized = prepared.sanitize(CaptureBoundary::Log, &output.stdout);
        assert!(!String::from_utf8_lossy(sanitized.bytes().unwrap()).contains(canary));
    }

    #[test]
    fn process_capture_refreshes_custody_registered_during_live_output() {
        let owner = ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap());
        let claim = ProcessClaim::new(ProcessId::generate().unwrap(), owner);
        let prepared = PreparedSecrets {
            leases: vec![SecretLease::new("fixed-process-secret")],
            bindings: Vec::new(),
            files: Vec::new(),
            descriptor_mappings: Vec::new(),
            provenance: SanitizerProvenance::issue(claim, "test-profile", "live-pty").unwrap(),
        };
        let custody = crate::domain::secret::SecretCustody::default();
        let mut capture =
            prepared.start_capture_with_custody(CaptureBoundary::TerminalMetadata, &custody);
        capture.push(b"late-").unwrap();

        custody.register(
            "live-process",
            "rotated",
            std::sync::Arc::new(SecretLease::new("late-secret")),
        );
        assert!(capture.take_ready().is_none());
        capture.push(b"secret fixed-process-secret").unwrap();
        capture.finish().unwrap();

        let output = capture.bytes().unwrap();
        assert!(
            !output
                .windows(b"late-secret".len())
                .any(|value| value == b"late-secret")
        );
        assert!(
            !output
                .windows(b"fixed-process-secret".len())
                .any(|value| value == b"fixed-process-secret")
        );
        assert!(
            output
                .windows(b"[REDACTED]".len())
                .any(|value| value == b"[REDACTED]")
        );
    }
}
