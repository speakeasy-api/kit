use std::path::Path;
use std::time::Instant;

use super::{CapabilityBinding, PathAuthError};
use crate::workspace::{
    edit::ir::EditLimits,
    revision::{EpochId, RevisionId, WorkspaceMutationGuard},
};

const REASON: &str = "descriptor-relative no-follow path authorization requires Linux or macOS";

pub struct PathAuthorizer<'guard, 'workspace> {
    _private: std::marker::PhantomData<&'guard mut WorkspaceMutationGuard<'workspace>>,
}
pub struct ExistingRead<'guard, 'workspace>(
    std::marker::PhantomData<
        fn(
            &'guard mut WorkspaceMutationGuard<'workspace>,
        ) -> &'guard mut WorkspaceMutationGuard<'workspace>,
    >,
);
pub struct ReplaceSource<'guard, 'workspace>(ExistingRead<'guard, 'workspace>);
pub struct DeleteSource<'guard, 'workspace>(ExistingRead<'guard, 'workspace>);
pub struct CreateParent<'guard, 'workspace>(ExistingRead<'guard, 'workspace>);
pub struct MoveSource<'guard, 'workspace>(ExistingRead<'guard, 'workspace>);
pub struct MoveDestination<'guard, 'workspace>(ExistingRead<'guard, 'workspace>);
pub(crate) struct AcceptedPathCapability;

impl AcceptedPathCapability {
    pub(crate) fn source_binding(&self) -> Option<&CapabilityBinding> {
        None
    }
}

impl<'guard, 'workspace> PathAuthorizer<'guard, 'workspace> {
    pub fn new(
        _guard: &'guard mut WorkspaceMutationGuard<'workspace>,
        _revision: RevisionId,
        _epoch: EpochId,
        _limits: EditLimits,
    ) -> Result<Self, PathAuthError> {
        Err(unavailable())
    }

    pub(crate) fn new_before(
        _guard: &'guard mut WorkspaceMutationGuard<'workspace>,
        _revision: RevisionId,
        _epoch: EpochId,
        _limits: EditLimits,
        _deadline: Instant,
    ) -> Result<Self, PathAuthError> {
        Err(unavailable())
    }

    pub fn authorize_read(
        &mut self,
        _path: impl AsRef<Path>,
    ) -> Result<ExistingRead<'guard, 'workspace>, PathAuthError> {
        Err(unavailable())
    }

    pub fn authorize_replace(
        &mut self,
        _path: impl AsRef<Path>,
    ) -> Result<ReplaceSource<'guard, 'workspace>, PathAuthError> {
        Err(unavailable())
    }

    pub fn authorize_delete(
        &mut self,
        _path: impl AsRef<Path>,
    ) -> Result<DeleteSource<'guard, 'workspace>, PathAuthError> {
        Err(unavailable())
    }

    pub fn authorize_create(
        &mut self,
        _path: impl AsRef<Path>,
    ) -> Result<CreateParent<'guard, 'workspace>, PathAuthError> {
        Err(unavailable())
    }

    pub fn authorize_move(
        &mut self,
        _from: impl AsRef<Path>,
        _to: impl AsRef<Path>,
    ) -> Result<
        (
            MoveSource<'guard, 'workspace>,
            MoveDestination<'guard, 'workspace>,
        ),
        PathAuthError,
    > {
        Err(unavailable())
    }

    pub fn read(
        &mut self,
        _capability: ExistingRead<'guard, 'workspace>,
        _max_bytes: usize,
        _max_memory_bytes: usize,
        _deadline: Instant,
    ) -> Result<Vec<u8>, PathAuthError> {
        Err(unavailable())
    }

    pub(crate) fn accept_replace(
        &mut self,
        _capability: ReplaceSource<'guard, 'workspace>,
    ) -> Result<AcceptedPathCapability, PathAuthError> {
        Err(unavailable())
    }

    pub(crate) fn accept_delete(
        &mut self,
        _capability: DeleteSource<'guard, 'workspace>,
    ) -> Result<AcceptedPathCapability, PathAuthError> {
        Err(unavailable())
    }

    pub(crate) fn accept_create(
        &mut self,
        _capability: CreateParent<'guard, 'workspace>,
    ) -> Result<AcceptedPathCapability, PathAuthError> {
        Err(unavailable())
    }

    pub(crate) fn accept_move(
        &mut self,
        _source: MoveSource<'guard, 'workspace>,
        _destination: MoveDestination<'guard, 'workspace>,
    ) -> Result<AcceptedPathCapability, PathAuthError> {
        Err(unavailable())
    }

    pub(crate) fn finalize_before(
        &mut self,
        _capabilities: &mut [AcceptedPathCapability],
    ) -> Result<(), PathAuthError> {
        Err(unavailable())
    }
}

macro_rules! unavailable_binding {
    ($type:ident) => {
        impl<'guard, 'workspace> $type<'guard, 'workspace> {
            pub fn binding(&self) -> &CapabilityBinding {
                unreachable!()
            }
        }
    };
}

unavailable_binding!(ExistingRead);
unavailable_binding!(ReplaceSource);
unavailable_binding!(DeleteSource);
unavailable_binding!(CreateParent);
unavailable_binding!(MoveSource);
unavailable_binding!(MoveDestination);

fn unavailable() -> PathAuthError {
    PathAuthError::Unavailable { reason: REASON }
}
