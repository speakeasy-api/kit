use std::fs::File;

use crate::{store::artifacts::ArtifactStore, workspace::edit::stage::VerifiedStagedEdit};

use super::{MaterializeOptions, MaterializedEdit, RecoveryError, RecoveryHook};

pub fn materialize(
    _stage: VerifiedStagedEdit<'_>,
    _artifacts: &ArtifactStore,
    _options: MaterializeOptions,
) -> Result<MaterializedEdit, RecoveryError> {
    Err(RecoveryError::Unavailable)
}

pub fn materialize_with_hook(
    _stage: VerifiedStagedEdit<'_>,
    _artifacts: &ArtifactStore,
    _options: MaterializeOptions,
    _hook: RecoveryHook<'_>,
) -> Result<MaterializedEdit, RecoveryError> {
    Err(RecoveryError::Unavailable)
}

pub(crate) fn recover_pending<E>(
    _root: &File,
    _state: &File,
    _artifacts: impl FnMut(
        &std::path::Path,
    ) -> Result<ArtifactStore, crate::store::artifacts::ArtifactError>,
    _committed: impl FnMut(&str, &str, &str, &str, &str, &str) -> Result<super::RecoveryPosition, E>,
) -> Result<(), RecoveryError> {
    Err(RecoveryError::Unavailable)
}
