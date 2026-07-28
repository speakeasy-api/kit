use std::{
    ffi::{CStr, CString, OsStr},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    marker::PhantomData,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::Instant,
};

use unicode_normalization::UnicodeNormalization;

use super::{
    Authority, CapabilityBinding, EntryType, FileIdentity, PathAuthError, PathAuthLimit, binding,
    file_identity, policy_identity,
};
use crate::workspace::{
    edit::ir::{EditLimits, FilesystemIdentityPolicy, RootRelativePath},
    revision::{
        EpochId, LimitKind, MutationGuardNonce, RevisionError, RevisionId,
        WorkspaceKernelMutationFence, WorkspaceMutationGuard,
    },
};

mod sys;

use sys::{
    MODE_DIRECTORY, MODE_REGULAR, MODE_SYMLINK, MountIdentity, Stat, open_directory_component,
    open_file_component, stat_at, stat_file,
};

pub struct PathAuthorizer<'guard, 'workspace> {
    guard: &'guard mut WorkspaceMutationGuard<'workspace>,
    root: File,
    root_path: PathBuf,
    root_stat: Stat,
    root_mount: MountIdentity,
    revision: RevisionId,
    epoch: EpochId,
    limits: EditLimits,
    nonce: MutationGuardNonce,
    deadline: Instant,
}

pub(crate) struct AcceptedPathCapability {
    _nonce: MutationGuardNonce,
    _capability: AcceptedCapability,
}

impl AcceptedPathCapability {
    pub(crate) fn source_binding(&self) -> Option<&CapabilityBinding> {
        match &self._capability {
            AcceptedCapability::Replace { _capability }
            | AcceptedCapability::Delete { _capability } => Some(&_capability.binding),
            AcceptedCapability::Move { _source, .. } => Some(&_source.binding),
            AcceptedCapability::Create { .. } => None,
        }
    }
}

#[allow(clippy::large_enum_variant)]
enum AcceptedCapability {
    Replace {
        _capability: ExistingCapability,
    },
    Delete {
        _capability: ExistingCapability,
    },
    Create {
        _capability: DestinationCapability,
    },
    Move {
        _source: ExistingCapability,
        _destination: DestinationCapability,
    },
}

struct ExistingCapability {
    file: File,
    parent: File,
    parent_stat: Stat,
    leaf: CString,
    stat: Stat,
    binding: CapabilityBinding,
    fence: WorkspaceKernelMutationFence,
}

struct DestinationCapability {
    parent: File,
    parent_stat: Stat,
    leaf: CString,
    binding: CapabilityBinding,
    fence: WorkspaceKernelMutationFence,
}

#[derive(Clone, Copy)]
struct GuardBinding<'guard, 'workspace> {
    nonce: MutationGuardNonce,
    _invariant: PhantomData<
        fn(
            &'guard mut WorkspaceMutationGuard<'workspace>,
        ) -> &'guard mut WorkspaceMutationGuard<'workspace>,
    >,
}

pub struct ExistingRead<'guard, 'workspace> {
    capability: ExistingCapability,
    guard: GuardBinding<'guard, 'workspace>,
}
pub struct ReplaceSource<'guard, 'workspace> {
    capability: ExistingCapability,
    _guard: GuardBinding<'guard, 'workspace>,
}
pub struct DeleteSource<'guard, 'workspace> {
    capability: ExistingCapability,
    _guard: GuardBinding<'guard, 'workspace>,
}
pub struct CreateParent<'guard, 'workspace> {
    capability: DestinationCapability,
    _guard: GuardBinding<'guard, 'workspace>,
}
pub struct MoveSource<'guard, 'workspace> {
    capability: ExistingCapability,
    _guard: GuardBinding<'guard, 'workspace>,
}
pub struct MoveDestination<'guard, 'workspace> {
    capability: DestinationCapability,
    _guard: GuardBinding<'guard, 'workspace>,
}

impl<'guard, 'workspace> PathAuthorizer<'guard, 'workspace> {
    pub fn new(
        guard: &'guard mut WorkspaceMutationGuard<'workspace>,
        revision: RevisionId,
        epoch: EpochId,
        limits: EditLimits,
    ) -> Result<Self, PathAuthError> {
        let deadline = authorization_deadline(limits);
        Self::new_before(guard, revision, epoch, limits, deadline)
    }

    pub(crate) fn new_before(
        guard: &'guard mut WorkspaceMutationGuard<'workspace>,
        revision: RevisionId,
        epoch: EpochId,
        limits: EditLimits,
        operation_deadline: Instant,
    ) -> Result<Self, PathAuthError> {
        validate_limits(limits)?;
        if limits.identity_policy == FilesystemIdentityPolicy::CaseSensitive {
            return Err(PathAuthError::Unavailable {
                reason: "case-sensitive identity requires a safe filesystem semantics probe",
            });
        }
        let deadline = operation_deadline.min(authorization_deadline(limits));
        let current = guard.validate_held_revision_until(revision, deadline)?;
        if current.epoch() != epoch {
            return Err(PathAuthError::StaleEpoch {
                expected: epoch,
                current: current.epoch(),
            });
        }
        let (root, root_path) = guard.path_authorization_root()?;
        sys::ensure_local_filesystem(&root).map_err(|_| PathAuthError::Unavailable {
            reason: "path authorization requires a supported local filesystem",
        })?;
        let root_stat =
            stat_file(&root).map_err(|source| io_error("inspect workspace root", source))?;
        if root_stat.kind() != MODE_DIRECTORY {
            return Err(PathAuthError::NotDirectory(root_path));
        }
        let root_mount = mount_identity(&root)?;
        let nonce = guard.path_authorization_nonce();
        let authorizer = Self {
            guard,
            root,
            root_path,
            root_stat,
            root_mount,
            revision,
            epoch,
            limits,
            nonce,
            deadline,
        };
        authorizer.verify_named_root()?;
        Ok(authorizer)
    }

    pub fn authorize_read(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<ExistingRead<'guard, 'workspace>, PathAuthError> {
        self.authorize_read_inner(path.as_ref(), &mut |_, _| {})
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn authorize_read_with_hook(
        &mut self,
        path: impl AsRef<Path>,
        mut hook: impl FnMut(&str, &Path),
    ) -> Result<ExistingRead<'guard, 'workspace>, PathAuthError> {
        self.authorize_read_inner(path.as_ref(), &mut hook)
    }

    fn authorize_read_inner(
        &mut self,
        path: &Path,
        hook: &mut dyn FnMut(&str, &Path),
    ) -> Result<ExistingRead<'guard, 'workspace>, PathAuthError> {
        let (mut budget, fence) = self.begin_authorization()?;
        let path = self.parse_path(path.as_ref())?;
        let mut capability =
            self.resolve_existing(&path, Authority::ExistingRead, &mut budget, fence, hook)?;
        self.finish_existing(&mut capability, budget.deadline)?;
        Ok(ExistingRead {
            capability,
            guard: self.guard_binding(),
        })
    }

    pub fn authorize_replace(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<ReplaceSource<'guard, 'workspace>, PathAuthError> {
        let (mut budget, fence) = self.begin_authorization()?;
        let path = self.parse_path(path.as_ref())?;
        let mut capability = self.resolve_existing(
            &path,
            Authority::ReplaceSource,
            &mut budget,
            fence,
            &mut |_, _| {},
        )?;
        self.finish_existing(&mut capability, budget.deadline)?;
        Ok(ReplaceSource {
            capability,
            _guard: self.guard_binding(),
        })
    }

    pub fn authorize_delete(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<DeleteSource<'guard, 'workspace>, PathAuthError> {
        let (mut budget, fence) = self.begin_authorization()?;
        let path = self.parse_path(path.as_ref())?;
        let mut capability = self.resolve_existing(
            &path,
            Authority::DeleteSource,
            &mut budget,
            fence,
            &mut |_, _| {},
        )?;
        self.finish_existing(&mut capability, budget.deadline)?;
        Ok(DeleteSource {
            capability,
            _guard: self.guard_binding(),
        })
    }

    pub fn authorize_create(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Result<CreateParent<'guard, 'workspace>, PathAuthError> {
        let (mut budget, fence) = self.begin_authorization()?;
        let path = self.parse_path(path.as_ref())?;
        let mut capability = self.resolve_destination(
            &path,
            Authority::CreateParent,
            &mut budget,
            fence,
            &mut |_, _| {},
        )?;
        self.finish_destination(&mut capability, budget.deadline)?;
        Ok(CreateParent {
            capability,
            _guard: self.guard_binding(),
        })
    }

    pub fn authorize_move(
        &mut self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
    ) -> Result<
        (
            MoveSource<'guard, 'workspace>,
            MoveDestination<'guard, 'workspace>,
        ),
        PathAuthError,
    > {
        let (mut budget, source_fence) = self.begin_authorization()?;
        let from = self.parse_path(from.as_ref())?;
        let to = self.parse_path(to.as_ref())?;
        if policy_identity(&from, self.limits.identity_policy)
            == policy_identity(&to, self.limits.identity_policy)
        {
            return Err(PathAuthError::Alias(PathBuf::from(to.as_str())));
        }
        let mut source = self.resolve_existing(
            &from,
            Authority::MoveSource,
            &mut budget,
            source_fence,
            &mut |_, _| {},
        )?;
        self.finish_existing(&mut source, budget.deadline)?;
        let (mut budget, destination_fence) = self.begin_authorization()?;
        let mut destination = self.resolve_destination(
            &to,
            Authority::MoveDestination,
            &mut budget,
            destination_fence,
            &mut |_, _| {},
        )?;
        self.finish_destination(&mut destination, budget.deadline)?;
        let guard = self.guard_binding();
        Ok((
            MoveSource {
                capability: source,
                _guard: guard,
            },
            MoveDestination {
                capability: destination,
                _guard: guard,
            },
        ))
    }

    pub fn read(
        &mut self,
        mut capability: ExistingRead<'guard, 'workspace>,
        max_bytes: usize,
        max_memory_bytes: usize,
        request_deadline: Instant,
    ) -> Result<Vec<u8>, PathAuthError> {
        if capability.guard.nonce != self.nonce {
            return Err(PathAuthError::CrossGuard);
        }
        self.read_existing(
            &mut capability.capability,
            Authority::ExistingRead,
            max_bytes,
            max_memory_bytes,
            request_deadline,
        )
    }

    pub(crate) fn read_replace(
        &mut self,
        capability: &mut ReplaceSource<'guard, 'workspace>,
        max_bytes: usize,
        max_memory_bytes: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>, PathAuthError> {
        self.require_guard(capability._guard)?;
        self.read_existing(
            &mut capability.capability,
            Authority::ReplaceSource,
            max_bytes,
            max_memory_bytes,
            deadline,
        )
    }

    pub(crate) fn read_delete(
        &mut self,
        capability: &mut DeleteSource<'guard, 'workspace>,
        max_bytes: usize,
        max_memory_bytes: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>, PathAuthError> {
        self.require_guard(capability._guard)?;
        self.read_existing(
            &mut capability.capability,
            Authority::DeleteSource,
            max_bytes,
            max_memory_bytes,
            deadline,
        )
    }

    pub(crate) fn read_move(
        &mut self,
        capability: &mut MoveSource<'guard, 'workspace>,
        max_bytes: usize,
        max_memory_bytes: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>, PathAuthError> {
        self.require_guard(capability._guard)?;
        self.read_existing(
            &mut capability.capability,
            Authority::MoveSource,
            max_bytes,
            max_memory_bytes,
            deadline,
        )
    }

    fn read_existing(
        &mut self,
        capability: &mut ExistingCapability,
        authority: Authority,
        max_bytes: usize,
        max_memory_bytes: usize,
        request_deadline: Instant,
    ) -> Result<Vec<u8>, PathAuthError> {
        if max_bytes == 0 || max_memory_bytes == 0 {
            return Err(PathAuthError::LimitExceeded(PathAuthLimit::Memory));
        }
        let deadline = self.deadline.min(request_deadline);
        capability.fence.ensure_clean().map_err(map_fence_error)?;
        self.refresh_until(deadline)?;
        capability.fence.ensure_clean().map_err(map_fence_error)?;
        self.verify_existing(capability, authority)?;
        let size = usize::try_from(capability.stat.size())
            .map_err(|_| PathAuthError::LimitExceeded(PathAuthLimit::Content))?;
        if size > max_bytes {
            return Err(PathAuthError::LimitExceeded(PathAuthLimit::ReadBytes));
        }
        if size > max_memory_bytes {
            return Err(PathAuthError::LimitExceeded(PathAuthLimit::Memory));
        }
        if size > self.limits.max_content_bytes {
            return Err(PathAuthError::LimitExceeded(PathAuthLimit::Content));
        }
        if size > self.limits.max_authorization_memory_bytes {
            return Err(PathAuthError::LimitExceeded(PathAuthLimit::Memory));
        }
        capability
            .file
            .seek(SeekFrom::Start(0))
            .map_err(|source| io_error("seek authorized file", source))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| PathAuthError::LimitExceeded(PathAuthLimit::Memory))?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            check_deadline(deadline)?;
            let count = capability
                .file
                .read(&mut buffer)
                .map_err(|source| io_error("read authorized file", source))?;
            check_deadline(deadline)?;
            if count == 0 {
                break;
            }
            if bytes.len().saturating_add(count) > size {
                return Err(PathAuthError::ObjectChanged(path_of(&capability.binding)));
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        let after = stat_file(&capability.file)
            .map_err(|source| io_error("reinspect authorized file", source))?;
        check_deadline(deadline)?;
        if bytes.len() != size || !capability.stat.same_bound(after) {
            return Err(PathAuthError::ObjectChanged(path_of(&capability.binding)));
        }
        capability.fence.ensure_clean().map_err(map_fence_error)?;
        self.refresh_until(deadline)?;
        capability.fence.ensure_clean().map_err(map_fence_error)?;
        check_deadline(deadline)?;
        Ok(bytes)
    }

    pub(crate) fn accept_replace(
        &mut self,
        capability: ReplaceSource<'guard, 'workspace>,
    ) -> Result<AcceptedPathCapability, PathAuthError> {
        self.accept_existing(
            capability.capability,
            capability._guard,
            Authority::ReplaceSource,
        )
        .map(|capability| AcceptedPathCapability {
            _nonce: self.nonce,
            _capability: AcceptedCapability::Replace {
                _capability: capability,
            },
        })
    }

    pub(crate) fn accept_delete(
        &mut self,
        capability: DeleteSource<'guard, 'workspace>,
    ) -> Result<AcceptedPathCapability, PathAuthError> {
        self.accept_existing(
            capability.capability,
            capability._guard,
            Authority::DeleteSource,
        )
        .map(|capability| AcceptedPathCapability {
            _nonce: self.nonce,
            _capability: AcceptedCapability::Delete {
                _capability: capability,
            },
        })
    }

    pub(crate) fn accept_create(
        &mut self,
        capability: CreateParent<'guard, 'workspace>,
    ) -> Result<AcceptedPathCapability, PathAuthError> {
        self.accept_destination(
            capability.capability,
            capability._guard,
            Authority::CreateParent,
        )
        .map(|capability| AcceptedPathCapability {
            _nonce: self.nonce,
            _capability: AcceptedCapability::Create {
                _capability: capability,
            },
        })
    }

    pub(crate) fn accept_move(
        &mut self,
        source: MoveSource<'guard, 'workspace>,
        destination: MoveDestination<'guard, 'workspace>,
    ) -> Result<AcceptedPathCapability, PathAuthError> {
        let source =
            self.accept_existing(source.capability, source._guard, Authority::MoveSource)?;
        let destination = self.accept_destination(
            destination.capability,
            destination._guard,
            Authority::MoveDestination,
        )?;
        Ok(AcceptedPathCapability {
            _nonce: self.nonce,
            _capability: AcceptedCapability::Move {
                _source: source,
                _destination: destination,
            },
        })
    }

    pub(crate) fn finalize_before(
        &mut self,
        capabilities: &mut [AcceptedPathCapability],
    ) -> Result<(), PathAuthError> {
        for capability in capabilities.iter() {
            if capability._nonce != self.nonce {
                return Err(PathAuthError::CrossGuard);
            }
        }
        self.verify_accepted(capabilities)?;
        self.refresh_until(self.deadline)?;
        self.verify_accepted(capabilities)?;
        self.refresh_until(self.deadline)?;
        self.verify_accepted(capabilities)
    }

    fn accept_existing(
        &mut self,
        mut capability: ExistingCapability,
        guard: GuardBinding<'guard, 'workspace>,
        authority: Authority,
    ) -> Result<ExistingCapability, PathAuthError> {
        if guard.nonce != self.nonce {
            return Err(PathAuthError::CrossGuard);
        }
        capability.fence.ensure_clean().map_err(map_fence_error)?;
        self.refresh_until(self.deadline)?;
        capability.fence.ensure_clean().map_err(map_fence_error)?;
        self.verify_existing(&capability, authority)?;
        Ok(capability)
    }

    fn accept_destination(
        &mut self,
        mut capability: DestinationCapability,
        guard: GuardBinding<'guard, 'workspace>,
        authority: Authority,
    ) -> Result<DestinationCapability, PathAuthError> {
        if guard.nonce != self.nonce {
            return Err(PathAuthError::CrossGuard);
        }
        capability.fence.ensure_clean().map_err(map_fence_error)?;
        self.refresh_until(self.deadline)?;
        capability.fence.ensure_clean().map_err(map_fence_error)?;
        self.verify_destination(&capability, authority, self.deadline)?;
        Ok(capability)
    }

    fn begin_authorization(
        &mut self,
    ) -> Result<(AuthorizationBudget, WorkspaceKernelMutationFence), PathAuthError> {
        let deadline = self.deadline;
        self.refresh_until(deadline)?;
        let fence = self
            .guard
            .path_authorization_fence(self.limits.max_authorization_memory_bytes)
            .map_err(map_fence_error)?;
        Ok((AuthorizationBudget::new(self.limits, deadline), fence))
    }

    fn finish_existing(
        &mut self,
        capability: &mut ExistingCapability,
        deadline: Instant,
    ) -> Result<(), PathAuthError> {
        capability.fence.ensure_clean().map_err(map_fence_error)?;
        self.refresh_until(deadline)?;
        capability.fence.ensure_clean().map_err(map_fence_error)?;
        self.verify_existing(capability, capability.binding.authority())?;
        check_deadline(deadline)
    }

    fn finish_destination(
        &mut self,
        capability: &mut DestinationCapability,
        deadline: Instant,
    ) -> Result<(), PathAuthError> {
        capability.fence.ensure_clean().map_err(map_fence_error)?;
        self.refresh_until(deadline)?;
        capability.fence.ensure_clean().map_err(map_fence_error)?;
        self.verify_destination(capability, capability.binding.authority(), deadline)?;
        check_deadline(deadline)
    }

    fn verify_accepted(
        &mut self,
        capabilities: &mut [AcceptedPathCapability],
    ) -> Result<(), PathAuthError> {
        for accepted in capabilities {
            match &mut accepted._capability {
                AcceptedCapability::Replace { _capability }
                | AcceptedCapability::Delete { _capability } => {
                    _capability.fence.ensure_clean().map_err(map_fence_error)?;
                    self.verify_existing(_capability, _capability.binding.authority())?;
                }
                AcceptedCapability::Create { _capability } => {
                    _capability.fence.ensure_clean().map_err(map_fence_error)?;
                    self.verify_destination(_capability, Authority::CreateParent, self.deadline)?;
                }
                AcceptedCapability::Move {
                    _source,
                    _destination,
                } => {
                    _source.fence.ensure_clean().map_err(map_fence_error)?;
                    _destination.fence.ensure_clean().map_err(map_fence_error)?;
                    self.verify_existing(_source, Authority::MoveSource)?;
                    self.verify_destination(
                        _destination,
                        Authority::MoveDestination,
                        self.deadline,
                    )?;
                }
            }
            check_deadline(self.deadline)?;
        }
        Ok(())
    }

    fn require_guard(&self, guard: GuardBinding<'guard, 'workspace>) -> Result<(), PathAuthError> {
        if guard.nonce == self.nonce {
            Ok(())
        } else {
            Err(PathAuthError::CrossGuard)
        }
    }

    fn refresh_until(&mut self, deadline: Instant) -> Result<(), PathAuthError> {
        let current = self
            .guard
            .validate_revision_until(self.revision, deadline)?;
        if current.epoch() != self.epoch {
            return Err(PathAuthError::StaleEpoch {
                expected: self.epoch,
                current: current.epoch(),
            });
        }
        self.verify_named_root()?;
        check_deadline(deadline)
    }

    fn guard_binding(&self) -> GuardBinding<'guard, 'workspace> {
        GuardBinding {
            nonce: self.nonce,
            _invariant: PhantomData,
        }
    }

    fn parse_path(&self, path: &Path) -> Result<RootRelativePath, PathAuthError> {
        let Some(value) = path.to_str() else {
            return Err(PathAuthError::InvalidPath(path.to_owned()));
        };
        let parsed = RootRelativePath::parse(value, self.limits.max_path_bytes)
            .map_err(|_| PathAuthError::InvalidPath(path.to_owned()))?;
        if value
            .split('/')
            .any(|component| private_component(component, self.limits.identity_policy))
        {
            return Err(PathAuthError::PrivatePath(path.to_owned()));
        }
        if value.nfc().ne(value.chars()) {
            return Err(PathAuthError::Alias(path.to_owned()));
        }
        Ok(parsed)
    }

    fn resolve_existing(
        &self,
        path: &RootRelativePath,
        authority: Authority,
        budget: &mut AuthorizationBudget,
        mut fence: WorkspaceKernelMutationFence,
        hook: &mut dyn FnMut(&str, &Path),
    ) -> Result<ExistingCapability, PathAuthError> {
        let (parent, leaf) = self.resolve_parent(path, budget, &mut fence, hook)?;
        let parent_stat =
            stat_file(&parent).map_err(|source| io_error("inspect authorized parent", source))?;
        require_exact_entry(
            &parent,
            &leaf,
            self.limits.identity_policy,
            Path::new(path.as_str()),
            true,
            budget,
        )?;
        let before = stat_at(&parent, &leaf)
            .map_err(|source| map_leaf_error(PathBuf::from(path.as_str()), source))?;
        check_deadline(budget.deadline)?;
        match before.kind() {
            MODE_SYMLINK => return Err(PathAuthError::Symlink(PathBuf::from(path.as_str()))),
            MODE_REGULAR => {}
            MODE_DIRECTORY => return Err(PathAuthError::NotFile(PathBuf::from(path.as_str()))),
            _ => return Err(PathAuthError::SpecialFile(PathBuf::from(path.as_str()))),
        }
        let file = open_file_component(&parent, &leaf)
            .map_err(|source| map_leaf_error(PathBuf::from(path.as_str()), source))?;
        check_deadline(budget.deadline)?;
        fence
            .watch(&self.root_path.join(path.as_str()), &file, false)
            .map_err(map_fence_error)?;
        let opened =
            stat_file(&file).map_err(|source| io_error("inspect authorized file", source))?;
        check_deadline(budget.deadline)?;
        if !before.same_bound(opened) {
            return Err(PathAuthError::ObjectChanged(PathBuf::from(path.as_str())));
        }
        let named = stat_at(&parent, &leaf)
            .map_err(|source| map_leaf_error(PathBuf::from(path.as_str()), source))?;
        check_deadline(budget.deadline)?;
        if !opened.same_object(named) {
            return Err(PathAuthError::ObjectChanged(PathBuf::from(path.as_str())));
        }
        self.validate_regular(&opened, PathBuf::from(path.as_str()), &file)?;
        let requested_path = Path::new(path.as_str());
        hook("leaf-opened", requested_path);
        check_deadline(budget.deadline)?;
        Ok(ExistingCapability {
            file,
            parent,
            parent_stat,
            leaf,
            stat: opened,
            binding: binding(
                stat_identity(self.root_stat),
                self.revision,
                self.epoch,
                path.clone(),
                policy_identity(path, self.limits.identity_policy),
                Some(stat_identity(opened)),
                authority,
            ),
            fence,
        })
    }

    fn resolve_destination(
        &self,
        path: &RootRelativePath,
        authority: Authority,
        budget: &mut AuthorizationBudget,
        mut fence: WorkspaceKernelMutationFence,
        hook: &mut dyn FnMut(&str, &Path),
    ) -> Result<DestinationCapability, PathAuthError> {
        let (parent, leaf) = self.resolve_parent(path, budget, &mut fence, hook)?;
        require_exact_entry(
            &parent,
            &leaf,
            self.limits.identity_policy,
            Path::new(path.as_str()),
            false,
            budget,
        )?;
        let parent_stat =
            stat_file(&parent).map_err(|source| io_error("inspect authorized parent", source))?;
        check_deadline(budget.deadline)?;
        Ok(DestinationCapability {
            parent,
            parent_stat,
            leaf,
            binding: binding(
                stat_identity(self.root_stat),
                self.revision,
                self.epoch,
                path.clone(),
                policy_identity(path, self.limits.identity_policy),
                Some(stat_identity(parent_stat)),
                authority,
            ),
            fence,
        })
    }

    fn resolve_parent(
        &self,
        path: &RootRelativePath,
        budget: &mut AuthorizationBudget,
        fence: &mut WorkspaceKernelMutationFence,
        hook: &mut dyn FnMut(&str, &Path),
    ) -> Result<(File, CString), PathAuthError> {
        let mut components = path.as_str().split('/').peekable();
        let mut directory = self
            .root
            .try_clone()
            .map_err(|source| io_error("clone workspace root", source))?;
        let mut walked = PathBuf::new();
        fence
            .watch(&self.root_path, &directory, true)
            .map_err(map_fence_error)?;
        hook("parent-watched", &walked);
        check_deadline(budget.deadline)?;
        loop {
            let component = components.next().expect("validated paths are non-empty");
            let name = c_name(component)?;
            if components.peek().is_none() {
                return Ok((directory, name));
            }
            walked.push(component);
            require_exact_entry(
                &directory,
                &name,
                self.limits.identity_policy,
                &walked,
                true,
                budget,
            )?;
            let before = stat_at(&directory, &name)
                .map_err(|source| map_leaf_error(walked.clone(), source))?;
            check_deadline(budget.deadline)?;
            if before.kind() == MODE_SYMLINK {
                return Err(PathAuthError::Symlink(walked));
            }
            if before.kind() != MODE_DIRECTORY {
                return Err(PathAuthError::NotDirectory(walked));
            }
            let child = open_directory_component(&directory, &name)
                .map_err(|source| map_leaf_error(walked.clone(), source))?;
            check_deadline(budget.deadline)?;
            fence
                .watch(&self.root_path.join(&walked), &child, true)
                .map_err(map_fence_error)?;
            let opened = stat_file(&child)
                .map_err(|source| io_error("inspect workspace path component", source))?;
            check_deadline(budget.deadline)?;
            if !before.same_object(opened) {
                return Err(PathAuthError::ObjectChanged(walked));
            }
            let named = stat_at(&directory, &name)
                .map_err(|source| map_leaf_error(walked.clone(), source))?;
            check_deadline(budget.deadline)?;
            if !opened.same_object(named) {
                return Err(PathAuthError::ObjectChanged(walked));
            }
            self.validate_mount(&opened, &walked, &child)?;
            hook("parent-watched", &walked);
            check_deadline(budget.deadline)?;
            directory = child;
        }
    }

    fn verify_existing(
        &self,
        capability: &ExistingCapability,
        authority: Authority,
    ) -> Result<(), PathAuthError> {
        self.verify_binding(&capability.binding, authority)?;
        let current = stat_file(&capability.file)
            .map_err(|source| io_error("reinspect authorized file", source))?;
        if !capability.stat.same_bound(current) {
            return Err(PathAuthError::ObjectChanged(path_of(&capability.binding)));
        }
        let parent = stat_file(&capability.parent)
            .map_err(|source| io_error("reinspect authorized parent", source))?;
        if !capability.parent_stat.same_bound(parent) {
            return Err(PathAuthError::ObjectChanged(path_of(&capability.binding)));
        }
        let named = stat_at(&capability.parent, &capability.leaf)
            .map_err(|source| map_leaf_error(path_of(&capability.binding), source))?;
        if !current.same_object(named) {
            return Err(PathAuthError::ObjectChanged(path_of(&capability.binding)));
        }
        self.validate_regular(&current, path_of(&capability.binding), &capability.file)
    }

    fn verify_destination(
        &self,
        capability: &DestinationCapability,
        authority: Authority,
        deadline: Instant,
    ) -> Result<(), PathAuthError> {
        self.verify_binding(&capability.binding, authority)?;
        let parent = stat_file(&capability.parent)
            .map_err(|source| io_error("reinspect authorized parent", source))?;
        if !capability.parent_stat.same_bound(parent) {
            return Err(PathAuthError::ObjectChanged(path_of(&capability.binding)));
        }
        let mut budget = AuthorizationBudget::new(self.limits, deadline);
        require_exact_entry(
            &capability.parent,
            &capability.leaf,
            self.limits.identity_policy,
            Path::new(capability.binding.path().as_str()),
            false,
            &mut budget,
        )
    }

    fn verify_binding(
        &self,
        binding: &CapabilityBinding,
        authority: Authority,
    ) -> Result<(), PathAuthError> {
        if binding.root_identity() != stat_identity(self.root_stat)
            || binding.revision() != self.revision
            || binding.epoch() != self.epoch
        {
            return Err(PathAuthError::CrossRoot);
        }
        if binding.authority() != authority {
            return Err(PathAuthError::WrongAuthority {
                expected: authority,
                actual: binding.authority(),
            });
        }
        Ok(())
    }

    fn verify_named_root(&self) -> Result<(), PathAuthError> {
        let matches = (|| {
            let named = open_absolute_directory(&self.root_path)?;
            let stat = stat_file(&named)
                .map_err(|source| io_error("inspect named workspace root", source))?;
            Ok::<_, PathAuthError>(
                stat_identity(self.root_stat) == stat_identity(stat)
                    && mount_identity(&named)? == self.root_mount,
            )
        })();
        if matches.as_ref().is_ok_and(|matches| *matches) {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        return Err(PathAuthError::Unavailable {
            reason: "named macOS workspace mount replacement is ambiguous",
        });
        #[cfg(target_os = "linux")]
        match matches {
            Ok(false) => Err(PathAuthError::ObjectChanged(self.root_path.clone())),
            Err(error) => Err(error),
            Ok(true) => unreachable!(),
        }
    }

    fn validate_regular(
        &self,
        stat: &Stat,
        path: PathBuf,
        file: &File,
    ) -> Result<(), PathAuthError> {
        if stat.kind() != MODE_REGULAR {
            return Err(PathAuthError::SpecialFile(path));
        }
        if stat.links() > 1 {
            return Err(PathAuthError::Hardlink(path));
        }
        self.validate_mount(stat, &path, file)
    }

    fn validate_mount(&self, stat: &Stat, path: &Path, file: &File) -> Result<(), PathAuthError> {
        if stat.device() != self.root_stat.device() || mount_identity(file)? != self.root_mount {
            Err(PathAuthError::MountBoundary(path.to_owned()))
        } else {
            Ok(())
        }
    }
}

macro_rules! binding_accessor {
    ($type:ident) => {
        impl<'guard, 'workspace> $type<'guard, 'workspace> {
            pub fn binding(&self) -> &CapabilityBinding {
                &self.capability.binding
            }
        }
    };
}

binding_accessor!(ReplaceSource);
binding_accessor!(DeleteSource);
binding_accessor!(CreateParent);
binding_accessor!(MoveSource);
binding_accessor!(MoveDestination);

impl ExistingRead<'_, '_> {
    pub fn binding(&self) -> &CapabilityBinding {
        &self.capability.binding
    }
}

fn validate_limits(limits: EditLimits) -> Result<(), PathAuthError> {
    if limits.max_authorization_entries == 0
        || limits.max_authorization_name_bytes == 0
        || limits.max_authorization_memory_bytes == 0
        || limits.max_authorization_time.is_zero()
        || limits.max_content_bytes == 0
    {
        Err(PathAuthError::Unavailable {
            reason: "path authorization limits must be nonzero",
        })
    } else {
        Ok(())
    }
}

fn authorization_deadline(limits: EditLimits) -> Instant {
    Instant::now()
        .checked_add(limits.max_authorization_time)
        .unwrap_or_else(Instant::now)
}

struct AuthorizationBudget {
    entries: usize,
    name_bytes: usize,
    limits: EditLimits,
    deadline: Instant,
}

impl AuthorizationBudget {
    fn new(limits: EditLimits, deadline: Instant) -> Self {
        Self {
            entries: 0,
            name_bytes: 0,
            limits,
            deadline,
        }
    }

    fn charge_name(&mut self, bytes: usize) -> Result<(), PathAuthError> {
        check_deadline(self.deadline)?;
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or(PathAuthError::LimitExceeded(PathAuthLimit::Entries))?;
        if self.entries > self.limits.max_authorization_entries {
            return Err(PathAuthError::LimitExceeded(PathAuthLimit::Entries));
        }
        self.name_bytes = self
            .name_bytes
            .checked_add(bytes)
            .ok_or(PathAuthError::LimitExceeded(PathAuthLimit::NameBytes))?;
        if self.name_bytes > self.limits.max_authorization_name_bytes {
            return Err(PathAuthError::LimitExceeded(PathAuthLimit::NameBytes));
        }
        if bytes > self.limits.max_authorization_memory_bytes {
            return Err(PathAuthError::LimitExceeded(PathAuthLimit::Memory));
        }
        Ok(())
    }

    fn check_identity_memory(&self, bytes: usize) -> Result<(), PathAuthError> {
        if bytes > self.limits.max_authorization_memory_bytes {
            Err(PathAuthError::LimitExceeded(PathAuthLimit::Memory))
        } else {
            Ok(())
        }
    }

    fn check_identity_inputs(&self, bytes: usize) -> Result<(), PathAuthError> {
        const MAX_UNICODE_EXPANSION: usize = 64;
        let maximum = bytes
            .checked_mul(MAX_UNICODE_EXPANSION)
            .ok_or(PathAuthError::LimitExceeded(PathAuthLimit::Memory))?;
        self.check_identity_memory(maximum)
    }
}

fn check_deadline(deadline: Instant) -> Result<(), PathAuthError> {
    if Instant::now() >= deadline {
        Err(PathAuthError::LimitExceeded(PathAuthLimit::Time))
    } else {
        Ok(())
    }
}

fn private_component(component: &str, policy: FilesystemIdentityPolicy) -> bool {
    let identity = component_identity(component, policy);
    identity == component_identity(".git", policy)
        || identity == component_identity(".kit", policy)
        || identity == component_identity(".kit.lock", policy)
        || identity.starts_with(&component_identity(".kit-", policy))
}

fn path_of(binding: &CapabilityBinding) -> PathBuf {
    PathBuf::from(binding.path().as_str())
}

fn c_name(name: impl AsRef<OsStr>) -> Result<CString, PathAuthError> {
    CString::new(name.as_ref().as_bytes())
        .map_err(|_| PathAuthError::InvalidPath(PathBuf::from(name.as_ref())))
}

fn require_exact_entry(
    directory: &File,
    requested: &CStr,
    policy: FilesystemIdentityPolicy,
    path: &Path,
    must_exist: bool,
    budget: &mut AuthorizationBudget,
) -> Result<(), PathAuthError> {
    let requested_bytes = requested.to_bytes();
    let requested_text = std::str::from_utf8(requested_bytes)
        .map_err(|_| PathAuthError::InvalidPath(path.to_owned()))?;
    budget.check_identity_inputs(requested_bytes.len())?;
    let requested_identity = component_identity(requested_text, policy);
    budget.check_identity_memory(requested_identity.len())?;
    let mut stream = sys::DirectoryStream::open(directory)
        .map_err(|source| io_error("enumerate directory", source))?;
    let mut exact = false;
    let mut aliases = 0_usize;
    loop {
        let Some(name) = stream
            .next()
            .map_err(|source| io_error("enumerate directory", source))?
        else {
            break;
        };
        check_deadline(budget.deadline)?;
        let name = name.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        budget.charge_name(name.len())?;
        let Ok(text) = std::str::from_utf8(name) else {
            continue;
        };
        budget.check_identity_inputs(requested_identity.len().saturating_add(name.len()))?;
        let identity = component_identity(text, policy);
        budget.check_identity_memory(identity.len())?;
        if identity == requested_identity {
            aliases = aliases
                .checked_add(1)
                .ok_or(PathAuthError::LimitExceeded(PathAuthLimit::Entries))?;
            exact |= name == requested_bytes;
        }
    }
    check_deadline(budget.deadline)?;
    if aliases > usize::from(exact) || (aliases != 0 && !exact) {
        return Err(PathAuthError::Alias(path.to_owned()));
    }
    match (must_exist, exact) {
        (true, false) => Err(PathAuthError::NotFound(path.to_owned())),
        (false, true) => Err(PathAuthError::AlreadyExists(path.to_owned())),
        _ => Ok(()),
    }
}

fn component_identity(value: &str, policy: FilesystemIdentityPolicy) -> String {
    let normalized: String = value.nfc().collect();
    match policy {
        FilesystemIdentityPolicy::Portable => normalized
            .chars()
            .flat_map(char::to_uppercase)
            .flat_map(char::to_lowercase)
            .collect::<String>()
            .nfc()
            .collect(),
        FilesystemIdentityPolicy::CaseSensitive => normalized,
    }
}

fn open_absolute_directory(path: &Path) -> Result<File, PathAuthError> {
    if !path.is_absolute() {
        return Err(PathAuthError::InvalidPath(path.to_owned()));
    }
    let mut directory =
        sys::open_filesystem_root().map_err(|source| io_error("open filesystem root", source))?;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                let name = c_name(name)?;
                directory = open_directory_component(&directory, &name)
                    .map_err(|source| map_leaf_error(path.to_owned(), source))?;
            }
            _ => return Err(PathAuthError::InvalidPath(path.to_owned())),
        }
    }
    Ok(directory)
}

fn mount_identity(file: &File) -> Result<MountIdentity, PathAuthError> {
    sys::mount_identity(file)
        .map_err(|_| PathAuthError::Unavailable {
            reason: "descriptor-relative mount identity",
        })?
        .ok_or(PathAuthError::Unavailable {
            reason: "descriptor-relative mount identity",
        })
}

fn stat_identity(stat: Stat) -> FileIdentity {
    file_identity(
        stat.device(),
        stat.inode(),
        if stat.kind() == MODE_DIRECTORY {
            EntryType::Directory
        } else {
            EntryType::RegularFile
        },
        stat.mode(),
    )
}

fn map_leaf_error(path: PathBuf, source: io::Error) -> PathAuthError {
    if sys::is_symlink_loop(&source) {
        PathAuthError::Symlink(path)
    } else if matches!(
        source.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) {
        PathAuthError::ObjectChanged(path)
    } else {
        io_error("access authorized workspace path", source)
    }
}

fn io_error(operation: &'static str, source: io::Error) -> PathAuthError {
    PathAuthError::Io { operation, source }
}

fn map_fence_error(error: RevisionError) -> PathAuthError {
    match error {
        RevisionError::LimitExceeded(LimitKind::Memory) => {
            PathAuthError::LimitExceeded(PathAuthLimit::Memory)
        }
        error => PathAuthError::Revision(error),
    }
}
