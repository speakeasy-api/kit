use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const EXECUTOR_PROFILE_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    TrustedLocal,
    Restricted,
    Hostile,
}

impl TrustTier {
    pub const ALL: [Self; 3] = [Self::TrustedLocal, Self::Restricted, Self::Hostile];

    pub const fn label(self) -> &'static str {
        match self {
            Self::TrustedLocal => "trusted_local",
            Self::Restricted => "restricted",
            Self::Hostile => "hostile",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLabel {
    TrustedLocal,
    Restricted,
    Hostile,
    HostCompatibility,
}

impl ExecutionLabel {
    pub const ALL: [Self; 4] = [
        Self::TrustedLocal,
        Self::Restricted,
        Self::Hostile,
        Self::HostCompatibility,
    ];

    pub const fn trust_tier(self) -> Option<TrustTier> {
        match self {
            Self::TrustedLocal => Some(TrustTier::TrustedLocal),
            Self::Restricted => Some(TrustTier::Restricted),
            Self::Hostile => Some(TrustTier::Hostile),
            Self::HostCompatibility => None,
        }
    }

    pub const fn is_isolation(self) -> bool {
        self.trust_tier().is_some()
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::TrustedLocal => "trusted_local",
            Self::Restricted => "restricted",
            Self::Hostile => "hostile",
            Self::HostCompatibility => "host_compatibility",
        }
    }
}

impl From<TrustTier> for ExecutionLabel {
    fn from(value: TrustTier) -> Self {
        match value {
            TrustTier::TrustedLocal => Self::TrustedLocal,
            TrustTier::Restricted => Self::Restricted,
            TrustTier::Hostile => Self::Hostile,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityOptIn {
    weaker_than: TrustTier,
    reason: String,
}

impl CompatibilityOptIn {
    pub fn trusted_local(reason: impl Into<String>) -> Result<Self, ProfileError> {
        let reason = reason.into();
        if reason.trim().is_empty() {
            return Err(ProfileError::EmptyCompatibilityReason);
        }
        Ok(Self {
            weaker_than: TrustTier::TrustedLocal,
            reason,
        })
    }

    pub const fn weaker_than(&self) -> TrustTier {
        self.weaker_than
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Linux,
    MacOs,
    Windows,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    X86_64,
    Aarch64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MountRole {
    Root,
    Source,
    Build,
    Temp,
}

impl MountRole {
    pub const ALL: [Self; 4] = [Self::Root, Self::Source, Self::Build, Self::Temp];
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MountAccess {
    ReadOnly,
    ReadWrite,
    CopyOnWrite,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub role: MountRole,
    pub target: PathBuf,
    pub access: MountAccess,
}

impl Mount {
    pub fn new(role: MountRole, target: impl Into<PathBuf>, access: MountAccess) -> Self {
        Self {
            role,
            target: target.into(),
            access,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceWriteMode {
    ReadOnly,
    MutationOverlay,
    Direct,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CredentialHandle(String);

impl CredentialHandle {
    pub fn new(value: impl Into<String>) -> Result<Self, ProfileError> {
        let value = value.into();
        if !valid_credential_handle(&value) {
            return Err(ProfileError::InvalidCredentialHandle);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CredentialInjectionMode {
    FileDescriptor,
    MemoryFile,
    ScopedEnvironment { variable: String },
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialInjection {
    pub handle: CredentialHandle,
    pub mode: CredentialInjectionMode,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressTransport {
    Tcp,
    Udp,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EgressGrant {
    destination: String,
    port: u16,
    transport: EgressTransport,
}

impl EgressGrant {
    pub fn new(
        destination: impl Into<String>,
        port: u16,
        transport: EgressTransport,
    ) -> Result<Self, ProfileError> {
        let destination = destination.into().to_ascii_lowercase();
        if !valid_destination(&destination) {
            return Err(ProfileError::UnsafeEgressDestination(destination));
        }
        if port == 0 {
            return Err(ProfileError::ZeroEgressPort);
        }
        Ok(Self {
            destination,
            port,
            transport,
        })
    }

    pub fn destination(&self) -> &str {
        &self.destination
    }

    pub const fn port(&self) -> u16 {
        self.port
    }

    pub const fn transport(&self) -> EgressTransport {
        self.transport
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryCodePolicy {
    Disabled,
    Sandboxed,
    Unrestricted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryExecutionPolicy {
    pub hooks: RepositoryCodePolicy,
    pub submodules: RepositoryCodePolicy,
}

impl RepositoryExecutionPolicy {
    pub const DISABLED: Self = Self {
        hooks: RepositoryCodePolicy::Disabled,
        submodules: RepositoryCodePolicy::Disabled,
    };
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    pub pids: u32,
    pub file_bytes: u64,
    pub disk_bytes: u64,
    pub io_bytes: u64,
    pub output_bytes: u64,
    pub wall_time_millis: u64,
}

impl ResourceLimits {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        cpu_millis: u64,
        memory_bytes: u64,
        pids: u32,
        file_bytes: u64,
        disk_bytes: u64,
        io_bytes: u64,
        output_bytes: u64,
        wall_time_millis: u64,
    ) -> Self {
        Self {
            cpu_millis,
            memory_bytes,
            pids,
            file_bytes,
            disk_bytes,
            io_bytes,
            output_bytes,
            wall_time_millis,
        }
    }

    pub const fn finite(self) -> bool {
        self.cpu_millis > 0
            && self.memory_bytes > 0
            && self.pids > 0
            && self.file_bytes > 0
            && self.disk_bytes > 0
            && self.io_bytes > 0
            && self.output_bytes > 0
            && self.wall_time_millis > 0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendPrimitive {
    OsSandbox,
    FilesystemBoundary,
    ProcessBoundary,
    PrivilegeBoundary,
    UserNamespace,
    RootlessBoundary,
    WindowsJobObject,
    ContainerOrVmBoundary,
    UserKernelOrVmTenantBoundary,
    VmTenantBoundary,
    SyscallPolicy,
    TenantBoundary,
    IsolatedStorage,
    ScrubbedEnvironment,
    ProcessGroup,
    WholeProcessTreeControl,
    ReadOnlyMount,
    WritableMount,
    CopyOnWriteMount,
    ReadOnlySource,
    SourceMutationOverlay,
    CredentialFileDescriptor,
    CredentialMemoryFile,
    CredentialScopedEnvironment,
    NetworkDeny,
    DestinationEgress,
    /// A proxy/enforcer revalidates every DNS answer and connection against granted public addresses.
    RebindingSafeEgress,
    CpuLimit,
    MemoryLimit,
    PidLimit,
    FileSizeLimit,
    DiskLimit,
    IoLimit,
    OutputLimit,
    WallTimeLimit,
    RepositoryCodeDisabled,
    RepositoryCodeSandbox,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSpec {
    pub schema_version: u16,
    pub label: ExecutionLabel,
    #[serde(default)]
    pub compatibility: Option<CompatibilityOptIn>,
    pub mounts: Vec<Mount>,
    pub source_write: SourceWriteMode,
    #[serde(default)]
    pub credentials: Vec<CredentialInjection>,
    #[serde(default)]
    pub egress: BTreeSet<EgressGrant>,
    pub resources: ResourceLimits,
    pub repository: RepositoryExecutionPolicy,
    pub platform: Platform,
    pub architecture: Architecture,
    #[serde(default)]
    pub additional_requirements: BTreeSet<BackendPrimitive>,
}

impl ProfileSpec {
    pub fn isolated(
        tier: TrustTier,
        platform: Platform,
        architecture: Architecture,
        resources: ResourceLimits,
    ) -> Self {
        Self::base(tier.into(), platform, architecture, resources)
    }

    pub fn host_compatibility(
        platform: Platform,
        architecture: Architecture,
        resources: ResourceLimits,
        opt_in: CompatibilityOptIn,
    ) -> Self {
        let mut spec = Self::base(
            ExecutionLabel::HostCompatibility,
            platform,
            architecture,
            resources,
        );
        spec.compatibility = Some(opt_in);
        spec
    }

    fn base(
        label: ExecutionLabel,
        platform: Platform,
        architecture: Architecture,
        resources: ResourceLimits,
    ) -> Self {
        let (root, source, build, temp) = match platform {
            Platform::Windows => (r"C:\", r"C:\workspace", r"C:\build", r"C:\temp"),
            Platform::Linux | Platform::MacOs => ("/", "/workspace", "/build", "/tmp"),
        };
        let compatibility = label == ExecutionLabel::HostCompatibility;
        let repository = match label {
            ExecutionLabel::TrustedLocal => RepositoryExecutionPolicy {
                hooks: RepositoryCodePolicy::Sandboxed,
                submodules: RepositoryCodePolicy::Sandboxed,
            },
            ExecutionLabel::HostCompatibility => RepositoryExecutionPolicy {
                hooks: RepositoryCodePolicy::Unrestricted,
                submodules: RepositoryCodePolicy::Unrestricted,
            },
            ExecutionLabel::Restricted | ExecutionLabel::Hostile => {
                RepositoryExecutionPolicy::DISABLED
            }
        };
        Self {
            schema_version: EXECUTOR_PROFILE_SCHEMA_VERSION,
            label,
            compatibility: None,
            mounts: vec![
                Mount::new(
                    MountRole::Root,
                    root,
                    if compatibility {
                        MountAccess::ReadWrite
                    } else {
                        MountAccess::ReadOnly
                    },
                ),
                Mount::new(
                    MountRole::Source,
                    source,
                    if compatibility {
                        MountAccess::ReadWrite
                    } else {
                        MountAccess::ReadOnly
                    },
                ),
                Mount::new(MountRole::Build, build, MountAccess::ReadWrite),
                Mount::new(MountRole::Temp, temp, MountAccess::ReadWrite),
            ],
            source_write: if compatibility {
                SourceWriteMode::Direct
            } else {
                SourceWriteMode::ReadOnly
            },
            credentials: Vec::new(),
            egress: BTreeSet::new(),
            resources,
            repository,
            platform,
            architecture,
            additional_requirements: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProfileDigest([u8; 32]);

impl ProfileDigest {
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }

    pub fn hex(self) -> String {
        let mut value = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            write!(value, "{byte:02x}").expect("writing to a string cannot fail");
        }
        value
    }
}

impl fmt::Display for ProfileDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blake3:{}", self.hex())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutorProfile {
    spec: ProfileSpec,
    requirements: BTreeSet<BackendPrimitive>,
    canonical_bytes: Vec<u8>,
    digest: ProfileDigest,
}

impl ExecutorProfile {
    pub fn new(mut spec: ProfileSpec) -> Result<Self, ProfileError> {
        for mount in &mut spec.mounts {
            if spec.platform == Platform::Windows {
                mount.target = PathBuf::from(mount.target.to_string_lossy().to_ascii_lowercase());
            }
        }
        spec.egress = spec
            .egress
            .into_iter()
            .map(|mut grant| {
                grant.destination.make_ascii_lowercase();
                grant
            })
            .collect();
        validate(&spec)?;
        spec.mounts.sort();
        spec.credentials.sort();
        let requirements = requirements(&spec);
        let canonical_bytes = serde_json::to_vec(&CanonicalProfile {
            spec: &spec,
            requirements: &requirements,
            effective: EffectivePolicy::new(&spec, &requirements),
        })
        .map_err(|error| ProfileError::Canonicalization(error.to_string()))?;
        let digest = ProfileDigest(*blake3::hash(&canonical_bytes).as_bytes());
        Ok(Self {
            spec,
            requirements,
            canonical_bytes,
            digest,
        })
    }

    pub const fn schema_version(&self) -> u16 {
        self.spec.schema_version
    }

    pub const fn label(&self) -> ExecutionLabel {
        self.spec.label
    }

    pub fn compatibility(&self) -> Option<&CompatibilityOptIn> {
        self.spec.compatibility.as_ref()
    }

    pub fn mounts(&self) -> &[Mount] {
        &self.spec.mounts
    }

    pub const fn source_write(&self) -> SourceWriteMode {
        self.spec.source_write
    }

    pub fn credentials(&self) -> &[CredentialInjection] {
        &self.spec.credentials
    }

    pub const fn egress(&self) -> &BTreeSet<EgressGrant> {
        &self.spec.egress
    }

    pub const fn resources(&self) -> ResourceLimits {
        self.spec.resources
    }

    pub fn with_resources(&self, resources: ResourceLimits) -> Result<Self, ProfileError> {
        let mut spec = self.spec.clone();
        spec.resources = resources;
        Self::new(spec)
    }

    pub const fn repository(&self) -> RepositoryExecutionPolicy {
        self.spec.repository
    }

    pub const fn platform(&self) -> Platform {
        self.spec.platform
    }

    pub const fn architecture(&self) -> Architecture {
        self.spec.architecture
    }

    pub const fn requirements(&self) -> &BTreeSet<BackendPrimitive> {
        &self.requirements
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    pub const fn digest(&self) -> ProfileDigest {
        self.digest
    }
}

#[derive(Serialize)]
struct CanonicalProfile<'a> {
    spec: &'a ProfileSpec,
    requirements: &'a BTreeSet<BackendPrimitive>,
    effective: EffectivePolicy,
}

#[derive(Serialize)]
struct EffectivePolicy {
    filesystem: &'static str,
    network: &'static str,
    source_write: SourceWriteMode,
    resources: BTreeSet<BackendPrimitive>,
    repository: RepositoryExecutionPolicy,
}

impl EffectivePolicy {
    fn new(spec: &ProfileSpec, requirements: &BTreeSet<BackendPrimitive>) -> Self {
        use BackendPrimitive as P;

        let resources = requirements
            .iter()
            .filter(|primitive| {
                matches!(
                    primitive,
                    P::CpuLimit
                        | P::MemoryLimit
                        | P::PidLimit
                        | P::FileSizeLimit
                        | P::DiskLimit
                        | P::IoLimit
                        | P::OutputLimit
                        | P::WallTimeLimit
                )
            })
            .copied()
            .collect();
        Self {
            filesystem: if spec.label == ExecutionLabel::HostCompatibility {
                "host_read_write"
            } else {
                "sandboxed_mounts"
            },
            network: if spec.label == ExecutionLabel::HostCompatibility {
                "host"
            } else if spec.egress.is_empty() {
                "deny"
            } else {
                "granted_egress"
            },
            source_write: spec.source_write,
            resources,
            repository: spec.repository,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct BackendId(String);

impl BackendId {
    pub fn new(value: impl Into<String>) -> Result<Self, ProfileError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ProfileError::InvalidBackendId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeStatus {
    Available,
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendProbe {
    pub backend: BackendId,
    pub label: ExecutionLabel,
    pub platform: Platform,
    pub architecture: Architecture,
    pub capabilities: BTreeSet<BackendPrimitive>,
    pub status: ProbeStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeRejection {
    Unavailable(String),
    WrongLabel(ExecutionLabel),
    WrongPlatform(Platform),
    WrongArchitecture(Architecture),
    MissingCapabilities(BTreeSet<BackendPrimitive>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectedBackend {
    pub backend: BackendId,
    pub reason: ProbeRejection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendSelectionError {
    RequiredBackendUnavailable {
        label: ExecutionLabel,
        platform: Platform,
        architecture: Architecture,
        required: BTreeSet<BackendPrimitive>,
        rejected: Vec<RejectedBackend>,
    },
}

impl fmt::Display for BackendSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequiredBackendUnavailable {
                label,
                platform,
                architecture,
                ..
            } => write!(
                f,
                "required {} backend is unavailable for {platform:?}/{architecture:?}",
                label.label()
            ),
        }
    }
}

impl std::error::Error for BackendSelectionError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendSelection {
    backend: BackendId,
    label: ExecutionLabel,
    compatibility: Option<CompatibilityOptIn>,
    profile_digest: ProfileDigest,
}

impl BackendSelection {
    pub const fn backend(&self) -> &BackendId {
        &self.backend
    }

    pub const fn label(&self) -> ExecutionLabel {
        self.label
    }

    pub fn compatibility(&self) -> Option<&CompatibilityOptIn> {
        self.compatibility.as_ref()
    }

    pub const fn profile_digest(&self) -> ProfileDigest {
        self.profile_digest
    }
}

pub fn select_backend(
    profile: &ExecutorProfile,
    probes: impl IntoIterator<Item = BackendProbe>,
) -> Result<BackendSelection, BackendSelectionError> {
    let mut probes = probes.into_iter().collect::<Vec<_>>();
    probes.sort_by(|left, right| left.backend.cmp(&right.backend));
    let mut rejected = Vec::with_capacity(probes.len());

    for probe in probes {
        let reason = match &probe.status {
            ProbeStatus::Unavailable { reason } => {
                Some(ProbeRejection::Unavailable(reason.clone()))
            }
            ProbeStatus::Available if probe.label != profile.label() => {
                Some(ProbeRejection::WrongLabel(probe.label))
            }
            ProbeStatus::Available if probe.platform != profile.platform() => {
                Some(ProbeRejection::WrongPlatform(probe.platform))
            }
            ProbeStatus::Available if probe.architecture != profile.architecture() => {
                Some(ProbeRejection::WrongArchitecture(probe.architecture))
            }
            ProbeStatus::Available => {
                let missing = profile
                    .requirements()
                    .difference(&probe.capabilities)
                    .copied()
                    .collect::<BTreeSet<_>>();
                (!missing.is_empty()).then_some(ProbeRejection::MissingCapabilities(missing))
            }
        };
        if let Some(reason) = reason {
            rejected.push(RejectedBackend {
                backend: probe.backend,
                reason,
            });
            continue;
        }
        return Ok(BackendSelection {
            backend: probe.backend,
            label: profile.label(),
            compatibility: profile.compatibility().cloned(),
            profile_digest: profile.digest(),
        });
    }

    Err(BackendSelectionError::RequiredBackendUnavailable {
        label: profile.label(),
        platform: profile.platform(),
        architecture: profile.architecture(),
        required: profile.requirements().clone(),
        rejected,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileError {
    UnsupportedSchemaVersion(u16),
    MissingCompatibilityOptIn,
    UnexpectedCompatibilityOptIn,
    InvalidCompatibilityTier(TrustTier),
    EmptyCompatibilityReason,
    MissingMount(MountRole),
    DuplicateMount(MountRole),
    DuplicateMountTarget(PathBuf),
    OverlappingMountTargets(PathBuf, PathBuf),
    UnsafeMountTarget(PathBuf),
    InvalidMountAccess {
        role: MountRole,
        expected: &'static str,
        actual: MountAccess,
    },
    InvalidCredentialHandle,
    DuplicateCredentialHandle(String),
    InvalidEnvironmentVariable(String),
    DuplicateEnvironmentVariable(String),
    UnsafeEgressDestination(String),
    ZeroEgressPort,
    UnboundedResource(&'static str),
    InvalidBackendId,
    Canonicalization(String),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion(version) => {
                write!(f, "unsupported executor profile schema version {version}")
            }
            Self::MissingCompatibilityOptIn => {
                f.write_str("host compatibility requires an explicit opt-in record")
            }
            Self::UnexpectedCompatibilityOptIn => {
                f.write_str("isolated profiles cannot carry a compatibility opt-in")
            }
            Self::InvalidCompatibilityTier(tier) => write!(
                f,
                "host compatibility can only be weaker than trusted local, not {}",
                tier.label()
            ),
            Self::EmptyCompatibilityReason => {
                f.write_str("compatibility opt-in reason must not be empty")
            }
            Self::MissingMount(role) => write!(f, "missing {role:?} mount"),
            Self::DuplicateMount(role) => write!(f, "duplicate {role:?} mount"),
            Self::DuplicateMountTarget(target) => {
                write!(f, "duplicate mount target {}", target.display())
            }
            Self::OverlappingMountTargets(left, right) => write!(
                f,
                "mount targets {} and {} overlap",
                left.display(),
                right.display()
            ),
            Self::UnsafeMountTarget(target) => {
                write!(
                    f,
                    "mount target {} is not a normalized absolute path",
                    target.display()
                )
            }
            Self::InvalidMountAccess {
                role,
                expected,
                actual,
            } => write!(f, "{role:?} mount must be {expected}, not {actual:?}"),
            Self::InvalidCredentialHandle => {
                f.write_str("credential handle contains invalid characters")
            }
            Self::DuplicateCredentialHandle(handle) => {
                write!(f, "duplicate credential handle {handle}")
            }
            Self::InvalidEnvironmentVariable(variable) => {
                write!(f, "invalid scoped environment variable {variable}")
            }
            Self::DuplicateEnvironmentVariable(variable) => {
                write!(f, "duplicate scoped environment variable {variable}")
            }
            Self::UnsafeEgressDestination(destination) => {
                write!(f, "unsafe egress destination {destination}")
            }
            Self::ZeroEgressPort => f.write_str("egress grant port must be non-zero"),
            Self::UnboundedResource(resource) => {
                write!(
                    f,
                    "executor resource bound {resource} must be finite and non-zero"
                )
            }
            Self::InvalidBackendId => f.write_str("backend id must not be empty"),
            Self::Canonicalization(error) => {
                write!(f, "executor profile canonicalization failed: {error}")
            }
        }
    }
}

impl std::error::Error for ProfileError {}

fn validate(spec: &ProfileSpec) -> Result<(), ProfileError> {
    if spec.schema_version != EXECUTOR_PROFILE_SCHEMA_VERSION {
        return Err(ProfileError::UnsupportedSchemaVersion(spec.schema_version));
    }
    match (spec.label, &spec.compatibility) {
        (ExecutionLabel::HostCompatibility, None) => {
            return Err(ProfileError::MissingCompatibilityOptIn);
        }
        (ExecutionLabel::HostCompatibility, Some(opt_in))
            if opt_in.weaker_than != TrustTier::TrustedLocal =>
        {
            return Err(ProfileError::InvalidCompatibilityTier(opt_in.weaker_than));
        }
        (ExecutionLabel::HostCompatibility, Some(opt_in)) if opt_in.reason.trim().is_empty() => {
            return Err(ProfileError::EmptyCompatibilityReason);
        }
        (label, Some(_)) if label.is_isolation() => {
            return Err(ProfileError::UnexpectedCompatibilityOptIn);
        }
        _ => {}
    }
    validate_mounts(spec)?;
    validate_credentials(&spec.credentials)?;
    validate_egress(&spec.egress)?;
    validate_resources(spec.resources)?;
    Ok(())
}

fn validate_mounts(spec: &ProfileSpec) -> Result<(), ProfileError> {
    let mut roles = BTreeMap::new();
    let mut targets = BTreeMap::new();
    for mount in &spec.mounts {
        let Some(target) = canonical_virtual_path(&mount.target, spec.platform) else {
            return Err(ProfileError::UnsafeMountTarget(mount.target.clone()));
        };
        if roles.insert(mount.role, mount.access).is_some() {
            return Err(ProfileError::DuplicateMount(mount.role));
        }
        if targets.insert(target, &mount.target).is_some() {
            return Err(ProfileError::DuplicateMountTarget(mount.target.clone()));
        }
    }
    for role in MountRole::ALL {
        if !roles.contains_key(&role) {
            return Err(ProfileError::MissingMount(role));
        }
    }
    let expected_root = match spec.platform {
        Platform::Windows => "c:/",
        Platform::Linux | Platform::MacOs => "/",
    };
    let actual_root = spec
        .mounts
        .iter()
        .find(|mount| mount.role == MountRole::Root)
        .expect("all mount roles were checked above");
    if canonical_virtual_path(&actual_root.target, spec.platform).as_deref() != Some(expected_root)
    {
        return Err(ProfileError::UnsafeMountTarget(actual_root.target.clone()));
    }
    let non_root = spec
        .mounts
        .iter()
        .filter(|mount| mount.role != MountRole::Root)
        .collect::<Vec<_>>();
    for (index, left) in non_root.iter().enumerate() {
        for right in &non_root[index + 1..] {
            if virtual_path_contains(&left.target, &right.target, spec.platform)
                || virtual_path_contains(&right.target, &left.target, spec.platform)
            {
                return Err(ProfileError::OverlappingMountTargets(
                    left.target.clone(),
                    right.target.clone(),
                ));
            }
        }
    }
    let check = |role, valid: fn(MountAccess) -> bool, expected| {
        let actual = roles[&role];
        if valid(actual) {
            Ok(())
        } else {
            Err(ProfileError::InvalidMountAccess {
                role,
                expected,
                actual,
            })
        }
    };
    if spec.label == ExecutionLabel::HostCompatibility {
        check(
            MountRole::Root,
            |access| access == MountAccess::ReadWrite,
            "read-write in host compatibility mode",
        )?;
    } else {
        check(
            MountRole::Root,
            |access| access == MountAccess::ReadOnly,
            "read-only",
        )?;
    }
    match spec.source_write {
        SourceWriteMode::ReadOnly => check(
            MountRole::Source,
            |access| access == MountAccess::ReadOnly,
            "read-only when source writes are disabled",
        )?,
        SourceWriteMode::MutationOverlay => check(
            MountRole::Source,
            |access| access == MountAccess::CopyOnWrite,
            "copy-on-write when mutation overlay is enabled",
        )?,
        SourceWriteMode::Direct => {
            if spec.label != ExecutionLabel::HostCompatibility {
                return Err(ProfileError::InvalidMountAccess {
                    role: MountRole::Source,
                    expected: "direct writes only in host compatibility mode",
                    actual: roles[&MountRole::Source],
                });
            }
            check(
                MountRole::Source,
                |access| access == MountAccess::ReadWrite,
                "read-write when direct host writes are enabled",
            )?;
        }
    }
    for role in [MountRole::Build, MountRole::Temp] {
        check(
            role,
            |access| matches!(access, MountAccess::ReadWrite | MountAccess::CopyOnWrite),
            "writable or copy-on-write",
        )?;
    }
    Ok(())
}

fn validate_credentials(credentials: &[CredentialInjection]) -> Result<(), ProfileError> {
    let mut handles = BTreeSet::new();
    let mut variables = BTreeSet::new();
    for credential in credentials {
        if !valid_credential_handle(credential.handle.as_str()) {
            return Err(ProfileError::InvalidCredentialHandle);
        }
        if !handles.insert(credential.handle.as_str()) {
            return Err(ProfileError::DuplicateCredentialHandle(
                credential.handle.as_str().to_owned(),
            ));
        }
        if let CredentialInjectionMode::ScopedEnvironment { variable } = &credential.mode {
            if !valid_environment_variable(variable) {
                return Err(ProfileError::InvalidEnvironmentVariable(variable.clone()));
            }
            if !variables.insert(variable) {
                return Err(ProfileError::DuplicateEnvironmentVariable(variable.clone()));
            }
        }
    }
    Ok(())
}

fn validate_egress(egress: &BTreeSet<EgressGrant>) -> Result<(), ProfileError> {
    for grant in egress {
        if !valid_destination(&grant.destination) {
            return Err(ProfileError::UnsafeEgressDestination(
                grant.destination.clone(),
            ));
        }
        if grant.port == 0 {
            return Err(ProfileError::ZeroEgressPort);
        }
    }
    Ok(())
}

fn validate_resources(resources: ResourceLimits) -> Result<(), ProfileError> {
    for (name, value) in [
        ("cpu", resources.cpu_millis),
        ("memory", resources.memory_bytes),
        ("pid", u64::from(resources.pids)),
        ("file", resources.file_bytes),
        ("disk", resources.disk_bytes),
        ("io", resources.io_bytes),
        ("output", resources.output_bytes),
        ("wall_time", resources.wall_time_millis),
    ] {
        if value == 0 {
            return Err(ProfileError::UnboundedResource(name));
        }
    }
    Ok(())
}

fn requirements(spec: &ProfileSpec) -> BTreeSet<BackendPrimitive> {
    use BackendPrimitive as P;

    let mut required = spec.additional_requirements.clone();
    match spec.label {
        ExecutionLabel::TrustedLocal => {
            required.extend([
                P::OsSandbox,
                P::FilesystemBoundary,
                P::ProcessBoundary,
                P::ReadOnlyMount,
                P::WritableMount,
                P::ScrubbedEnvironment,
                P::NetworkDeny,
                P::OutputLimit,
                P::WallTimeLimit,
                P::WholeProcessTreeControl,
            ]);
        }
        ExecutionLabel::Restricted => {
            required.extend([
                P::FilesystemBoundary,
                P::ProcessBoundary,
                P::PrivilegeBoundary,
                P::SyscallPolicy,
                P::ReadOnlyMount,
                P::WritableMount,
                P::ScrubbedEnvironment,
                P::NetworkDeny,
                P::WholeProcessTreeControl,
                P::CpuLimit,
                P::MemoryLimit,
                P::PidLimit,
                P::FileSizeLimit,
                P::DiskLimit,
                P::IoLimit,
                P::OutputLimit,
                P::WallTimeLimit,
            ]);
        }
        ExecutionLabel::Hostile => {
            required.extend([
                P::FilesystemBoundary,
                P::ProcessBoundary,
                P::TenantBoundary,
                P::IsolatedStorage,
                P::ReadOnlyMount,
                P::WritableMount,
                P::ScrubbedEnvironment,
                P::NetworkDeny,
                P::WholeProcessTreeControl,
                P::CpuLimit,
                P::MemoryLimit,
                P::PidLimit,
                P::FileSizeLimit,
                P::DiskLimit,
                P::IoLimit,
                P::OutputLimit,
                P::WallTimeLimit,
            ]);
        }
        ExecutionLabel::HostCompatibility => {
            required.extend([
                P::ScrubbedEnvironment,
                P::ProcessGroup,
                P::OutputLimit,
                P::WallTimeLimit,
            ]);
        }
    }
    match (spec.label, spec.platform) {
        (ExecutionLabel::TrustedLocal, Platform::Linux | Platform::MacOs) => {}
        (ExecutionLabel::TrustedLocal, Platform::Windows) => {
            required.extend([P::WindowsJobObject, P::ContainerOrVmBoundary]);
        }
        (ExecutionLabel::Restricted, Platform::Linux) => {
            required.extend([P::UserNamespace, P::RootlessBoundary]);
        }
        (ExecutionLabel::Restricted, Platform::MacOs) => {
            required.insert(P::OsSandbox);
        }
        (ExecutionLabel::Restricted, Platform::Windows) => {
            required.extend([P::WindowsJobObject, P::ContainerOrVmBoundary]);
        }
        (ExecutionLabel::Hostile, Platform::Linux) => {
            required.insert(P::UserKernelOrVmTenantBoundary);
        }
        (ExecutionLabel::Hostile, Platform::MacOs) => {
            required.insert(P::VmTenantBoundary);
        }
        (ExecutionLabel::Hostile, Platform::Windows) => {
            required.extend([P::WindowsJobObject, P::VmTenantBoundary]);
        }
        _ => {}
    }
    if spec.label != ExecutionLabel::HostCompatibility {
        match spec.source_write {
            SourceWriteMode::ReadOnly => {
                required.insert(P::ReadOnlySource);
            }
            SourceWriteMode::MutationOverlay => {
                required.extend([P::CopyOnWriteMount, P::SourceMutationOverlay]);
            }
            SourceWriteMode::Direct => {}
        }
    }
    for credential in &spec.credentials {
        required.insert(match credential.mode {
            CredentialInjectionMode::FileDescriptor => P::CredentialFileDescriptor,
            CredentialInjectionMode::MemoryFile => P::CredentialMemoryFile,
            CredentialInjectionMode::ScopedEnvironment { .. } => P::CredentialScopedEnvironment,
        });
    }
    if !spec.egress.is_empty() {
        required.extend([P::DestinationEgress, P::RebindingSafeEgress]);
    }
    if spec.label != ExecutionLabel::HostCompatibility {
        for policy in [spec.repository.hooks, spec.repository.submodules] {
            required.insert(match policy {
                RepositoryCodePolicy::Disabled => P::RepositoryCodeDisabled,
                RepositoryCodePolicy::Sandboxed => P::RepositoryCodeSandbox,
                RepositoryCodePolicy::Unrestricted => continue,
            });
        }
    }
    if spec
        .mounts
        .iter()
        .any(|mount| mount.access == MountAccess::CopyOnWrite)
    {
        required.insert(P::CopyOnWriteMount);
    }
    required
}

fn canonical_virtual_path(path: &Path, platform: Platform) -> Option<String> {
    let path = path.to_str()?;
    if !path.is_ascii() {
        return None;
    }
    match platform {
        Platform::Linux | Platform::MacOs => {
            let rest = path.strip_prefix('/')?;
            if !rest.is_empty()
                && !rest.split('/').all(|component| {
                    !component.is_empty()
                        && !matches!(component, "." | "..")
                        && !component.contains('\\')
                })
            {
                return None;
            }
            Some(path.to_owned())
        }
        Platform::Windows => {
            let bytes = path.as_bytes();
            if bytes.len() < 3
                || !bytes[0].is_ascii_alphabetic()
                || bytes[1] != b':'
                || bytes[2] != b'\\'
                || path.contains('/')
            {
                return None;
            }
            let rest = &path[3..];
            if !rest.is_empty() && !rest.split('\\').all(valid_windows_segment) {
                return None;
            }
            Some(format!(
                "{}:/{}",
                path[..1].to_ascii_lowercase(),
                rest.replace('\\', "/").to_lowercase()
            ))
        }
    }
}

fn valid_windows_segment(segment: &str) -> bool {
    if segment.is_empty()
        || matches!(segment, "." | "..")
        || segment.ends_with(['.', ' '])
        || segment.contains('~')
        || segment.chars().any(|character| {
            character <= '\u{1f}' || matches!(character, '<' | '>' | '"' | '|' | '?' | '*' | ':')
        })
    {
        return false;
    }
    let stem = segment
        .split('.')
        .next()
        .unwrap()
        .trim_end_matches(['.', ' '])
        .to_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|digit| {
            matches!(
                digit,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

fn virtual_path_contains(parent: &Path, child: &Path, platform: Platform) -> bool {
    let parent = canonical_virtual_path(parent, platform).expect("mount paths were validated");
    let child = canonical_virtual_path(child, platform).expect("mount paths were validated");
    child
        .strip_prefix(&parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn valid_environment_variable(variable: &str) -> bool {
    const RESERVED: [&str; 16] = [
        "ALL_PROXY",
        "DOCKER_HOST",
        "HOME",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "LANG",
        "LC_ALL",
        "NO_PROXY",
        "PATH",
        "SSH_AUTH_SOCK",
        "TEMP",
        "TMP",
        "TMPDIR",
        "XDG_RUNTIME_DIR",
        "CONTAINER_HOST",
        "PODMAN_HOST",
    ];
    let mut bytes = variable.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z') | Some(b'_'))
        && bytes.all(|byte| matches!(byte, b'A'..=b'Z' | b'0'..=b'9' | b'_'))
        && !RESERVED.contains(&variable)
        && !variable.starts_with("LD_")
        && !variable.starts_with("DYLD_")
}

fn valid_credential_handle(handle: &str) -> bool {
    !handle.is_empty()
        && handle.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
}

fn valid_destination(destination: &str) -> bool {
    if let Ok(address) = destination.parse::<IpAddr>() {
        return match address {
            IpAddr::V4(address) => globally_routable_ipv4(address),
            IpAddr::V6(address) => embedded_ipv4(address)
                .map_or_else(|| globally_routable_ipv6(address), globally_routable_ipv4),
        };
    }

    let blocked = [
        "localhost",
        "metadata.google.internal",
        "metadata.aws.internal",
        "instance-data.ec2.internal",
    ];
    !blocked.contains(&destination)
        && !destination.ends_with(".localhost")
        && destination.len() <= 253
        && !destination.ends_with('.')
        && destination.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
}

fn globally_routable_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, d] = address.octets();
    !matches!(
        (a, b, c, d),
        (0, ..)
            | (10, ..)
            | (100, 64..=127, ..)
            | (127, ..)
            | (169, 254, ..)
            | (172, 16..=31, ..)
            | (192, 0, 0, 0..=8 | 11..=255)
            | (192, 0, 2, _)
            | (192, 168, ..)
            | (198, 18..=19, ..)
            | (198, 51, 100, _)
            | (203, 0, 113, _)
            | (224..=255, ..)
    )
}

fn globally_routable_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    let ietf_protocol_assignment = segments[0] == 0x2001
        && segments[1] < 0x200
        && !(u128::from(address) == 0x2001_0001_0000_0000_0000_0000_0000_0001
            || u128::from(address) == 0x2001_0001_0000_0000_0000_0000_0000_0002
            || segments[..2] == [0x2001, 3]
            || segments[..3] == [0x2001, 4, 0x112]
            || (segments[0] == 0x2001 && matches!(segments[1], 0x20..=0x3f)));
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || segments[..3] == [0x64, 0xff9b, 1]
        || segments[..4] == [0x100, 0, 0, 0]
        || ietf_protocol_assignment
        || segments[0] == 0x2002
        || matches!(segments[..2], [0x2001, 0xdb8] | [0x3fff, 0..=0x0fff])
        || segments[0] == 0x5f00
        || segments[0] & 0xffc0 == 0xfec0
        || address.is_unique_local()
        || address.is_unicast_link_local())
}

fn embedded_ipv4(address: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(address) = address.to_ipv4() {
        return Some(address);
    }
    let octets = address.octets();
    (octets[..12] == [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0])
        .then(|| Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15]))
}
