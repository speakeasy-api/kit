use std::{fmt, io, path::PathBuf};

use super::{
    edit::ir::{FilesystemIdentityPolicy, RootRelativePath},
    revision::{EpochId, RevisionError, RevisionId},
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use unix::*;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod unavailable;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub use unavailable::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Authority {
    ExistingRead,
    ReplaceSource,
    DeleteSource,
    CreateParent,
    MoveSource,
    MoveDestination,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathAuthLimit {
    Entries,
    NameBytes,
    Time,
    Memory,
    Content,
    ReadBytes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryType {
    Directory,
    RegularFile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
    entry_type: EntryType,
    mode: u32,
}

impl FileIdentity {
    pub fn device(&self) -> u64 {
        self.device
    }

    pub fn inode(&self) -> u64 {
        self.inode
    }

    pub fn entry_type(&self) -> EntryType {
        self.entry_type
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityBinding {
    root: FileIdentity,
    revision: RevisionId,
    epoch: EpochId,
    path: RootRelativePath,
    path_identity: String,
    object: Option<FileIdentity>,
    authority: Authority,
}

impl CapabilityBinding {
    pub fn root_identity(&self) -> FileIdentity {
        self.root
    }

    pub fn revision(&self) -> RevisionId {
        self.revision
    }

    pub fn epoch(&self) -> EpochId {
        self.epoch
    }

    pub fn path(&self) -> &RootRelativePath {
        &self.path
    }

    pub fn path_identity(&self) -> &str {
        &self.path_identity
    }

    pub fn object_identity(&self) -> Option<FileIdentity> {
        self.object
    }

    pub fn authority(&self) -> Authority {
        self.authority
    }
}

#[derive(Debug)]
pub enum PathAuthError {
    InvalidPath(PathBuf),
    PrivatePath(PathBuf),
    Alias(PathBuf),
    Symlink(PathBuf),
    NotFound(PathBuf),
    AlreadyExists(PathBuf),
    NotDirectory(PathBuf),
    NotFile(PathBuf),
    Hardlink(PathBuf),
    SpecialFile(PathBuf),
    MountBoundary(PathBuf),
    ObjectChanged(PathBuf),
    LimitExceeded(PathAuthLimit),
    StaleEpoch {
        expected: EpochId,
        current: EpochId,
    },
    WrongAuthority {
        expected: Authority,
        actual: Authority,
    },
    CrossGuard,
    CrossRoot,
    Revision(RevisionError),
    Unavailable {
        reason: &'static str,
    },
    Io {
        operation: &'static str,
        source: io::Error,
    },
}

impl fmt::Display for PathAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(path) => {
                write!(formatter, "invalid workspace path: {}", path.display())
            }
            Self::PrivatePath(path) => {
                write!(formatter, "private workspace path: {}", path.display())
            }
            Self::Alias(path) => {
                write!(formatter, "case or normalization alias: {}", path.display())
            }
            Self::Symlink(path) => write!(formatter, "symlink path component: {}", path.display()),
            Self::NotFound(path) => {
                write!(formatter, "workspace path not found: {}", path.display())
            }
            Self::AlreadyExists(path) => write!(
                formatter,
                "workspace path already exists: {}",
                path.display()
            ),
            Self::NotDirectory(path) => write!(
                formatter,
                "workspace path is not a directory: {}",
                path.display()
            ),
            Self::NotFile(path) => write!(
                formatter,
                "workspace path is not a regular file: {}",
                path.display()
            ),
            Self::Hardlink(path) => {
                write!(formatter, "hardlinked workspace file: {}", path.display())
            }
            Self::SpecialFile(path) => {
                write!(formatter, "special workspace entry: {}", path.display())
            }
            Self::MountBoundary(path) => {
                write!(formatter, "workspace mount boundary: {}", path.display())
            }
            Self::ObjectChanged(path) => write!(
                formatter,
                "authorized workspace object changed: {}",
                path.display()
            ),
            Self::LimitExceeded(kind) => {
                write!(
                    formatter,
                    "workspace path authorization exceeded {kind:?} limit"
                )
            }
            Self::StaleEpoch { expected, current } => write!(
                formatter,
                "stale workspace epoch {expected}; current epoch is {current}"
            ),
            Self::WrongAuthority { expected, actual } => write!(
                formatter,
                "capability authority {actual:?} cannot perform {expected:?}"
            ),
            Self::CrossGuard => {
                formatter.write_str("capability belongs to a different mutation guard")
            }
            Self::CrossRoot => {
                formatter.write_str("capability belongs to a different workspace root or revision")
            }
            Self::Revision(error) => error.fmt(formatter),
            Self::Unavailable { reason } => write!(
                formatter,
                "workspace path authorization unavailable: {reason}"
            ),
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
        }
    }
}

impl std::error::Error for PathAuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Revision(source) => Some(source),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<RevisionError> for PathAuthError {
    fn from(error: RevisionError) -> Self {
        Self::Revision(error)
    }
}

pub(crate) fn binding(
    root: FileIdentity,
    revision: RevisionId,
    epoch: EpochId,
    path: RootRelativePath,
    path_identity: String,
    object: Option<FileIdentity>,
    authority: Authority,
) -> CapabilityBinding {
    CapabilityBinding {
        root,
        revision,
        epoch,
        path,
        path_identity,
        object,
        authority,
    }
}

pub(crate) fn file_identity(
    device: u64,
    inode: u64,
    entry_type: EntryType,
    mode: u32,
) -> FileIdentity {
    FileIdentity {
        device,
        inode,
        entry_type,
        mode,
    }
}

pub(crate) fn policy_identity(path: &RootRelativePath, policy: FilesystemIdentityPolicy) -> String {
    super::edit::ir::identity_key(path, policy)
}
