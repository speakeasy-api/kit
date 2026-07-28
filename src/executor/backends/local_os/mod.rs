use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::{
    domain::lifecycle::ProcessOwnership,
    executor::{
        process::{
            own::{PreparedCommandToken, ProcessOutput, ProcessState, spawn_owned},
            tree::{
                BoundaryControl, BoundaryIdentity, BoundaryKind, Containment, Inspection,
                PersistedBoundary,
            },
        },
        profile::{
            Architecture, BackendId, BackendPrimitive, BackendProbe, ExecutionLabel,
            ExecutorProfile, Platform, ProbeStatus, ResourceLimits,
        },
    },
};

const MACOS_SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const LINUX_BWRAP_CANDIDATES: [&str; 2] = ["/usr/bin/bwrap", "/bin/bwrap"];
const LINUX_SYSTEM_DIRS: [&str; 5] = ["/usr", "/bin", "/sbin", "/lib", "/lib64"];
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_SETTLE_TIME: Duration = Duration::from_millis(1_100);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotAvailableReason {
    UnsupportedHost,
    ProfileHostMismatch,
    UnsupportedLabel,
    InvalidPaths,
    PrimitiveMissing,
    ProbeFailed,
    CredentialCustodyUnavailable,
    MountCustodyUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotAvailable {
    pub reason: NotAvailableReason,
    pub detail: String,
    pub missing_primitives: BTreeSet<BackendPrimitive>,
}

impl NotAvailable {
    fn new(reason: NotAvailableReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
            missing_primitives: BTreeSet::new(),
        }
    }

    fn missing_primitives(missing_primitives: BTreeSet<BackendPrimitive>) -> Self {
        Self {
            reason: NotAvailableReason::PrimitiveMissing,
            detail: format!("missing required local executor primitives: {missing_primitives:?}"),
            missing_primitives,
        }
    }
}

impl fmt::Display for NotAvailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "local executor not available: {}", self.detail)
    }
}

impl std::error::Error for NotAvailable {}

#[derive(Debug)]
pub enum LocalExecutionError {
    NotAvailable(NotAvailable),
    InvalidRequest(String),
    Io(io::Error),
}

impl fmt::Display for LocalExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAvailable(error) => error.fmt(f),
            Self::InvalidRequest(error) => write!(f, "invalid local execution request: {error}"),
            Self::Io(error) => write!(f, "local execution failed: {error}"),
        }
    }
}

impl std::error::Error for LocalExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotAvailable(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::InvalidRequest(_) => None,
        }
    }
}

impl From<NotAvailable> for LocalExecutionError {
    fn from(value: NotAvailable) -> Self {
        Self::NotAvailable(value)
    }
}

impl From<io::Error> for LocalExecutionError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxPaths {
    source: PathBuf,
    build: PathBuf,
    temp: PathBuf,
    identities: [PathIdentity; 3],
}

impl SandboxPaths {
    pub fn new(
        source: impl AsRef<Path>,
        build: impl AsRef<Path>,
        temp: impl AsRef<Path>,
    ) -> Result<Self, NotAvailable> {
        let source = canonical_directory(source.as_ref())?;
        let build = canonical_directory(build.as_ref())?;
        let temp = canonical_directory(temp.as_ref())?;
        let paths = Self {
            identities: [
                PathIdentity::read(&source)?,
                PathIdentity::read(&build)?,
                PathIdentity::read(&temp)?,
            ],
            source,
            build,
            temp,
        };
        let all = [&paths.source, &paths.build, &paths.temp];
        for (index, left) in all.iter().enumerate() {
            for right in &all[index + 1..] {
                if left.starts_with(right) || right.starts_with(left) {
                    return Err(NotAvailable::new(
                        NotAvailableReason::InvalidPaths,
                        "source, build, and temporary directories must not overlap",
                    ));
                }
            }
        }
        paths.revalidate()?;
        Ok(paths)
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn build(&self) -> &Path {
        &self.build
    }

    pub fn temp(&self) -> &Path {
        &self.temp
    }

    fn revalidate(&self) -> Result<(), NotAvailable> {
        for (path, identity) in [self.source(), self.build(), self.temp()]
            .into_iter()
            .zip(&self.identities)
        {
            identity.revalidate(path)?;
        }
        #[cfg(target_os = "linux")]
        {
            validate_linux_tree(self.source())?;
            reject_nested_linux_mounts([self.source(), self.build(), self.temp()])?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PathIdentity {
    device: u64,
    inode: u64,
}

impl PathIdentity {
    #[cfg(unix)]
    fn read(path: &Path) -> Result<Self, NotAvailable> {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = fs::metadata(path).map_err(|error| invalid_path(path, error))?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    #[cfg(not(unix))]
    fn read(_path: &Path) -> Result<Self, NotAvailable> {
        Err(NotAvailable::new(
            NotAvailableReason::UnsupportedHost,
            "local path identity pinning requires Unix metadata",
        ))
    }

    fn revalidate(self, path: &Path) -> Result<(), NotAvailable> {
        if Self::read(path)? == self {
            Ok(())
        } else {
            Err(NotAvailable::new(
                NotAvailableReason::InvalidPaths,
                format!(
                    "{} changed after its mount identity was pinned",
                    path.display()
                ),
            ))
        }
    }
}

fn invalid_path(path: &Path, error: io::Error) -> NotAvailable {
    NotAvailable::new(
        NotAvailableReason::InvalidPaths,
        format!("cannot inspect {}: {error}", path.display()),
    )
}

fn canonical_directory(path: &Path) -> Result<PathBuf, NotAvailable> {
    let path = fs::canonicalize(path).map_err(|error| {
        NotAvailable::new(
            NotAvailableReason::InvalidPaths,
            format!("cannot resolve {}: {error}", path.display()),
        )
    })?;
    if !path.is_dir() {
        return Err(NotAvailable::new(
            NotAvailableReason::InvalidPaths,
            format!("{} is not a directory", path.display()),
        ));
    }
    Ok(path)
}

#[cfg(target_os = "linux")]
fn validate_linux_tree(root: &Path) -> Result<(), NotAvailable> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

    let root_device = fs::metadata(root)
        .map_err(|error| invalid_path(root, error))?
        .dev();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).map_err(|error| invalid_path(&directory, error))? {
            let entry = entry.map_err(|error| invalid_path(&directory, error))?;
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(|error| invalid_path(&path, error))?;
            let kind = metadata.file_type();
            if kind.is_symlink() {
                let target = fs::canonicalize(&path).map_err(|error| invalid_path(&path, error))?;
                if !target.starts_with(root) {
                    return Err(NotAvailable::new(
                        NotAvailableReason::InvalidPaths,
                        format!(
                            "source symlink {} escapes the source boundary",
                            path.display()
                        ),
                    ));
                }
            } else if kind.is_dir() {
                if metadata.dev() != root_device {
                    return Err(NotAvailable::new(
                        NotAvailableReason::InvalidPaths,
                        format!(
                            "source directory {} crosses a filesystem boundary",
                            path.display()
                        ),
                    ));
                }
                pending.push(path);
            } else if kind.is_file() {
                if metadata.nlink() != 1 {
                    return Err(NotAvailable::new(
                        NotAvailableReason::InvalidPaths,
                        format!(
                            "source file {} has a cross-boundary-capable hard link",
                            path.display()
                        ),
                    ));
                }
            } else if kind.is_socket() {
                return Err(NotAvailable::new(
                    NotAvailableReason::InvalidPaths,
                    format!("source path {} is a Unix socket", path.display()),
                ));
            } else {
                return Err(NotAvailable::new(
                    NotAvailableReason::InvalidPaths,
                    format!(
                        "source path {} is not a regular file or directory",
                        path.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn reject_nested_linux_mounts<'a>(
    roots: impl IntoIterator<Item = &'a Path>,
) -> Result<(), NotAvailable> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").map_err(|error| {
        NotAvailable::new(
            NotAvailableReason::InvalidPaths,
            format!("cannot inspect Linux mount topology: {error}"),
        )
    })?;
    let roots = roots.into_iter().collect::<Vec<_>>();
    for line in mountinfo.lines() {
        let Some(encoded) = line.split_whitespace().nth(4) else {
            return Err(NotAvailable::new(
                NotAvailableReason::InvalidPaths,
                "malformed /proc/self/mountinfo entry",
            ));
        };
        let mount = PathBuf::from(unescape_mountinfo(encoded));
        if roots
            .iter()
            .any(|root| mount.as_path() != *root && mount.starts_with(root))
        {
            return Err(NotAvailable::new(
                NotAvailableReason::InvalidPaths,
                format!(
                    "nested mount {} crosses a local sandbox boundary",
                    mount.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unescape_mountinfo(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendKind {
    MacOsSeatbelt,
    LinuxBubblewrap,
    HostCompatibility,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalOsBackend {
    kind: BackendKind,
    program: Option<PathBuf>,
}

impl LocalOsBackend {
    /// Selects only the backend named by the profile. Trusted-local failure never falls back to
    /// host compatibility; compatibility has its own explicit profile label and opt-in record.
    pub fn select(profile: &ExecutorProfile, paths: &SandboxPaths) -> Result<Self, NotAvailable> {
        validate_host(profile)?;
        validate_mount_targets(profile)?;
        paths.revalidate()?;
        if !profile.credentials().is_empty() {
            return Err(NotAvailable::new(
                NotAvailableReason::CredentialCustodyUnavailable,
                "local execution cannot prove complete sandbox and secret custody through quiescence",
            ));
        }
        let mut backend = match profile.label() {
            ExecutionLabel::TrustedLocal => Self {
                kind: native_kind()?,
                program: None,
            },
            ExecutionLabel::HostCompatibility if profile.compatibility().is_some() => Self {
                kind: BackendKind::HostCompatibility,
                program: None,
            },
            label => Err(NotAvailable::new(
                NotAvailableReason::UnsupportedLabel,
                format!("{} is not a local OS backend label", label.label()),
            ))?,
        };
        validate_requirements(profile, &backend.capabilities())?;
        match backend.kind {
            BackendKind::LinuxBubblewrap if cfg!(target_os = "linux") => {
                return Err(NotAvailable::new(
                    NotAvailableReason::MountCustodyUnavailable,
                    "Linux trusted-local mounts are not descriptor or lease pinned through bubblewrap setup",
                ));
            }
            BackendKind::HostCompatibility if cfg!(target_os = "macos") => {
                return Err(NotAvailable::new(
                    NotAvailableReason::PrimitiveMissing,
                    "macOS host compatibility has no complete reconstructible process boundary",
                ));
            }
            _ => {}
        }
        if backend.is_isolation() {
            backend.program = Some(native_program(backend.kind)?);
            SystemCapabilityRunner.run(&backend.capability_command(paths)?)?;
        } else if cfg!(target_os = "linux") {
            backend.program = Some(native_program(BackendKind::LinuxBubblewrap)?);
            SystemCapabilityRunner.run(&backend.compatibility_capability_command(paths)?)?;
        }
        Ok(backend)
    }

    pub const fn label(&self) -> ExecutionLabel {
        match self.kind {
            BackendKind::MacOsSeatbelt | BackendKind::LinuxBubblewrap => {
                ExecutionLabel::TrustedLocal
            }
            BackendKind::HostCompatibility => ExecutionLabel::HostCompatibility,
        }
    }

    pub const fn is_isolation(&self) -> bool {
        !matches!(self.kind, BackendKind::HostCompatibility)
    }

    pub const fn description(&self) -> &'static str {
        match self.kind {
            BackendKind::MacOsSeatbelt => "trusted-local macOS Seatbelt sandbox",
            BackendKind::LinuxBubblewrap => "trusted-local Linux namespace sandbox",
            BackendKind::HostCompatibility => "explicit host compatibility runner (not isolation)",
        }
    }

    pub fn capabilities(&self) -> BTreeSet<BackendPrimitive> {
        use BackendPrimitive as P;

        let mut capabilities = BTreeSet::from([
            P::ScrubbedEnvironment,
            P::ProcessGroup,
            P::OutputLimit,
            P::WallTimeLimit,
        ]);
        if self.is_isolation() {
            capabilities.extend([
                P::OsSandbox,
                P::FilesystemBoundary,
                P::ProcessBoundary,
                P::ReadOnlyMount,
                P::WritableMount,
                P::ReadOnlySource,
                P::NetworkDeny,
                P::RepositoryCodeSandbox,
            ]);
            if self.kind == BackendKind::LinuxBubblewrap {
                capabilities.insert(P::WholeProcessTreeControl);
            }
        }
        capabilities
    }

    pub fn probe(profile: &ExecutorProfile, paths: &SandboxPaths) -> BackendProbe {
        let selection = Self::select(profile, paths);
        let (backend, capabilities, status) = match selection {
            Ok(backend) => (
                backend_id(backend.kind),
                backend.capabilities(),
                ProbeStatus::Available,
            ),
            Err(error) => (
                BackendId::new("local-os-unavailable").expect("static backend ID is valid"),
                BTreeSet::new(),
                ProbeStatus::Unavailable {
                    reason: error.to_string(),
                },
            ),
        };
        BackendProbe {
            backend,
            label: profile.label(),
            platform: profile.platform(),
            architecture: profile.architecture(),
            capabilities,
            status,
        }
    }

    pub fn prepare(
        &self,
        profile: &ExecutorProfile,
        paths: &SandboxPaths,
        request: LocalCommand,
    ) -> Result<PreparedCommand, LocalExecutionError> {
        validate_host(profile)?;
        validate_mount_targets(profile)?;
        paths.revalidate()?;
        if profile.label() != self.label() {
            return Err(LocalExecutionError::InvalidRequest(format!(
                "{} backend cannot run a {} profile",
                self.label().label(),
                profile.label().label()
            )));
        }
        validate_requirements(profile, &self.capabilities())?;
        let program = canonical_program(&request.program)?;
        let current_dir = canonical_directory(&request.current_dir)?;
        if !is_inside_workspace(&current_dir, paths) {
            return Err(LocalExecutionError::InvalidRequest(
                "working directory is outside source/build/temp".to_owned(),
            ));
        }
        validate_environment(&request.environment)?;
        let mut pinned_paths = vec![PinnedPath::new(&program)?];
        if let Some(wrapper) = &self.program {
            pinned_paths.push(PinnedPath::new(wrapper)?);
        }

        match self.kind {
            BackendKind::MacOsSeatbelt => Ok(PreparedCommand {
                program: self.program.clone().expect("native backend has a program"),
                args: seatbelt_args(paths, &program, &request.args),
                current_dir,
                environment: scrubbed_environment(paths, request.environment, false),
                label: self.label(),
                resources: profile.resources(),
                boundary_root: boundary_root(paths),
                sandbox_paths: paths.clone(),
                pinned_paths,
                probe_marker: None,
            }),
            BackendKind::LinuxBubblewrap => {
                let (sandbox_program, sandbox_cwd) = linux_path(&program, &current_dir, paths)?;
                let environment = scrubbed_environment(paths, request.environment, true);
                Ok(PreparedCommand {
                    program: self.program.clone().expect("native backend has a program"),
                    args: bubblewrap_args(
                        paths,
                        &program,
                        &sandbox_program,
                        &sandbox_cwd,
                        &environment,
                        &request.args,
                    ),
                    current_dir: PathBuf::from("/"),
                    environment,
                    label: self.label(),
                    resources: profile.resources(),
                    boundary_root: boundary_root(paths),
                    sandbox_paths: paths.clone(),
                    pinned_paths,
                    probe_marker: None,
                })
            }
            BackendKind::HostCompatibility => {
                let environment = scrubbed_environment(paths, request.environment, false);
                if cfg!(target_os = "linux") {
                    Ok(PreparedCommand {
                        program: self
                            .program
                            .clone()
                            .expect("Linux compatibility uses bwrap"),
                        args: bubblewrap_compatibility_args(
                            &program,
                            &current_dir,
                            &environment,
                            &request.args,
                        ),
                        current_dir: PathBuf::from("/"),
                        environment,
                        label: self.label(),
                        resources: profile.resources(),
                        boundary_root: boundary_root(paths),
                        sandbox_paths: paths.clone(),
                        pinned_paths,
                        probe_marker: None,
                    })
                } else {
                    Ok(PreparedCommand {
                        program,
                        args: request.args,
                        current_dir,
                        environment,
                        label: self.label(),
                        resources: profile.resources(),
                        boundary_root: boundary_root(paths),
                        sandbox_paths: paths.clone(),
                        pinned_paths,
                        probe_marker: None,
                    })
                }
            }
        }
    }

    pub fn structural_policy_for_test(
        paths: &SandboxPaths,
    ) -> Result<StructuralPolicy, NotAvailable> {
        let kind = native_kind()?;
        let backend = Self {
            kind,
            program: Some(native_program_path(kind)),
        };
        let command = backend.capability_command(paths)?;
        Ok(StructuralPolicy {
            program: command.program,
            args: command.args,
        })
    }

    fn capability_command(&self, paths: &SandboxPaths) -> Result<PreparedCommand, NotAvailable> {
        match self.kind {
            BackendKind::MacOsSeatbelt => {
                let marker_name = format!(
                    ".kit-probe-{}",
                    random_token().map_err(|error| {
                        NotAvailable::new(
                            NotAvailableReason::ProbeFailed,
                            format!("cannot create capability probe identity: {error}"),
                        )
                    })?
                );
                let mut environment = scrubbed_environment(paths, BTreeMap::new(), false);
                environment.insert(
                    OsString::from("KIT_PROBE_SOURCE"),
                    paths.source.as_os_str().to_owned(),
                );
                environment.insert(
                    OsString::from("KIT_PROBE_BUILD"),
                    paths.build.as_os_str().to_owned(),
                );
                Ok(PreparedCommand {
                    program: PathBuf::from(MACOS_SANDBOX_EXEC),
                    args: seatbelt_args(
                        paths,
                        Path::new("/bin/sh"),
                        &[
                            OsString::from("-c"),
                            OsString::from(macos_probe_script(&marker_name)),
                        ],
                    ),
                    current_dir: paths.source.clone(),
                    environment,
                    label: ExecutionLabel::TrustedLocal,
                    resources: probe_limits(),
                    boundary_root: paths.temp.clone(),
                    sandbox_paths: paths.clone(),
                    pinned_paths: Vec::new(),
                    probe_marker: Some(paths.build.join(marker_name)),
                })
            }
            BackendKind::LinuxBubblewrap => {
                let marker_name = format!(
                    ".kit-probe-{}",
                    random_token().map_err(|error| {
                        NotAvailable::new(
                            NotAvailableReason::ProbeFailed,
                            format!("cannot create capability probe identity: {error}"),
                        )
                    })?
                );
                Ok(PreparedCommand {
                    program: self.program.clone().expect("native backend has a program"),
                    args: bubblewrap_args(
                        paths,
                        Path::new("/bin/sh"),
                        Path::new("/bin/sh"),
                        Path::new("/workspace"),
                        &scrubbed_environment(paths, BTreeMap::new(), true),
                        &[
                            OsString::from("-c"),
                            OsString::from(linux_isolation_probe_script(&marker_name)),
                        ],
                    ),
                    current_dir: PathBuf::from("/"),
                    environment: scrubbed_environment(paths, BTreeMap::new(), true),
                    label: ExecutionLabel::TrustedLocal,
                    resources: probe_limits(),
                    boundary_root: paths.temp.clone(),
                    sandbox_paths: paths.clone(),
                    pinned_paths: Vec::new(),
                    probe_marker: Some(paths.build.join(marker_name)),
                })
            }
            BackendKind::HostCompatibility => Err(NotAvailable::new(
                NotAvailableReason::UnsupportedLabel,
                "host compatibility is not an isolation capability",
            )),
        }
    }

    fn compatibility_capability_command(
        &self,
        paths: &SandboxPaths,
    ) -> Result<PreparedCommand, NotAvailable> {
        let marker = paths.build.join(format!(
            ".kit-probe-{}",
            random_token().map_err(|error| {
                NotAvailable::new(
                    NotAvailableReason::ProbeFailed,
                    format!("cannot create capability probe identity: {error}"),
                )
            })?
        ));
        let mut environment = scrubbed_environment(paths, BTreeMap::new(), false);
        environment.insert(
            OsString::from("KIT_PROBE_MARKER"),
            marker.as_os_str().to_owned(),
        );
        Ok(PreparedCommand {
            program: self.program.clone().ok_or_else(|| {
                NotAvailable::new(
                    NotAvailableReason::PrimitiveMissing,
                    "Linux compatibility process namespace helper is absent",
                )
            })?,
            args: bubblewrap_compatibility_args(
                Path::new("/bin/sh"),
                paths.source(),
                &environment,
                &[
                    OsString::from("-c"),
                    OsString::from(linux_compatibility_probe_script()),
                ],
            ),
            current_dir: PathBuf::from("/"),
            environment,
            label: ExecutionLabel::HostCompatibility,
            resources: probe_limits(),
            boundary_root: paths.temp.clone(),
            sandbox_paths: paths.clone(),
            pinned_paths: Vec::new(),
            probe_marker: Some(marker),
        })
    }
}

fn backend_id(kind: BackendKind) -> BackendId {
    let id = match kind {
        BackendKind::MacOsSeatbelt => "local-os-macos-seatbelt",
        BackendKind::LinuxBubblewrap => "local-os-linux-bubblewrap",
        BackendKind::HostCompatibility => "local-os-host-compatibility",
    };
    BackendId::new(id).expect("static backend ID is valid")
}

fn validate_host(profile: &ExecutorProfile) -> Result<(), NotAvailable> {
    if profile.platform() != current_platform()?
        || profile.architecture() != current_architecture()?
    {
        return Err(NotAvailable::new(
            NotAvailableReason::ProfileHostMismatch,
            "profile platform or architecture does not match this host",
        ));
    }
    Ok(())
}

fn validate_mount_targets(profile: &ExecutorProfile) -> Result<(), NotAvailable> {
    use crate::executor::profile::MountRole;

    let expected = match profile.platform() {
        Platform::Linux | Platform::MacOs => [
            (MountRole::Root, Path::new("/")),
            (MountRole::Source, Path::new("/workspace")),
            (MountRole::Build, Path::new("/build")),
            (MountRole::Temp, Path::new("/tmp")),
        ],
        Platform::Windows => [
            (MountRole::Root, Path::new(r"c:\")),
            (MountRole::Source, Path::new(r"c:\workspace")),
            (MountRole::Build, Path::new(r"c:\build")),
            (MountRole::Temp, Path::new(r"c:\temp")),
        ],
    };
    for (role, target) in expected {
        let actual = profile
            .mounts()
            .iter()
            .find(|mount| mount.role == role)
            .expect("profile mount roles were validated");
        if actual.target != target {
            return Err(NotAvailable::new(
                NotAvailableReason::InvalidPaths,
                format!(
                    "local executor requires the {role:?} target {}, not {}",
                    target.display(),
                    actual.target.display()
                ),
            ));
        }
    }
    Ok(())
}

fn validate_requirements(
    profile: &ExecutorProfile,
    capabilities: &BTreeSet<BackendPrimitive>,
) -> Result<(), NotAvailable> {
    let missing = profile
        .requirements()
        .difference(capabilities)
        .copied()
        .collect::<BTreeSet<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(NotAvailable::missing_primitives(missing))
    }
}

fn current_platform() -> Result<Platform, NotAvailable> {
    if cfg!(target_os = "macos") {
        Ok(Platform::MacOs)
    } else if cfg!(target_os = "linux") {
        Ok(Platform::Linux)
    } else {
        Err(NotAvailable::new(
            NotAvailableReason::UnsupportedHost,
            "trusted-local native sandbox supports macOS and Linux",
        ))
    }
}

fn current_architecture() -> Result<Architecture, NotAvailable> {
    if cfg!(target_arch = "x86_64") {
        Ok(Architecture::X86_64)
    } else if cfg!(target_arch = "aarch64") {
        Ok(Architecture::Aarch64)
    } else {
        Err(NotAvailable::new(
            NotAvailableReason::UnsupportedHost,
            "trusted-local native sandbox supports x86_64 and aarch64",
        ))
    }
}

fn native_kind() -> Result<BackendKind, NotAvailable> {
    if cfg!(target_os = "macos") {
        return Ok(BackendKind::MacOsSeatbelt);
    }
    if cfg!(target_os = "linux") {
        return Ok(BackendKind::LinuxBubblewrap);
    }
    Err(NotAvailable::new(
        NotAvailableReason::UnsupportedHost,
        "no native local sandbox is compiled for this host",
    ))
}

fn native_program_path(kind: BackendKind) -> PathBuf {
    match kind {
        BackendKind::MacOsSeatbelt => PathBuf::from(MACOS_SANDBOX_EXEC),
        BackendKind::LinuxBubblewrap => PathBuf::from(LINUX_BWRAP_CANDIDATES[0]),
        BackendKind::HostCompatibility => unreachable!("compatibility has no sandbox program"),
    }
}

fn native_program(kind: BackendKind) -> Result<PathBuf, NotAvailable> {
    match kind {
        BackendKind::MacOsSeatbelt => {
            let program = native_program_path(kind);
            if program.is_file() {
                Ok(program)
            } else {
                Err(NotAvailable::new(
                    NotAvailableReason::PrimitiveMissing,
                    "macOS sandbox-exec is absent",
                ))
            }
        }
        BackendKind::LinuxBubblewrap => LINUX_BWRAP_CANDIDATES
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .ok_or_else(|| {
                NotAvailable::new(
                    NotAvailableReason::PrimitiveMissing,
                    "bubblewrap with user/PID/network namespaces is absent",
                )
            }),
        BackendKind::HostCompatibility => unreachable!("compatibility has no sandbox program"),
    }
}

pub trait CapabilityRunner {
    fn run(&self, command: &PreparedCommand) -> Result<(), NotAvailable>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCapabilityRunner;

impl CapabilityRunner for SystemCapabilityRunner {
    fn run(&self, command: &PreparedCommand) -> Result<(), NotAvailable> {
        command.validate_launch()?;
        if let Some(marker) = &command.probe_marker {
            match fs::remove_file(marker) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(invalid_path(marker, error)),
            }
        }
        let mut process = command.command();
        process.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = process.spawn().map_err(|error| {
            NotAvailable::new(
                NotAvailableReason::ProbeFailed,
                format!("sandbox capability probe could not start: {error}"),
            )
        })?;
        let deadline = Instant::now() + PROBE_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|error| {
                NotAvailable::new(
                    NotAvailableReason::ProbeFailed,
                    format!("sandbox capability probe wait failed: {error}"),
                )
            })? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(NotAvailable::new(
                    NotAvailableReason::ProbeFailed,
                    "sandbox capability probe exceeded its five-second timeout",
                ));
            }
            thread::sleep(Duration::from_millis(2));
        };
        if !status.success() {
            return Err(NotAvailable::new(
                NotAvailableReason::PrimitiveMissing,
                format!("sandbox behavioral capability probe exited with {status}"),
            ));
        }
        if let Some(marker) = &command.probe_marker {
            thread::sleep(PROBE_SETTLE_TIME);
            if marker.exists() {
                let _ = fs::remove_file(marker);
                return Err(NotAvailable::new(
                    NotAvailableReason::PrimitiveMissing,
                    "PID namespace supervisor did not tear down a setsid descendant",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCommand {
    program: PathBuf,
    args: Vec<OsString>,
    current_dir: PathBuf,
    environment: BTreeMap<OsString, OsString>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuralPolicy {
    program: PathBuf,
    args: Vec<OsString>,
}

impl StructuralPolicy {
    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn args(&self) -> &[OsString] {
        &self.args
    }
}

impl LocalCommand {
    pub fn new(program: impl Into<PathBuf>, current_dir: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: current_dir.into(),
            environment: BTreeMap::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedCommand {
    program: PathBuf,
    args: Vec<OsString>,
    current_dir: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    label: ExecutionLabel,
    resources: ResourceLimits,
    boundary_root: PathBuf,
    sandbox_paths: SandboxPaths,
    pinned_paths: Vec<PinnedPath>,
    probe_marker: Option<PathBuf>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct CompatibilityExecutionReport {
    pub state: ProcessState,
    pub output: ProcessOutput,
}

impl PreparedCommand {
    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }

    pub const fn label(&self) -> ExecutionLabel {
        self.label
    }

    /// Runs host compatibility work to completion without publishing it through the process API.
    pub fn run_compatibility_sync(
        self,
        owner: ProcessOwnership,
    ) -> Result<CompatibilityExecutionReport, LocalExecutionError> {
        if self.label != ExecutionLabel::HostCompatibility {
            return Err(LocalExecutionError::InvalidRequest(
                "synchronous compatibility execution requires a host-compatibility profile"
                    .to_owned(),
            ));
        }
        if matches!(owner, ProcessOwnership::Attempt(_)) {
            return Err(LocalExecutionError::InvalidRequest(
                "attempt-owned local execution requires cancellation coordination".to_owned(),
            ));
        }
        self.validate_launch()?;
        let (control, process_group) =
            ProcessGroupBoundary::start(&self.boundary_root, self.label)?;
        let mut command = self.command();
        configure_owned_process(&mut command, process_group);
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(self.resources.wall_time_millis))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "wall-time limit overflows clock",
                )
            })?;
        let record = control.record.clone();
        let prepared = PreparedCommandToken::issue_observed_registered(
            command,
            owner,
            control,
            move |boundary: &PersistedBoundary| persist_boundary(&record, boundary),
            |_, _| Ok(()),
            None,
            deadline,
            self.resources,
        )?;
        let mut process = spawn_owned(prepared, self.resources)?;
        let output = process.wait()?;
        Ok(CompatibilityExecutionReport {
            state: process.record().state(),
            output,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .current_dir(&self.current_dir)
            .env_clear()
            .envs(&self.environment)
            .stdin(Stdio::null());
        command
    }

    fn validate_launch(&self) -> Result<(), NotAvailable> {
        self.sandbox_paths.revalidate()?;
        for path in &self.pinned_paths {
            path.revalidate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PinnedPath {
    path: PathBuf,
    identity: PathIdentity,
}

impl PinnedPath {
    fn new(path: &Path) -> Result<Self, LocalExecutionError> {
        Ok(Self {
            path: path.to_owned(),
            identity: PathIdentity::read(path)?,
        })
    }

    fn revalidate(&self) -> Result<(), NotAvailable> {
        self.identity.revalidate(&self.path)
    }
}

fn probe_limits() -> ResourceLimits {
    ResourceLimits::new(1, 1, 1, 1, 1, 1, 4096, 5_000)
}

struct ProcessGroupBoundary {
    identity: BoundaryIdentity,
    leader: Option<Child>,
    process_group: i32,
    killed: bool,
    record: PathBuf,
}

impl ProcessGroupBoundary {
    fn start(
        boundary_root: &Path,
        label: ExecutionLabel,
    ) -> Result<(Self, i32), LocalExecutionError> {
        let token = random_token()?;
        let mut leader = Command::new("/bin/sleep");
        leader
            .arg("2147483647")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_owned_process(&mut leader, 0);
        let leader = leader.spawn()?;
        let process_group = i32::try_from(leader.id())
            .map_err(|_| io::Error::other("process-group leader PID does not fit i32"))?;
        let kind = if cfg!(target_os = "linux") {
            BoundaryKind::Container
        } else {
            BoundaryKind::MacOsProcessGroup
        };
        let record = boundary_root.join(format!(".kit-local-boundary-{token}"));
        let identity = BoundaryIdentity::new(
            kind,
            process_group.to_string(),
            token,
            format!("unreaped-leader:{process_group}:{}", label.label()),
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
        Ok((
            Self {
                identity,
                leader: Some(leader),
                process_group,
                killed: false,
                record,
            },
            process_group,
        ))
    }

    fn kill_group(&mut self) -> io::Result<()> {
        if self.killed {
            return Ok(());
        }
        let result = signal_process_group(self.process_group, SIGKILL);
        let error = io::Error::last_os_error();
        if result == 0 || error.raw_os_error() == Some(ESRCH) {
            self.killed = true;
            Ok(())
        } else {
            Err(error)
        }
    }
}

impl BoundaryControl for ProcessGroupBoundary {
    fn identity(&self) -> &BoundaryIdentity {
        &self.identity
    }

    fn containment(&self) -> Containment {
        if cfg!(target_os = "linux") {
            Containment::Complete
        } else {
            Containment::ProcessGroupOnly
        }
    }

    fn release(&mut self, _deadline: Instant) -> io::Result<()> {
        Ok(())
    }

    fn kill_boundary(&mut self, deadline: Instant) -> io::Result<()> {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "process-group kill deadline elapsed",
            ));
        }
        self.kill_group()
    }

    fn wait_and_reap(&mut self, deadline: Instant) -> io::Result<()> {
        self.kill_group()?;
        let Some(leader) = self.leader.as_mut() else {
            return Ok(());
        };
        loop {
            if leader.try_wait()?.is_some() {
                self.leader = None;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process-group leader could not be reaped",
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn inspect(&mut self, deadline: Instant) -> io::Result<Inspection> {
        loop {
            let result = signal_process_group(self.process_group, 0);
            let error = io::Error::last_os_error();
            if result != 0 && error.raw_os_error() == Some(ESRCH) {
                match fs::remove_file(&self.record) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                return Ok(Inspection {
                    identity: self.identity.clone(),
                    survivors: Some(0),
                    quiescent: true,
                });
            }
            if result != 0 {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Ok(Inspection {
                    identity: self.identity.clone(),
                    survivors: None,
                    quiescent: false,
                });
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for ProcessGroupBoundary {
    fn drop(&mut self) {
        let _ = self.kill_group();
        if let Some(leader) = self.leader.as_mut() {
            let deadline = Instant::now() + CLEANUP_TIMEOUT;
            while Instant::now() < deadline {
                match leader.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => thread::sleep(Duration::from_millis(2)),
                }
            }
        }
    }
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    let mut token = String::with_capacity(64);
    for byte in bytes {
        use fmt::Write as _;
        write!(token, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(token)
}

fn persist_boundary(path: &Path, boundary: &PersistedBoundary) -> io::Result<()> {
    let mut record = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    record.write_all(boundary.encode().as_bytes())?;
    record.sync_all()?;
    fs::File::open(path.parent().expect("boundary record has a parent"))?.sync_all()
}

fn boundary_root(paths: &SandboxPaths) -> PathBuf {
    paths
        .temp
        .parent()
        .unwrap_or(paths.temp.as_path())
        .to_owned()
}

fn canonical_program(program: &Path) -> Result<PathBuf, LocalExecutionError> {
    if !program.is_absolute() {
        return Err(LocalExecutionError::InvalidRequest(
            "program path must be absolute".to_owned(),
        ));
    }
    let program = fs::canonicalize(program)?;
    if !program.is_file() {
        return Err(LocalExecutionError::InvalidRequest(format!(
            "program {} is not a file",
            program.display()
        )));
    }
    Ok(program)
}

fn is_inside_workspace(path: &Path, paths: &SandboxPaths) -> bool {
    [paths.source(), paths.build(), paths.temp()]
        .into_iter()
        .any(|root| path.starts_with(root))
}

fn validate_environment(
    environment: &BTreeMap<OsString, OsString>,
) -> Result<(), LocalExecutionError> {
    const RESERVED: [&str; 9] = [
        "HOME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "SSH_AUTH_SOCK",
        "DOCKER_HOST",
        "XDG_RUNTIME_DIR",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
    ];
    for key in environment.keys() {
        let Some(key) = key.to_str() else {
            return Err(LocalExecutionError::InvalidRequest(
                "environment keys must be UTF-8".to_owned(),
            ));
        };
        if RESERVED.contains(&key)
            || key.starts_with("DYLD_")
            || key.starts_with("LD_")
            || key.starts_with("GIT_CONFIG")
        {
            return Err(LocalExecutionError::InvalidRequest(format!(
                "environment key {key} is reserved by the executor"
            )));
        }
    }
    Ok(())
}

fn scrubbed_environment(
    paths: &SandboxPaths,
    explicit: BTreeMap<OsString, OsString>,
    linux_namespace: bool,
) -> BTreeMap<OsString, OsString> {
    let (home, temp) = if linux_namespace {
        (OsString::from("/home/sandbox"), OsString::from("/tmp"))
    } else {
        (
            paths.temp.join("home").into_os_string(),
            paths.temp.clone().into_os_string(),
        )
    };
    let mut environment = BTreeMap::from([
        (OsString::from("HOME"), home),
        (OsString::from("TMPDIR"), temp),
        (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        (OsString::from("LANG"), OsString::from("C")),
        (OsString::from("LC_ALL"), OsString::from("C")),
        (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
        (
            OsString::from("GIT_CONFIG_GLOBAL"),
            OsString::from("/dev/null"),
        ),
        (OsString::from("GIT_CONFIG_COUNT"), OsString::from("1")),
        (
            OsString::from("GIT_CONFIG_KEY_0"),
            OsString::from("core.hooksPath"),
        ),
        (
            OsString::from("GIT_CONFIG_VALUE_0"),
            OsString::from("/dev/null"),
        ),
    ]);
    environment.extend(explicit);
    environment
}

fn seatbelt_args(paths: &SandboxPaths, program: &Path, args: &[OsString]) -> Vec<OsString> {
    let policy = format!(
        "(version 1)\n(deny default)\n(import \"system.sb\")\n(allow process*)\n(deny network*)\n(allow file-read* (literal {}) (subpath {}) (subpath {}) (subpath {}))\n(allow file-write* (subpath {}) (subpath {}))",
        seatbelt_string(program),
        seatbelt_string(paths.source()),
        seatbelt_string(paths.build()),
        seatbelt_string(paths.temp()),
        seatbelt_string(paths.build()),
        seatbelt_string(paths.temp()),
    );
    let mut sandbox_args = vec![
        OsString::from("-p"),
        OsString::from(policy),
        program.as_os_str().to_owned(),
    ];
    sandbox_args.extend_from_slice(args);
    sandbox_args
}

fn seatbelt_string(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn macos_probe_script(marker: &str) -> String {
    format!(
        r#"set -eu
/bin/ls "$KIT_PROBE_SOURCE" >/dev/null
if (: > "$KIT_PROBE_SOURCE/{marker}") 2>/dev/null; then
  /bin/rm -f "$KIT_PROBE_SOURCE/{marker}"
  exit 20
fi
: > "$KIT_PROBE_BUILD/{marker}"
test -f "$KIT_PROBE_BUILD/{marker}"
/bin/rm -f "$KIT_PROBE_BUILD/{marker}"
if /usr/bin/python3 -c 'import socket; socket.socket()' 2>/dev/null; then
  exit 21
fi"#
    )
}

fn linux_isolation_probe_script(marker: &str) -> String {
    format!(
        r#"set -eu
test -d /workspace
/bin/ls /workspace >/dev/null
if (: > /workspace/{marker}) 2>/dev/null; then
  /bin/rm -f /workspace/{marker}
  exit 20
fi
: > /build/.kit-probe-write
test -f /build/.kit-probe-write
/bin/rm -f /build/.kit-probe-write
while IFS=: read -r name counters; do
  test -z "$counters" && continue
  name=${{name##* }}
  test "$name" = lo || exit 21
done < /proc/net/dev
test -x /usr/bin/setsid
/usr/bin/setsid /bin/sh -c 'sleep 1; : > "$1"' probe /build/{marker} &
exit 0"#
    )
}

fn linux_compatibility_probe_script() -> &'static str {
    r#"set -eu
test -x /usr/bin/setsid
test -n "$KIT_PROBE_MARKER"
/usr/bin/setsid /bin/sh -c 'sleep 1; : > "$1"' probe "$KIT_PROBE_MARKER" &
exit 0"#
}

fn bubblewrap_args(
    paths: &SandboxPaths,
    host_program: &Path,
    program: &Path,
    current_dir: &Path,
    environment: &BTreeMap<OsString, OsString>,
    args: &[OsString],
) -> Vec<OsString> {
    let mut sandbox_args = [
        "--unshare-user",
        "--unshare-pid",
        "--unshare-net",
        "--unshare-ipc",
        "--unshare-uts",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/home",
        "--tmpfs",
        "/run",
        "--tmpfs",
        "/tmp",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    for directory in LINUX_SYSTEM_DIRS {
        sandbox_args.extend([
            OsString::from("--ro-bind-try"),
            OsString::from(directory),
            OsString::from(directory),
        ]);
    }
    if translate_linux_path(host_program, paths).is_none()
        && !LINUX_SYSTEM_DIRS
            .into_iter()
            .any(|directory| host_program.starts_with(directory))
    {
        sandbox_args.extend([
            OsString::from("--ro-bind"),
            host_program.as_os_str().to_owned(),
            host_program.as_os_str().to_owned(),
        ]);
    }
    sandbox_args.extend([
        OsString::from("--ro-bind"),
        paths.source.as_os_str().to_owned(),
        OsString::from("/workspace"),
        OsString::from("--bind"),
        paths.build.as_os_str().to_owned(),
        OsString::from("/build"),
        OsString::from("--bind"),
        paths.temp.as_os_str().to_owned(),
        OsString::from("/tmp"),
        OsString::from("--dir"),
        OsString::from("/home/sandbox"),
        OsString::from("--chdir"),
        current_dir.as_os_str().to_owned(),
    ]);
    for (key, value) in environment {
        sandbox_args.extend([OsString::from("--setenv"), key.clone(), value.clone()]);
    }
    sandbox_args.extend([OsString::from("--"), program.as_os_str().to_owned()]);
    sandbox_args.extend_from_slice(args);
    sandbox_args
}

fn bubblewrap_compatibility_args(
    program: &Path,
    current_dir: &Path,
    environment: &BTreeMap<OsString, OsString>,
    args: &[OsString],
) -> Vec<OsString> {
    let mut sandbox_args = [
        "--unshare-pid",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--bind",
        "/",
        "/",
        "--proc",
        "/proc",
        "--chdir",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    sandbox_args.push(current_dir.as_os_str().to_owned());
    for (key, value) in environment {
        sandbox_args.extend([OsString::from("--setenv"), key.clone(), value.clone()]);
    }
    sandbox_args.extend([OsString::from("--"), program.as_os_str().to_owned()]);
    sandbox_args.extend_from_slice(args);
    sandbox_args
}

fn linux_path(
    program: &Path,
    current_dir: &Path,
    paths: &SandboxPaths,
) -> Result<(PathBuf, PathBuf), LocalExecutionError> {
    Ok((
        translate_linux_path(program, paths).unwrap_or_else(|| program.to_owned()),
        translate_linux_path(current_dir, paths).ok_or_else(|| {
            LocalExecutionError::InvalidRequest(
                "working directory cannot be represented in the sandbox".to_owned(),
            )
        })?,
    ))
}

fn translate_linux_path(path: &Path, paths: &SandboxPaths) -> Option<PathBuf> {
    for (host, sandbox) in [
        (paths.source(), Path::new("/workspace")),
        (paths.build(), Path::new("/build")),
        (paths.temp(), Path::new("/tmp")),
    ] {
        if let Ok(relative) = path.strip_prefix(host) {
            return Some(sandbox.join(relative));
        }
    }
    None
}

#[cfg(unix)]
fn configure_owned_process(command: &mut Command, process_group: i32) {
    use std::os::unix::process::CommandExt;

    // SAFETY: only async-signal-safe syscalls run between fork and exec. Descriptor custody relies
    // on CLOEXEC; closing arbitrary descriptors here would also close Rust's spawn error channel.
    unsafe {
        command.pre_exec(move || {
            if setpgid(0, process_group) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_owned_process(_command: &mut Command, _process_group: i32) {}

#[cfg(unix)]
unsafe extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

#[cfg(unix)]
fn signal_process_group(process_group: i32, signal: i32) -> i32 {
    unsafe { kill(-process_group, signal) }
}

#[cfg(not(unix))]
fn signal_process_group(_process_group: i32, _signal: i32) -> i32 {
    -1
}

#[cfg(unix)]
const SIGKILL: i32 = 9;
#[cfg(unix)]
const ESRCH: i32 = 3;

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn pre_exec_failure_is_reported_by_spawn() {
        let mut command = Command::new("/usr/bin/true");
        configure_owned_process(&mut command, -1);
        let error = command.spawn().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    }
}
