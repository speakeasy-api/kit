use std::{
    fmt, fs, io,
    path::{Component, Path, PathBuf},
};

use crate::{
    executor::profile::{
        ExecutionLabel, ExecutorProfile, Mount, MountAccess, MountRole, Platform, SourceWriteMode,
    },
    workspace::acquire::{AcquisitionResult, GitMetadata},
};

#[derive(Debug)]
pub enum MountError {
    InvalidProfile(&'static str),
    SharedGitMetadata,
    InvalidPath {
        kind: &'static str,
        path: PathBuf,
        reason: &'static str,
    },
    IdentityChanged(&'static str),
    IdentityUnavailable(io::Error),
}

impl fmt::Display for MountError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProfile(reason) => {
                write!(formatter, "invalid container profile: {reason}")
            }
            Self::SharedGitMetadata => formatter
                .write_str("shared Git metadata cannot be mounted into a restricted container"),
            Self::InvalidPath { kind, path, reason } => {
                write!(
                    formatter,
                    "invalid {kind} path {}: {reason}",
                    path.display()
                )
            }
            Self::IdentityChanged(kind) => {
                write!(
                    formatter,
                    "validated {kind} directory identity changed before launch"
                )
            }
            Self::IdentityUnavailable(error) => {
                write!(formatter, "cannot revalidate mount identity: {error}")
            }
        }
    }
}

impl std::error::Error for MountError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MountIdentity {
    device: u64,
    inode: u64,
    change_seconds: i64,
    change_nanos: i64,
}

impl MountIdentity {
    pub(crate) fn protocol_value(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.device, self.inode, self.change_seconds, self.change_nanos
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ValidatedMounts {
    pub(crate) source: PathBuf,
    pub(crate) overlay: Option<PathBuf>,
    pub(crate) build: PathBuf,
    pub(crate) temp: PathBuf,
    pub(crate) source_target: PathBuf,
    pub(crate) base_source_target: Option<PathBuf>,
    pub(crate) build_target: PathBuf,
    pub(crate) temp_target: PathBuf,
    source_identity: MountIdentity,
    overlay_identity: Option<MountIdentity>,
    build_identity: MountIdentity,
    temp_identity: MountIdentity,
}

impl ValidatedMounts {
    pub(crate) fn acquire(
        profile: &ExecutorProfile,
        workspace: &AcquisitionResult,
        build: &Path,
        temp: &Path,
    ) -> Result<Self, MountError> {
        validate_profile(profile)?;
        if workspace.git_metadata != GitMetadata::Independent {
            return Err(MountError::SharedGitMetadata);
        }

        let source = canonical_directory(&workspace.path, "source")?;
        if source != workspace.path {
            return Err(invalid_path(
                "source",
                &workspace.path,
                "path is no longer canonical",
            ));
        }
        validate_source_tree(&source)?;
        let writable_parent = source
            .parent()
            .expect("validated workspace repositories have an allocation parent");
        let build = canonical_writable_directory(build, "build", writable_parent)?;
        let temp = canonical_writable_directory(temp, "temp", writable_parent)?;
        reject_overlap(&source, &build, "build")?;
        reject_overlap(&source, &temp, "temp")?;
        reject_overlap(&build, &temp, "temp")?;

        Ok(Self {
            source_identity: identity(&source).map_err(MountError::IdentityUnavailable)?,
            overlay_identity: None,
            build_identity: identity(&build).map_err(MountError::IdentityUnavailable)?,
            temp_identity: identity(&temp).map_err(MountError::IdentityUnavailable)?,
            source,
            overlay: None,
            build,
            temp,
            source_target: mount(profile, MountRole::Source).target.clone(),
            base_source_target: None,
            build_target: mount(profile, MountRole::Build).target.clone(),
            temp_target: mount(profile, MountRole::Temp).target.clone(),
        })
    }

    pub(crate) fn acquire_overlay(
        profile: &ExecutorProfile,
        source: &Path,
        overlay: &Path,
        build: &Path,
        temp: &Path,
    ) -> Result<Self, MountError> {
        validate_profile(profile)?;
        if profile.source_write() != SourceWriteMode::MutationOverlay {
            return Err(MountError::InvalidProfile(
                "source mutation overlay is not enabled",
            ));
        }
        let requested_source = source;
        let source = canonical_directory(requested_source, "source")?;
        if source != requested_source {
            return Err(invalid_path(
                "source",
                &source,
                "path is no longer canonical",
            ));
        }
        validate_source_tree(&source)?;
        let allocation = source
            .parent()
            .ok_or_else(|| invalid_path("source", &source, "source has no allocation parent"))?;
        let overlay = canonical_writable_directory(overlay, "overlay", allocation)?;
        validate_source_tree(&overlay)?;
        let build = canonical_writable_directory(build, "build", allocation)?;
        let temp = canonical_writable_directory(temp, "temp", allocation)?;
        for (left, right, kind) in [
            (&source, &overlay, "overlay"),
            (&source, &build, "build"),
            (&source, &temp, "temp"),
            (&overlay, &build, "build"),
            (&overlay, &temp, "temp"),
            (&build, &temp, "temp"),
        ] {
            reject_overlap(left, right, kind)?;
        }
        Ok(Self {
            source_identity: identity(&source).map_err(MountError::IdentityUnavailable)?,
            overlay_identity: Some(identity(&overlay).map_err(MountError::IdentityUnavailable)?),
            build_identity: identity(&build).map_err(MountError::IdentityUnavailable)?,
            temp_identity: identity(&temp).map_err(MountError::IdentityUnavailable)?,
            source,
            overlay: Some(overlay),
            build,
            temp,
            source_target: mount(profile, MountRole::Source).target.clone(),
            base_source_target: Some(PathBuf::from("/kit-stage-source")),
            build_target: mount(profile, MountRole::Build).target.clone(),
            temp_target: mount(profile, MountRole::Temp).target.clone(),
        })
    }

    pub(crate) fn acquire_immutable(
        profile: &ExecutorProfile,
        source: &Path,
        build: &Path,
        temp: &Path,
    ) -> Result<Self, MountError> {
        validate_profile(profile)?;
        if profile.source_write() != SourceWriteMode::ReadOnly {
            return Err(MountError::InvalidProfile("source is not read-only"));
        }
        let requested_source = source;
        let source = canonical_directory(requested_source, "source")?;
        if source != requested_source {
            return Err(invalid_path(
                "source",
                &source,
                "path is no longer canonical",
            ));
        }
        validate_source_tree(&source)?;
        let allocation = source
            .parent()
            .ok_or_else(|| invalid_path("source", &source, "source has no allocation parent"))?;
        let build = canonical_writable_directory(build, "build", allocation)?;
        let temp = canonical_writable_directory(temp, "temp", allocation)?;
        reject_overlap(&source, &build, "build")?;
        reject_overlap(&source, &temp, "temp")?;
        reject_overlap(&build, &temp, "temp")?;
        Ok(Self {
            source_identity: identity(&source).map_err(MountError::IdentityUnavailable)?,
            overlay_identity: None,
            build_identity: identity(&build).map_err(MountError::IdentityUnavailable)?,
            temp_identity: identity(&temp).map_err(MountError::IdentityUnavailable)?,
            source,
            overlay: None,
            build,
            temp,
            source_target: mount(profile, MountRole::Source).target.clone(),
            base_source_target: None,
            build_target: mount(profile, MountRole::Build).target.clone(),
            temp_target: mount(profile, MountRole::Temp).target.clone(),
        })
    }

    /// The trusted helper repeats this check and opens lease-pinned objects before constructing
    /// runtime mounts. This local check catches changes before the helper is started.
    pub(crate) fn revalidate(&self) -> Result<(), MountError> {
        for (kind, path, expected) in [
            ("source", &self.source, &self.source_identity),
            ("build", &self.build, &self.build_identity),
            ("temp", &self.temp, &self.temp_identity),
        ] {
            let actual = identity(path).map_err(MountError::IdentityUnavailable)?;
            if &actual != expected {
                return Err(MountError::IdentityChanged(kind));
            }
        }
        if let (Some(path), Some(expected)) = (&self.overlay, &self.overlay_identity) {
            let actual = identity(path).map_err(MountError::IdentityUnavailable)?;
            if &actual != expected {
                return Err(MountError::IdentityChanged("overlay"));
            }
        }
        Ok(())
    }

    pub(crate) fn source_identity(&self) -> String {
        self.source_identity.protocol_value()
    }

    pub(crate) fn build_identity(&self) -> String {
        self.build_identity.protocol_value()
    }

    pub(crate) fn temp_identity(&self) -> String {
        self.temp_identity.protocol_value()
    }

    pub(crate) fn overlay_identity(&self) -> Option<String> {
        self.overlay_identity
            .as_ref()
            .map(MountIdentity::protocol_value)
    }
}

fn validate_profile(profile: &ExecutorProfile) -> Result<(), MountError> {
    if profile.label() != ExecutionLabel::Restricted {
        return Err(MountError::InvalidProfile("profile is not restricted"));
    }
    if profile.platform() != Platform::Linux {
        return Err(MountError::InvalidProfile("profile is not Linux"));
    }
    if !matches!(
        profile.source_write(),
        SourceWriteMode::ReadOnly | SourceWriteMode::MutationOverlay
    ) {
        return Err(MountError::InvalidProfile("source is not read-only"));
    }
    if profile
        .mounts()
        .iter()
        .any(|mount| mount.target.to_string_lossy().contains(','))
    {
        return Err(MountError::InvalidProfile(
            "mount target contains a runtime mount delimiter",
        ));
    }
    let source_access = match profile.source_write() {
        SourceWriteMode::ReadOnly => MountAccess::ReadOnly,
        SourceWriteMode::MutationOverlay => MountAccess::CopyOnWrite,
        SourceWriteMode::Direct => unreachable!(),
    };
    if mount(profile, MountRole::Root).access != MountAccess::ReadOnly
        || mount(profile, MountRole::Source).access != source_access
        || mount(profile, MountRole::Build).access != MountAccess::ReadWrite
        || mount(profile, MountRole::Temp).access != MountAccess::ReadWrite
    {
        return Err(MountError::InvalidProfile("unsupported mount access"));
    }
    Ok(())
}

fn mount(profile: &ExecutorProfile, role: MountRole) -> &Mount {
    profile
        .mounts()
        .iter()
        .find(|mount| mount.role == role)
        .expect("validated executor profiles contain every mount role")
}

fn canonical_directory(path: &Path, kind: &'static str) -> Result<PathBuf, MountError> {
    let canonical = fs::canonicalize(path)
        .map_err(|_| invalid_path(kind, path, "cannot canonicalize directory"))?;
    if !fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(invalid_path(kind, path, "not a directory"));
    }
    Ok(canonical)
}

fn canonical_writable_directory(
    path: &Path,
    kind: &'static str,
    allocation: &Path,
) -> Result<PathBuf, MountError> {
    let canonical = canonical_directory(path, kind)?;
    if canonical != path {
        return Err(invalid_path(
            kind,
            path,
            "path must be absolute, canonical, and symlink-free",
        ));
    }
    if canonical.parent() != Some(allocation) {
        return Err(invalid_path(
            kind,
            path,
            "directory is outside the workspace allocation",
        ));
    }
    reject_mount_delimiters(kind, &canonical)?;
    Ok(canonical)
}

fn reject_overlap(left: &Path, right: &Path, right_kind: &'static str) -> Result<(), MountError> {
    if left.starts_with(right) || right.starts_with(left) {
        return Err(invalid_path(
            right_kind,
            right,
            "mount path overlaps another boundary",
        ));
    }
    Ok(())
}

fn validate_source_tree(root: &Path) -> Result<(), MountError> {
    reject_mount_delimiters("source", root)?;
    let root_metadata =
        fs::metadata(root).map_err(|_| invalid_path("source", root, "cannot inspect source"))?;
    let mut directories = vec![root.to_owned()];
    while let Some(directory) = directories.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|_| invalid_path("source", &directory, "cannot inspect source entry"))?;
        for entry in entries {
            let entry = entry
                .map_err(|_| invalid_path("source", &directory, "cannot inspect source entry"))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| invalid_path("source", &path, "cannot inspect source entry"))?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                validate_symlink(root, &path)?;
                continue;
            }
            if !(file_type.is_dir() || file_type.is_file()) {
                return Err(invalid_path(
                    "source",
                    &path,
                    "socket, device, or special file",
                ));
            }
            validate_unix_entry(&path, &root_metadata, &metadata)?;
            if file_type.is_dir() {
                directories.push(path);
            }
        }
    }
    Ok(())
}

fn validate_symlink(root: &Path, path: &Path) -> Result<(), MountError> {
    let target =
        fs::read_link(path).map_err(|_| invalid_path("source", path, "cannot read symlink"))?;
    if target.is_absolute() {
        return Err(invalid_path("source", path, "absolute symlink target"));
    }
    let parent = path.parent().expect("source entries have a parent");
    let relative_parent = parent
        .strip_prefix(root)
        .expect("walked source entry remains below source");
    let mut depth = relative_parent.components().count();
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(invalid_path(
                    "source",
                    path,
                    "symlink target escapes source",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_unix_entry(
    path: &Path,
    root_metadata: &fs::Metadata,
    metadata: &fs::Metadata,
) -> Result<(), MountError> {
    use std::os::unix::fs::MetadataExt;

    if metadata.dev() != root_metadata.dev() {
        return Err(invalid_path("source", path, "nested filesystem mount"));
    }
    if metadata.is_file() && metadata.nlink() != 1 {
        return Err(invalid_path("source", path, "hard-linked file"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_unix_entry(
    _path: &Path,
    _root_metadata: &fs::Metadata,
    _metadata: &fs::Metadata,
) -> Result<(), MountError> {
    Ok(())
}

#[cfg(unix)]
fn identity(path: &Path) -> io::Result<MountIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path)?;
    Ok(MountIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        change_seconds: metadata.ctime(),
        change_nanos: metadata.ctime_nsec(),
    })
}

#[cfg(not(unix))]
fn identity(_path: &Path) -> io::Result<MountIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "stable mount identities require Unix",
    ))
}

fn reject_mount_delimiters(kind: &'static str, path: &Path) -> Result<(), MountError> {
    if path.to_string_lossy().contains(',') {
        return Err(invalid_path(
            kind,
            path,
            "comma cannot be represented safely in a runtime mount",
        ));
    }
    Ok(())
}

fn invalid_path(kind: &'static str, path: &Path, reason: &'static str) -> MountError {
    MountError::InvalidPath {
        kind,
        path: path.to_owned(),
        reason,
    }
}
