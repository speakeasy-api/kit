use super::{StageChange, StageError, StageLimits, StagedOperation, SyntaxRequirements};
use crate::workspace::edit::{ir::RootRelativePath, validate::ValidatedPlan};
use crate::workspace::revision::RevisionId;
use std::time::Instant;

pub struct StagedEdit<'workspace>(std::marker::PhantomData<&'workspace ()>);

impl StagedEdit<'_> {
    pub fn revision(&self) -> RevisionId {
        unreachable!()
    }

    pub fn plan_digest(&self) -> &str {
        unreachable!()
    }

    pub fn digest(&self) -> &str {
        unreachable!()
    }

    pub fn state_digest(&self) -> &str {
        unreachable!()
    }

    pub fn evidence_digest(&self) -> &str {
        unreachable!()
    }

    pub fn operation_context(&self) -> &crate::workspace::edit::validate::EditOperationContext {
        unreachable!()
    }

    pub(crate) fn workspace_digest(&self) -> &str {
        unreachable!()
    }

    pub(crate) fn expected_change_diff_digest(&self) -> Option<&str> {
        unreachable!()
    }

    pub(crate) fn operations(&self) -> &[StagedOperation] {
        unreachable!()
    }

    pub fn changes(&self) -> &[StageChange] {
        unreachable!()
    }

    pub fn read_file(
        &self,
        _path: &RootRelativePath,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, StageError> {
        Err(StageError::Unavailable)
    }

    pub fn read_file_before(
        &self,
        _path: &RootRelativePath,
        _max_bytes: usize,
        _deadline: Instant,
    ) -> Result<Vec<u8>, StageError> {
        Err(StageError::Unavailable)
    }
}

pub fn stage<'workspace>(
    _plan: ValidatedPlan<'workspace>,
    _limits: StageLimits,
    _syntax: SyntaxRequirements<'_>,
    _syntax_executors: &mut [&mut crate::executor::syntax::SyntaxExecutor],
) -> Result<StagedEdit<'workspace>, StageError> {
    Err(StageError::Unavailable)
}
