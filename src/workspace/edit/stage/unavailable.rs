use super::{
    FormatterCapture, StageChange, StageError, StageLimits, StagedOperation, SyntaxRequirements,
};
use crate::workspace::edit::{
    format::FormatterDescriptor, ir::RootRelativePath, validate::ValidatedPlan,
};
use crate::workspace::revision::RevisionId;
use std::time::Instant;

pub struct StagedEdit<'workspace>(std::marker::PhantomData<&'workspace ()>);

impl StagedEdit<'_> {
    pub fn verify(
        self,
        _request: crate::verify::profiles::VerificationRequest<'_>,
    ) -> Result<super::VerificationOutcome<'_>, crate::verify::profiles::VerificationError> {
        Err(crate::verify::profiles::VerificationError::StaleBinding)
    }

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

    pub(crate) fn feedback_mapping(&self) -> &crate::verify::feedback::EditMapping {
        unreachable!()
    }

    pub fn formatter(&self) -> Option<&FormatterCapture> {
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
    _formatter: Option<(
        &FormatterDescriptor,
        &mut crate::executor::formatter::FormatterExecutor,
    )>,
) -> Result<StagedEdit<'workspace>, StageError> {
    Err(StageError::Unavailable)
}
