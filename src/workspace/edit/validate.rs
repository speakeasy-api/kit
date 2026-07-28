use std::{fmt, fmt::Write as _, path::Path, sync::Arc, time::Instant};

use sha2::{Digest as _, Sha256};

use crate::{
    api::auth::contract::{AuthenticatedPrincipal, GrantSnapshot},
    domain::{
        config::Grant,
        events::ContentDigest,
        ids::{PrincipalId, ProjectId},
    },
    workspace::{
        path_auth::{
            AcceptedPathCapability, CapabilityBinding, EntryType, FileIdentity, PathAuthError,
            PathAuthLimit, PathAuthorizer,
        },
        revision::{
            EpochId, LimitKind, ManagedWorkspace, Revision, RevisionError, RevisionId,
            WorkspaceMutationGuard,
        },
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthenticatedEditAuthority {
    principal: PrincipalId,
    project: ProjectId,
}

impl AuthenticatedEditAuthority {
    pub(crate) fn from_authenticated(
        authenticated: &AuthenticatedPrincipal,
        grants: &GrantSnapshot,
        project: ProjectId,
    ) -> Result<Self, ValidationError> {
        if authenticated.grant_snapshot() != grants
            || grants.principal_id() != authenticated.principal_id()
            || grants.project_id() != project
            || !grants.grants().contains(&Grant::WorkspaceWrite)
        {
            return Err(ValidationError::IdentityPolicyMismatch);
        }
        Ok(Self {
            principal: grants.principal_id(),
            project,
        })
    }

    pub const fn principal(self) -> PrincipalId {
        self.principal
    }

    pub const fn project(self) -> ProjectId {
        self.project
    }
}

use super::ir::{
    ByteRange, EditIr, EditLimits, EditOperation, ExecutableMode, Newline, RootRelativePath,
    TextContent,
};

pub struct ValidatedTransaction<'workspace> {
    guard: WorkspaceMutationGuard<'workspace>,
    capabilities: Vec<AcceptedPathCapability>,
    limits: EditLimits,
    revision: RevisionId,
    epoch: EpochId,
    workspace_digest: String,
    digest: String,
    effects: Vec<PlannedEffect>,
    expected_paths: Vec<ExpectedPath>,
    changed_files: Vec<RootRelativePath>,
    authority: Option<AuthenticatedEditAuthority>,
}

pub type ValidatedPlan<'workspace> = ValidatedTransaction<'workspace>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditOperationContext {
    base_revision: String,
    base_epoch: String,
    base_workspace_digest: String,
    selected_plan_digest: String,
}

impl EditOperationContext {
    pub(crate) fn current(
        base_revision: impl Into<String>,
        base_epoch: impl Into<String>,
        base_workspace_digest: impl Into<String>,
        selected_plan_digest: impl Into<String>,
    ) -> Self {
        Self {
            base_revision: base_revision.into(),
            base_epoch: base_epoch.into(),
            base_workspace_digest: base_workspace_digest.into(),
            selected_plan_digest: selected_plan_digest.into(),
        }
    }

    pub fn base_revision(&self) -> &str {
        &self.base_revision
    }

    pub fn base_epoch(&self) -> &str {
        &self.base_epoch
    }

    pub fn base_workspace_digest(&self) -> &str {
        &self.base_workspace_digest
    }

    pub fn selected_plan_digest(&self) -> &str {
        &self.selected_plan_digest
    }
}

impl ValidatedTransaction<'_> {
    pub fn operation_context(&self) -> EditOperationContext {
        EditOperationContext {
            base_revision: self.revision.to_string(),
            base_epoch: self.epoch.to_string(),
            base_workspace_digest: self.workspace_digest.clone(),
            selected_plan_digest: self.digest.clone(),
        }
    }

    pub fn revision(&self) -> RevisionId {
        self.revision
    }

    pub fn epoch(&self) -> EpochId {
        self.epoch
    }

    pub fn workspace_digest(&self) -> &str {
        &self.workspace_digest
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn effects(&self) -> &[PlannedEffect] {
        &self.effects
    }

    pub fn expected_paths(&self) -> &[ExpectedPath] {
        &self.expected_paths
    }

    pub fn changed_files(&self) -> &[RootRelativePath] {
        &self.changed_files
    }

    pub fn revalidate_before(&mut self, deadline: Instant) -> Result<(), ValidationError> {
        let mut authorizer = PathAuthorizer::new_before(
            &mut self.guard,
            self.revision,
            self.epoch,
            self.limits,
            deadline,
        )
        .map_err(map_path_error)?;
        authorizer
            .finalize_before(&mut self.capabilities)
            .map_err(map_path_error)
    }

    pub(crate) fn capability_source_identities(&self) -> Vec<FileIdentity> {
        self.capabilities
            .iter()
            .filter_map(|capability| capability.source_binding()?.object_identity())
            .collect()
    }
}

pub(crate) struct PlanConsumption<'workspace> {
    pub(crate) guard: WorkspaceMutationGuard<'workspace>,
    pub(crate) capabilities: Vec<AcceptedPathCapability>,
    pub(crate) limits: EditLimits,
    pub(crate) revision: RevisionId,
    pub(crate) epoch: EpochId,
    pub(crate) workspace_digest: String,
    pub(crate) digest: String,
    pub(crate) effects: Vec<PlannedEffect>,
    pub(crate) expected_paths: Vec<ExpectedPath>,
    pub(crate) changed_files: Vec<RootRelativePath>,
    pub(crate) authority: Option<AuthenticatedEditAuthority>,
    pub(crate) operation_context: EditOperationContext,
}

impl<'workspace> ValidatedTransaction<'workspace> {
    pub(crate) fn consume_before(
        mut self,
        deadline: Instant,
    ) -> Result<PlanConsumption<'workspace>, ValidationError> {
        self.revalidate_before(deadline)?;
        let operation_context = self.operation_context();
        Ok(PlanConsumption {
            guard: self.guard,
            capabilities: self.capabilities,
            limits: self.limits,
            revision: self.revision,
            epoch: self.epoch,
            workspace_digest: self.workspace_digest,
            digest: self.digest,
            effects: self.effects,
            expected_paths: self.expected_paths,
            changed_files: self.changed_files,
            authority: self.authority,
            operation_context,
        })
    }
}

impl PlanConsumption<'_> {
    pub(crate) fn revalidate_before(&mut self, deadline: Instant) -> Result<(), ValidationError> {
        let mut authorizer = PathAuthorizer::new_before(
            &mut self.guard,
            self.revision,
            self.epoch,
            self.limits,
            deadline,
        )
        .map_err(map_path_error)?;
        authorizer
            .finalize_before(&mut self.capabilities)
            .map_err(map_path_error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeforeFile {
    digest: ContentDigest,
    identity: FileIdentity,
    mode: u32,
    content: Option<Arc<Vec<u8>>>,
}

impl BeforeFile {
    pub fn digest(&self) -> &ContentDigest {
        &self.digest
    }

    pub fn identity(&self) -> FileIdentity {
        self.identity
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub(crate) fn content(&self) -> Option<&[u8]> {
        self.content.as_deref().map(Vec::as_slice)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedFile {
    content: Arc<Vec<u8>>,
    digest: ContentDigest,
    mode: u32,
}

impl PlannedFile {
    pub fn content(&self) -> &[u8] {
        self.content.as_slice()
    }

    pub fn digest(&self) -> &ContentDigest {
        &self.digest
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedRange {
    pub operation_id: String,
    pub range: ByteRange,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlannedEffect {
    Add {
        operation_id: String,
        path: RootRelativePath,
        after: PlannedFile,
    },
    Delete {
        operation_id: String,
        path: RootRelativePath,
        before: BeforeFile,
    },
    Move {
        operation_id: String,
        from: RootRelativePath,
        to: RootRelativePath,
        before: BeforeFile,
        after: PlannedFile,
    },
    Replace {
        ranges: Vec<PlannedRange>,
        path: RootRelativePath,
        before: BeforeFile,
        after: PlannedFile,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExpectedPath {
    Absent(RootRelativePath),
    File {
        path: RootRelativePath,
        file: PlannedFile,
    },
}

impl ExpectedPath {
    pub fn path(&self) -> &RootRelativePath {
        match self {
            Self::Absent(path) | Self::File { path, .. } => path,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsafePathKind {
    Invalid,
    Private,
    Alias,
    Symlink,
    Hardlink,
    Special,
    MountBoundary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationLimit {
    Operations,
    Path,
    Content,
    ReadBytes,
    Memory,
    Time,
    Authorization,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationError {
    IdentityPolicyMismatch,
    StaleRevision,
    ExternalEdit,
    AmbiguousAnchor(RootRelativePath),
    AnchorMismatch(RootRelativePath),
    BaseDigestMismatch(RootRelativePath),
    InvalidUnicode(RootRelativePath),
    NewlineMismatch(RootRelativePath),
    FinalNewlineMismatch(RootRelativePath),
    BinaryFile(RootRelativePath),
    RangeOutsideFile(RootRelativePath),
    UnsafePath(UnsafePathKind),
    PathStateMismatch,
    LimitExceeded(ValidationLimit),
    Unavailable,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdentityPolicyMismatch => {
                formatter.write_str("edit IR identity policy does not match active policy")
            }
            Self::StaleRevision => formatter.write_str("stale workspace revision"),
            Self::ExternalEdit => formatter.write_str("workspace changed during validation"),
            Self::AmbiguousAnchor(path) => write!(formatter, "ambiguous exact anchor at {path}"),
            Self::AnchorMismatch(path) => write!(formatter, "exact anchor mismatch at {path}"),
            Self::BaseDigestMismatch(path) => write!(formatter, "base digest mismatch at {path}"),
            Self::InvalidUnicode(path) => write!(formatter, "invalid UTF-8 boundary at {path}"),
            Self::NewlineMismatch(path) => {
                write!(formatter, "newline semantics mismatch at {path}")
            }
            Self::FinalNewlineMismatch(path) => {
                write!(formatter, "final-newline semantics mismatch at {path}")
            }
            Self::BinaryFile(path) => write!(formatter, "binary file is not editable at {path}"),
            Self::RangeOutsideFile(path) => write!(formatter, "range is outside file at {path}"),
            Self::UnsafePath(kind) => write!(formatter, "unsafe workspace path: {kind:?}"),
            Self::PathStateMismatch => formatter.write_str("workspace path state mismatch"),
            Self::LimitExceeded(kind) => write!(formatter, "validation exceeded {kind:?} limit"),
            Self::Unavailable => formatter.write_str("safe edit validation is unavailable"),
        }
    }
}

impl std::error::Error for ValidationError {}

pub fn validate<'workspace>(
    workspace: &'workspace ManagedWorkspace,
    ir: &EditIr,
    limits: EditLimits,
) -> Result<ValidatedTransaction<'workspace>, ValidationError> {
    validate_inner(workspace, ir, limits, None, &mut |_, _| {})
}

pub fn validate_traced<'workspace>(
    workspace: &'workspace ManagedWorkspace,
    ir: &EditIr,
    limits: EditLimits,
    trace: &mut impl super::EditTrace,
) -> Result<ValidatedTransaction<'workspace>, ValidationError> {
    let transaction = validate(workspace, ir, limits)?;
    trace.emit(super::EditTraceId::Validate);
    Ok(transaction)
}

pub fn validate_authorized<'workspace>(
    workspace: &'workspace ManagedWorkspace,
    ir: &EditIr,
    limits: EditLimits,
    authority: AuthenticatedEditAuthority,
) -> Result<ValidatedTransaction<'workspace>, ValidationError> {
    validate_inner(workspace, ir, limits, Some(authority), &mut |_, _| {})
}

pub(crate) fn validate_authorized_traced<'workspace>(
    workspace: &'workspace ManagedWorkspace,
    ir: &EditIr,
    limits: EditLimits,
    authority: AuthenticatedEditAuthority,
    trace: &mut impl super::EditTrace,
) -> Result<ValidatedTransaction<'workspace>, ValidationError> {
    let transaction = validate_authorized(workspace, ir, limits, authority)?;
    trace.emit(super::EditTraceId::Validate);
    Ok(transaction)
}

pub(crate) fn validate_with_hook<'workspace>(
    workspace: &'workspace ManagedWorkspace,
    ir: &EditIr,
    limits: EditLimits,
    mut hook: impl FnMut(&str, &Path),
) -> Result<ValidatedTransaction<'workspace>, ValidationError> {
    validate_inner(workspace, ir, limits, None, &mut hook)
}

fn validate_inner<'workspace>(
    workspace: &'workspace ManagedWorkspace,
    ir: &EditIr,
    limits: EditLimits,
    authority: Option<AuthenticatedEditAuthority>,
    hook: &mut dyn FnMut(&str, &Path),
) -> Result<ValidatedTransaction<'workspace>, ValidationError> {
    let _profile = ValidationProfile::new();
    let deadline = Instant::now()
        .checked_add(limits.max_validation_time)
        .ok_or(ValidationError::LimitExceeded(ValidationLimit::Time))?;
    check_deadline(deadline)?;
    enforce_active_ir_limits(ir, limits, deadline)?;
    if limits.max_validation_read_bytes == 0 {
        return Err(ValidationError::LimitExceeded(ValidationLimit::ReadBytes));
    }
    if limits.max_validation_memory_bytes == 0 {
        return Err(ValidationError::LimitExceeded(ValidationLimit::Memory));
    }
    let expected =
        RevisionId::parse(ir.expected_revision().as_str()).ok_or(ValidationError::StaleRevision)?;
    let mut guard = workspace
        .mutation_guard_before(expected, deadline)
        .map_err(map_revision_error)?;
    let revision = guard.revision().clone();
    hook("guard-acquired", Path::new(""));
    check_deadline(deadline)?;

    let mut authorizer = PathAuthorizer::new_before(
        &mut guard,
        revision.id(),
        revision.epoch(),
        limits,
        deadline,
    )
    .map_err(map_path_error)?;
    let mut budget = ValidationBudget::new(limits);
    let mut capabilities = Vec::new();
    reserve_exact(&mut capabilities, ir.operations().len(), &mut budget)?;
    let mut effects = Vec::new();
    reserve_exact(&mut effects, ir.operations().len(), &mut budget)?;
    let mut expected_paths = Vec::new();
    reserve_exact(
        &mut expected_paths,
        ir.operations()
            .len()
            .checked_mul(2)
            .ok_or(ValidationError::LimitExceeded(ValidationLimit::Memory))?,
        &mut budget,
    )?;
    let mut changed_files = Vec::new();
    reserve_exact(
        &mut changed_files,
        ir.operations()
            .len()
            .checked_mul(2)
            .ok_or(ValidationError::LimitExceeded(ValidationLimit::Memory))?,
        &mut budget,
    )?;
    let mut replaced = Vec::new();
    reserve_exact(&mut replaced, ir.operations().len(), &mut budget)?;

    for canonical in ir.operations() {
        check_deadline(deadline)?;
        match canonical.operation() {
            EditOperation::AddFile {
                path,
                content,
                executable,
            } => {
                let capability = authorizer
                    .authorize_create(path.as_str())
                    .map_err(map_path_error)?;
                let bytes = render(content, &mut budget, deadline)?;
                let after = planned_file(
                    bytes,
                    if *executable { 0o755 } else { 0o644 },
                    &mut budget,
                    deadline,
                )?;
                effects.push(PlannedEffect::Add {
                    operation_id: clone_string(canonical.id(), &mut budget)?,
                    path: clone_path(path, &mut budget)?,
                    after: after.clone(),
                });
                expected_paths.push(ExpectedPath::File {
                    path: clone_path(path, &mut budget)?,
                    file: after,
                });
                changed_files.push(clone_path(path, &mut budget)?);
                capabilities.push(
                    authorizer
                        .accept_create(capability)
                        .map_err(map_path_error)?,
                );
            }
            EditOperation::DeleteFile { path, base_digest } => {
                let mut capability = authorizer
                    .authorize_delete(path.as_str())
                    .map_err(map_path_error)?;
                let bytes = read_delete_authorized(
                    &mut authorizer,
                    &mut capability,
                    path,
                    hook,
                    &mut budget,
                    deadline,
                )?;
                require_digest(path, base_digest, &bytes, deadline)?;
                let before = before_file(capability.binding(), base_digest, None, &mut budget)?;
                effects.push(PlannedEffect::Delete {
                    operation_id: clone_string(canonical.id(), &mut budget)?,
                    path: clone_path(path, &mut budget)?,
                    before,
                });
                expected_paths.push(ExpectedPath::Absent(clone_path(path, &mut budget)?));
                changed_files.push(clone_path(path, &mut budget)?);
                capabilities.push(
                    authorizer
                        .accept_delete(capability)
                        .map_err(map_path_error)?,
                );
            }
            EditOperation::MoveFile {
                from,
                to,
                base_digest,
            } => {
                let (mut source, destination) = authorizer
                    .authorize_move(from.as_str(), to.as_str())
                    .map_err(map_path_error)?;
                let bytes = read_move_authorized(
                    &mut authorizer,
                    &mut source,
                    from,
                    hook,
                    &mut budget,
                    deadline,
                )?;
                require_digest(from, base_digest, &bytes, deadline)?;
                let before = before_file(source.binding(), base_digest, None, &mut budget)?;
                let after = planned_file(bytes, before.mode, &mut budget, deadline)?;
                effects.push(PlannedEffect::Move {
                    operation_id: clone_string(canonical.id(), &mut budget)?,
                    from: clone_path(from, &mut budget)?,
                    to: clone_path(to, &mut budget)?,
                    before,
                    after: after.clone(),
                });
                expected_paths.push(ExpectedPath::Absent(clone_path(from, &mut budget)?));
                expected_paths.push(ExpectedPath::File {
                    path: clone_path(to, &mut budget)?,
                    file: after,
                });
                changed_files.push(clone_path(from, &mut budget)?);
                changed_files.push(clone_path(to, &mut budget)?);
                capabilities.push(
                    authorizer
                        .accept_move(source, destination)
                        .map_err(map_path_error)?,
                );
            }
            EditOperation::ReplaceRange { path, .. } => {
                if replaced.iter().any(|candidate| candidate == path) {
                    continue;
                }
                replaced.push(clone_path(path, &mut budget)?);
                let ranges = replacement_operations(ir, path, &mut budget, deadline)?;
                let mut capability = authorizer
                    .authorize_replace(path.as_str())
                    .map_err(map_path_error)?;
                let base_digest = ranges[0].1;
                let bytes = read_replace_authorized(
                    &mut authorizer,
                    &mut capability,
                    path,
                    hook,
                    &mut budget,
                    deadline,
                )?;
                let before =
                    before_file(capability.binding(), base_digest, Some(bytes), &mut budget)?;
                let (planned_ranges, content, mode) = validate_replacements(
                    path,
                    &before,
                    before.content().expect("replace captures source"),
                    &ranges,
                    &mut budget,
                    deadline,
                )?;
                let after = planned_file(content, mode, &mut budget, deadline)?;
                effects.push(PlannedEffect::Replace {
                    ranges: planned_ranges,
                    path: clone_path(path, &mut budget)?,
                    before,
                    after: after.clone(),
                });
                expected_paths.push(ExpectedPath::File {
                    path: clone_path(path, &mut budget)?,
                    file: after,
                });
                changed_files.push(clone_path(path, &mut budget)?);
                capabilities.push(
                    authorizer
                        .accept_replace(capability)
                        .map_err(map_path_error)?,
                );
            }
        }
    }

    hook("validation-complete", Path::new(""));
    expected_paths.sort_by(|left, right| left.path().cmp(right.path()));
    changed_files.sort();
    changed_files.dedup();
    authorizer
        .finalize_before(&mut capabilities)
        .map_err(map_path_error)?;
    hook("finalized", Path::new(""));
    check_deadline(deadline)?;
    let digest = plan_digest(
        &revision,
        authority,
        &effects,
        &expected_paths,
        &changed_files,
        &mut budget,
        deadline,
    )?;
    authorizer
        .finalize_before(&mut capabilities)
        .map_err(map_path_error)?;
    check_deadline(deadline)?;
    drop(authorizer);
    let workspace_digest = clone_string(revision.digest().as_str(), &mut budget)?;
    check_deadline(deadline)?;
    Ok(ValidatedTransaction {
        guard,
        capabilities,
        limits,
        revision: revision.id(),
        epoch: revision.epoch(),
        workspace_digest,
        digest,
        effects,
        expected_paths,
        changed_files,
        authority,
    })
}

struct ValidationProfile {
    started: Option<Instant>,
}

impl ValidationProfile {
    fn new() -> Self {
        Self {
            started: (std::env::var("KIT_WORKSPACE_SCAN_PROFILE").as_deref() == Ok("1"))
                .then(Instant::now),
        }
    }
}

impl Drop for ValidationProfile {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            eprintln!(
                "kit_edit_validation elapsed_ms={}",
                started.elapsed().as_millis()
            );
        }
    }
}

type Replacement<'a> = (
    &'a str,
    &'a ContentDigest,
    ByteRange,
    &'a TextContent,
    &'a TextContent,
    ExecutableMode,
);

fn replacement_operations<'a>(
    ir: &'a EditIr,
    path: &RootRelativePath,
    budget: &mut ValidationBudget,
    deadline: Instant,
) -> Result<Vec<Replacement<'a>>, ValidationError> {
    let mut ranges = Vec::new();
    reserve_exact(&mut ranges, ir.operations().len(), budget)?;
    for canonical in ir.operations() {
        check_deadline(deadline)?;
        if let EditOperation::ReplaceRange {
            path: candidate,
            base_digest,
            range,
            expected,
            replacement,
            executable,
        } = canonical.operation()
            && candidate == path
        {
            ranges.push((
                canonical.id(),
                base_digest,
                *range,
                expected,
                replacement,
                *executable,
            ));
        }
    }
    Ok(ranges)
}

fn validate_replacements(
    path: &RootRelativePath,
    before: &BeforeFile,
    bytes: &[u8],
    ranges: &[Replacement<'_>],
    budget: &mut ValidationBudget,
    deadline: Instant,
) -> Result<(Vec<PlannedRange>, Vec<u8>, u32), ValidationError> {
    require_digest(path, ranges[0].1, bytes, deadline)?;
    let facts = text_facts(path, bytes, deadline)?;
    let mut sorted = Vec::new();
    reserve_exact(&mut sorted, ranges.len(), budget)?;
    sorted.extend_from_slice(ranges);
    sorted.sort_by_key(|range| (range.2.start, range.2.end));
    check_deadline(deadline)?;
    let mut planned = Vec::new();
    reserve_exact(&mut planned, sorted.len(), budget)?;
    let mut rendered = Vec::new();
    reserve_exact(&mut rendered, sorted.len(), budget)?;
    let mut output_len = bytes.len();

    for (operation_id, base_digest, range, expected, replacement, _) in &sorted {
        check_deadline(deadline)?;
        require_digest(path, base_digest, bytes, deadline)?;
        let start = usize::try_from(range.start)
            .map_err(|_| ValidationError::RangeOutsideFile(path.clone()))?;
        let end = usize::try_from(range.end)
            .map_err(|_| ValidationError::RangeOutsideFile(path.clone()))?;
        if start > end || end > bytes.len() {
            return Err(ValidationError::RangeOutsideFile(path.clone()));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| ValidationError::InvalidUnicode(path.clone()))?;
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            return Err(ValidationError::InvalidUnicode(path.clone()));
        }
        if expected.newline() != facts.newline || replacement.newline() != facts.newline {
            return Err(ValidationError::NewlineMismatch(path.clone()));
        }
        if start != end && end == bytes.len() && expected.has_final_newline() != facts.final_newline
        {
            return Err(ValidationError::FinalNewlineMismatch(path.clone()));
        }
        let expected_bytes = render(expected, budget, deadline)?;
        if bytes[start..end] != expected_bytes {
            return Err(ValidationError::AnchorMismatch(path.clone()));
        }
        if !expected_bytes.is_empty() && count_occurrences(bytes, &expected_bytes, deadline)? != 1 {
            return Err(ValidationError::AmbiguousAnchor(path.clone()));
        }
        let replacement_bytes = render(replacement, budget, deadline)?;
        output_len = output_len
            .checked_sub(end - start)
            .and_then(|size| size.checked_add(replacement_bytes.len()))
            .ok_or(ValidationError::LimitExceeded(ValidationLimit::Memory))?;
        planned.push(PlannedRange {
            operation_id: clone_string(operation_id, budget)?,
            range: *range,
        });
        rendered.push((start, end, replacement_bytes));
    }
    let mut output = Vec::new();
    reserve_exact(&mut output, output_len, budget)?;
    let mut cursor = 0;
    for (start, end, replacement) in rendered {
        check_deadline(deadline)?;
        output.extend_from_slice(&bytes[cursor..start]);
        output.extend_from_slice(&replacement);
        cursor = end;
    }
    output.extend_from_slice(&bytes[cursor..]);
    check_deadline(deadline)?;
    let mode = match ranges[0].5 {
        ExecutableMode::Preserve => before.mode,
        ExecutableMode::Executable => before.mode | 0o111,
        ExecutableMode::NonExecutable => before.mode & !0o111,
    };
    Ok((planned, output, mode))
}

fn read_delete_authorized<'guard, 'workspace>(
    authorizer: &mut PathAuthorizer<'guard, 'workspace>,
    capability: &mut crate::workspace::path_auth::DeleteSource<'guard, 'workspace>,
    path: &RootRelativePath,
    hook: &mut dyn FnMut(&str, &Path),
    budget: &mut ValidationBudget,
    deadline: Instant,
) -> Result<Vec<u8>, ValidationError> {
    hook("before-read", Path::new(path.as_str()));
    let bytes = authorizer
        .read_delete(
            capability,
            budget.remaining_read(),
            budget.remaining_memory(),
            deadline,
        )
        .map_err(map_path_error)?;
    budget.charge_read(bytes.len())?;
    budget.charge_memory(bytes.len())?;
    hook("after-read", Path::new(path.as_str()));
    Ok(bytes)
}

fn read_move_authorized<'guard, 'workspace>(
    authorizer: &mut PathAuthorizer<'guard, 'workspace>,
    capability: &mut crate::workspace::path_auth::MoveSource<'guard, 'workspace>,
    path: &RootRelativePath,
    hook: &mut dyn FnMut(&str, &Path),
    budget: &mut ValidationBudget,
    deadline: Instant,
) -> Result<Vec<u8>, ValidationError> {
    hook("before-read", Path::new(path.as_str()));
    let bytes = authorizer
        .read_move(
            capability,
            budget.remaining_read(),
            budget.remaining_memory(),
            deadline,
        )
        .map_err(map_path_error)?;
    budget.charge_read(bytes.len())?;
    budget.charge_memory(bytes.len())?;
    hook("after-read", Path::new(path.as_str()));
    Ok(bytes)
}

fn read_replace_authorized<'guard, 'workspace>(
    authorizer: &mut PathAuthorizer<'guard, 'workspace>,
    capability: &mut crate::workspace::path_auth::ReplaceSource<'guard, 'workspace>,
    path: &RootRelativePath,
    hook: &mut dyn FnMut(&str, &Path),
    budget: &mut ValidationBudget,
    deadline: Instant,
) -> Result<Vec<u8>, ValidationError> {
    hook("before-read", Path::new(path.as_str()));
    let bytes = authorizer
        .read_replace(
            capability,
            budget.remaining_read(),
            budget.remaining_memory(),
            deadline,
        )
        .map_err(map_path_error)?;
    budget.charge_read(bytes.len())?;
    budget.charge_memory(bytes.len())?;
    hook("after-read", Path::new(path.as_str()));
    Ok(bytes)
}

fn before_file(
    binding: &CapabilityBinding,
    digest: &ContentDigest,
    content: Option<Vec<u8>>,
    budget: &mut ValidationBudget,
) -> Result<BeforeFile, ValidationError> {
    let identity = binding
        .object_identity()
        .expect("existing capabilities bind an object");
    Ok(BeforeFile {
        digest: clone_digest(digest, budget)?,
        identity,
        mode: identity.mode() & 0o777,
        content: content.map(Arc::new),
    })
}

fn planned_file(
    content: Vec<u8>,
    mode: u32,
    budget: &mut ValidationBudget,
    deadline: Instant,
) -> Result<PlannedFile, ValidationError> {
    budget.charge_memory(std::mem::size_of::<Vec<u8>>() + 2 * std::mem::size_of::<usize>())?;
    Ok(PlannedFile {
        digest: content_digest("blake3", &content, budget, deadline)?,
        content: Arc::new(content),
        mode,
    })
}

fn render(
    content: &TextContent,
    budget: &mut ValidationBudget,
    deadline: Instant,
) -> Result<Vec<u8>, ValidationError> {
    check_deadline(deadline)?;
    let length = content.rendered_len();
    budget.charge_memory(length)?;
    let rendered = content
        .try_render(length)
        .map_err(|_| ValidationError::LimitExceeded(ValidationLimit::Memory))?;
    check_deadline(deadline)?;
    Ok(rendered)
}

fn require_digest(
    path: &RootRelativePath,
    expected: &ContentDigest,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), ValidationError> {
    let (algorithm, expected_hex) = expected
        .as_str()
        .split_once(':')
        .ok_or_else(|| ValidationError::BaseDigestMismatch(path.clone()))?;
    let matches = match algorithm {
        "blake3" => {
            let mut hasher = blake3::Hasher::new();
            for chunk in bytes.chunks(64 * 1024) {
                check_deadline(deadline)?;
                hasher.update(chunk);
            }
            hasher.finalize().to_hex().as_str() == expected_hex
        }
        "sha256" => {
            let mut hasher = Sha256::new();
            for chunk in bytes.chunks(64 * 1024) {
                check_deadline(deadline)?;
                hasher.update(chunk);
            }
            let digest = hasher.finalize();
            expected_hex
                .as_bytes()
                .chunks_exact(2)
                .zip(digest)
                .all(|(encoded, byte)| {
                    u8::from_str_radix(std::str::from_utf8(encoded).unwrap_or(""), 16) == Ok(byte)
                })
        }
        _ => return Err(ValidationError::Unavailable),
    };
    if matches {
        Ok(())
    } else {
        Err(ValidationError::BaseDigestMismatch(path.clone()))
    }
}

fn content_digest(
    algorithm: &str,
    bytes: &[u8],
    budget: &mut ValidationBudget,
    deadline: Instant,
) -> Result<ContentDigest, ValidationError> {
    budget.charge_memory(71)?;
    let mut value = String::new();
    value
        .try_reserve_exact(71)
        .map_err(|_| ValidationError::LimitExceeded(ValidationLimit::Memory))?;
    match algorithm {
        "blake3" => {
            let mut hasher = blake3::Hasher::new();
            for chunk in bytes.chunks(64 * 1024) {
                check_deadline(deadline)?;
                hasher.update(chunk);
            }
            write!(value, "blake3:{}", hasher.finalize().to_hex())
                .map_err(|_| ValidationError::Unavailable)?;
        }
        "sha256" => {
            let mut hasher = Sha256::new();
            for chunk in bytes.chunks(64 * 1024) {
                check_deadline(deadline)?;
                hasher.update(chunk);
            }
            write!(value, "sha256:{:x}", hasher.finalize())
                .map_err(|_| ValidationError::Unavailable)?;
        }
        _ => return Err(ValidationError::Unavailable),
    }
    ContentDigest::from_owned(value).map_err(|_| ValidationError::Unavailable)
}

struct TextFacts {
    newline: Newline,
    final_newline: bool,
}

fn text_facts(
    path: &RootRelativePath,
    bytes: &[u8],
    deadline: Instant,
) -> Result<TextFacts, ValidationError> {
    if bytes.contains(&0) {
        return Err(ValidationError::BinaryFile(path.clone()));
    }
    check_deadline(deadline)?;
    std::str::from_utf8(bytes).map_err(|_| ValidationError::InvalidUnicode(path.clone()))?;
    check_deadline(deadline)?;
    let mut has_lf = false;
    let mut has_crlf = false;
    let mut index = 0;
    while index < bytes.len() {
        if index % (64 * 1024) == 0 {
            check_deadline(deadline)?;
        }
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                has_crlf = true;
                index += 2;
            }
            b'\r' => return Err(ValidationError::NewlineMismatch(path.clone())),
            b'\n' => {
                has_lf = true;
                index += 1;
            }
            _ => index += 1,
        }
    }
    if has_lf && has_crlf {
        return Err(ValidationError::NewlineMismatch(path.clone()));
    }
    Ok(TextFacts {
        newline: if has_crlf { Newline::Crlf } else { Newline::Lf },
        final_newline: bytes.ends_with(b"\n"),
    })
}

fn count_occurrences(
    haystack: &[u8],
    needle: &[u8],
    deadline: Instant,
) -> Result<usize, ValidationError> {
    let mut count = 0;
    for (index, window) in haystack.windows(needle.len()).enumerate() {
        if index % (64 * 1024) == 0 {
            check_deadline(deadline)?;
        }
        if window == needle {
            count += 1;
            if count > 1 {
                break;
            }
        }
    }
    Ok(count)
}

struct ValidationBudget {
    read: usize,
    memory: usize,
    limits: EditLimits,
}

impl ValidationBudget {
    fn new(limits: EditLimits) -> Self {
        Self {
            read: 0,
            memory: 0,
            limits,
        }
    }

    fn charge_read(&mut self, bytes: usize) -> Result<(), ValidationError> {
        self.read = self
            .read
            .checked_add(bytes)
            .ok_or(ValidationError::LimitExceeded(ValidationLimit::ReadBytes))?;
        if self.read > self.limits.max_validation_read_bytes {
            Err(ValidationError::LimitExceeded(ValidationLimit::ReadBytes))
        } else {
            Ok(())
        }
    }

    fn charge_memory(&mut self, bytes: usize) -> Result<(), ValidationError> {
        self.memory = self
            .memory
            .checked_add(bytes)
            .ok_or(ValidationError::LimitExceeded(ValidationLimit::Memory))?;
        if self.memory > self.limits.max_validation_memory_bytes {
            Err(ValidationError::LimitExceeded(ValidationLimit::Memory))
        } else {
            Ok(())
        }
    }

    fn remaining_read(&self) -> usize {
        self.limits.max_validation_read_bytes - self.read
    }

    fn remaining_memory(&self) -> usize {
        self.limits.max_validation_memory_bytes - self.memory
    }
}

fn clone_string(value: &str, budget: &mut ValidationBudget) -> Result<String, ValidationError> {
    budget.charge_memory(value.len())?;
    let mut clone = String::new();
    clone
        .try_reserve_exact(value.len())
        .map_err(|_| ValidationError::LimitExceeded(ValidationLimit::Memory))?;
    clone.push_str(value);
    Ok(clone)
}

fn reserve_exact<T>(
    values: &mut Vec<T>,
    capacity: usize,
    budget: &mut ValidationBudget,
) -> Result<(), ValidationError> {
    budget.charge_memory(
        capacity
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(ValidationError::LimitExceeded(ValidationLimit::Memory))?,
    )?;
    values
        .try_reserve_exact(capacity)
        .map_err(|_| ValidationError::LimitExceeded(ValidationLimit::Memory))
}

fn clone_path(
    path: &RootRelativePath,
    budget: &mut ValidationBudget,
) -> Result<RootRelativePath, ValidationError> {
    RootRelativePath::parse(clone_string(path.as_str(), budget)?, path.as_str().len())
        .map_err(|_| ValidationError::UnsafePath(UnsafePathKind::Invalid))
}

fn clone_digest(
    digest: &ContentDigest,
    budget: &mut ValidationBudget,
) -> Result<ContentDigest, ValidationError> {
    let value = clone_string(digest.as_str(), budget)?;
    ContentDigest::from_owned(value).map_err(|_| ValidationError::Unavailable)
}

fn enforce_active_ir_limits(
    ir: &EditIr,
    limits: EditLimits,
    deadline: Instant,
) -> Result<(), ValidationError> {
    if ir.identity_policy() != limits.identity_policy {
        return Err(ValidationError::IdentityPolicyMismatch);
    }
    if ir.identity_policy() == super::ir::FilesystemIdentityPolicy::CaseSensitive {
        return Err(ValidationError::UnsafePath(UnsafePathKind::Alias));
    }
    if ir.operations().len() > limits.max_operations {
        return Err(ValidationError::LimitExceeded(ValidationLimit::Operations));
    }
    let mut content = 0_usize;
    for canonical in ir.operations() {
        check_deadline(deadline)?;
        let operation = canonical.operation();
        let paths = match operation {
            EditOperation::MoveFile { from, to, .. } => [from, to],
            operation => [operation.primary_path(), operation.primary_path()],
        };
        if paths
            .iter()
            .any(|path| path.as_str().len() > limits.max_path_bytes)
        {
            return Err(ValidationError::LimitExceeded(ValidationLimit::Path));
        }
        let bytes = match operation {
            EditOperation::AddFile { content, .. } => content.rendered_len(),
            EditOperation::ReplaceRange {
                expected,
                replacement,
                ..
            } => expected
                .rendered_len()
                .checked_add(replacement.rendered_len())
                .ok_or(ValidationError::LimitExceeded(ValidationLimit::Content))?,
            EditOperation::DeleteFile { .. } | EditOperation::MoveFile { .. } => 0,
        };
        content = content
            .checked_add(bytes)
            .ok_or(ValidationError::LimitExceeded(ValidationLimit::Content))?;
        if content > limits.max_content_bytes {
            return Err(ValidationError::LimitExceeded(ValidationLimit::Content));
        }
    }
    Ok(())
}

fn check_deadline(deadline: Instant) -> Result<(), ValidationError> {
    if Instant::now() >= deadline {
        Err(ValidationError::LimitExceeded(ValidationLimit::Time))
    } else {
        Ok(())
    }
}

fn map_path_error(error: PathAuthError) -> ValidationError {
    match error {
        PathAuthError::InvalidPath(_) => ValidationError::UnsafePath(UnsafePathKind::Invalid),
        PathAuthError::PrivatePath(_) => ValidationError::UnsafePath(UnsafePathKind::Private),
        PathAuthError::Alias(_) => ValidationError::UnsafePath(UnsafePathKind::Alias),
        PathAuthError::Symlink(_) => ValidationError::UnsafePath(UnsafePathKind::Symlink),
        PathAuthError::Hardlink(_) => ValidationError::UnsafePath(UnsafePathKind::Hardlink),
        PathAuthError::SpecialFile(_) => ValidationError::UnsafePath(UnsafePathKind::Special),
        PathAuthError::MountBoundary(_) => {
            ValidationError::UnsafePath(UnsafePathKind::MountBoundary)
        }
        PathAuthError::NotFound(_)
        | PathAuthError::AlreadyExists(_)
        | PathAuthError::NotDirectory(_)
        | PathAuthError::NotFile(_) => ValidationError::PathStateMismatch,
        PathAuthError::ObjectChanged(_) | PathAuthError::CrossGuard | PathAuthError::CrossRoot => {
            ValidationError::ExternalEdit
        }
        PathAuthError::LimitExceeded(PathAuthLimit::Time) => {
            ValidationError::LimitExceeded(ValidationLimit::Time)
        }
        PathAuthError::LimitExceeded(PathAuthLimit::ReadBytes) => {
            ValidationError::LimitExceeded(ValidationLimit::ReadBytes)
        }
        PathAuthError::LimitExceeded(_) => {
            ValidationError::LimitExceeded(ValidationLimit::Authorization)
        }
        PathAuthError::StaleEpoch { .. } => ValidationError::StaleRevision,
        PathAuthError::Revision(error) => map_revision_error(error),
        PathAuthError::WrongAuthority { .. }
        | PathAuthError::Unavailable { .. }
        | PathAuthError::Io { .. } => ValidationError::Unavailable,
    }
}

fn map_revision_error(error: RevisionError) -> ValidationError {
    match error {
        RevisionError::StaleRevision { .. } => ValidationError::StaleRevision,
        RevisionError::ScanRace { .. } => ValidationError::ExternalEdit,
        RevisionError::Symlink(_) => ValidationError::UnsafePath(UnsafePathKind::Symlink),
        RevisionError::Hardlink(_) => ValidationError::UnsafePath(UnsafePathKind::Hardlink),
        RevisionError::UnsupportedEntry(_) => ValidationError::UnsafePath(UnsafePathKind::Special),
        RevisionError::MountBoundary(_) => {
            ValidationError::UnsafePath(UnsafePathKind::MountBoundary)
        }
        RevisionError::UnsafePath(_) => ValidationError::UnsafePath(UnsafePathKind::Invalid),
        RevisionError::LimitExceeded(LimitKind::Time) => {
            ValidationError::LimitExceeded(ValidationLimit::Time)
        }
        RevisionError::LimitExceeded(_) => {
            ValidationError::LimitExceeded(ValidationLimit::Authorization)
        }
        RevisionError::NotFound(_) | RevisionError::NotDirectory(_) => {
            ValidationError::PathStateMismatch
        }
        RevisionError::CorruptMetadata
        | RevisionError::Unavailable { .. }
        | RevisionError::Io { .. }
        | RevisionError::InvalidRange(_) => ValidationError::Unavailable,
    }
}

fn plan_digest(
    revision: &Revision,
    authority: Option<AuthenticatedEditAuthority>,
    effects: &[PlannedEffect],
    expected_paths: &[ExpectedPath],
    changed_files: &[RootRelativePath],
    budget: &mut ValidationBudget,
    deadline: Instant,
) -> Result<String, ValidationError> {
    let mut hasher = blake3::Hasher::new();
    frame(&mut hasher, b"kit-validated-edit-plan-v1");
    let revision_id = hex_id::<32, 66>(b'r', revision.id().as_bytes());
    let epoch_id = hex_id::<16, 34>(b'e', revision.epoch().as_bytes());
    frame(&mut hasher, &revision_id);
    frame(&mut hasher, &epoch_id);
    frame(&mut hasher, revision.digest().as_str().as_bytes());
    if let Some(authority) = authority {
        frame(&mut hasher, authority.principal().to_string().as_bytes());
        frame(&mut hasher, authority.project().to_string().as_bytes());
    } else {
        frame(&mut hasher, b"unauthenticated");
    }
    for effect in effects {
        check_deadline(deadline)?;
        match effect {
            PlannedEffect::Add {
                operation_id,
                path,
                after,
            } => {
                frame(&mut hasher, b"add");
                frame(&mut hasher, operation_id.as_bytes());
                frame(&mut hasher, path.as_str().as_bytes());
                hash_file(&mut hasher, after, deadline)?;
            }
            PlannedEffect::Delete {
                operation_id,
                path,
                before,
            } => {
                frame(&mut hasher, b"delete");
                frame(&mut hasher, operation_id.as_bytes());
                frame(&mut hasher, path.as_str().as_bytes());
                hash_before(&mut hasher, before);
            }
            PlannedEffect::Move {
                operation_id,
                from,
                to,
                before,
                after,
            } => {
                frame(&mut hasher, b"move");
                frame(&mut hasher, operation_id.as_bytes());
                frame(&mut hasher, from.as_str().as_bytes());
                frame(&mut hasher, to.as_str().as_bytes());
                hash_before(&mut hasher, before);
                hash_file(&mut hasher, after, deadline)?;
            }
            PlannedEffect::Replace {
                ranges,
                path,
                before,
                after,
            } => {
                frame(&mut hasher, b"replace");
                frame(&mut hasher, path.as_str().as_bytes());
                hash_before(&mut hasher, before);
                for range in ranges {
                    frame(&mut hasher, range.operation_id.as_bytes());
                    hasher.update(&range.range.start.to_le_bytes());
                    hasher.update(&range.range.end.to_le_bytes());
                }
                hash_file(&mut hasher, after, deadline)?;
            }
        }
    }
    for expected in expected_paths {
        check_deadline(deadline)?;
        match expected {
            ExpectedPath::Absent(path) => {
                frame(&mut hasher, b"absent");
                frame(&mut hasher, path.as_str().as_bytes());
            }
            ExpectedPath::File { path, file } => {
                frame(&mut hasher, b"file");
                frame(&mut hasher, path.as_str().as_bytes());
                hash_file(&mut hasher, file, deadline)?;
            }
        }
    }
    for path in changed_files {
        check_deadline(deadline)?;
        frame(&mut hasher, path.as_str().as_bytes());
    }
    check_deadline(deadline)?;
    budget.charge_memory(71)?;
    let mut digest = String::new();
    digest
        .try_reserve_exact(71)
        .map_err(|_| ValidationError::LimitExceeded(ValidationLimit::Memory))?;
    write!(digest, "blake3:{}", hasher.finalize().to_hex())
        .map_err(|_| ValidationError::Unavailable)?;
    Ok(digest)
}

fn hash_before(hasher: &mut blake3::Hasher, file: &BeforeFile) {
    frame(hasher, file.digest.as_str().as_bytes());
    hasher.update(&file.identity.device().to_le_bytes());
    hasher.update(&file.identity.inode().to_le_bytes());
    hasher.update(&[match file.identity.entry_type() {
        EntryType::Directory => 0,
        EntryType::RegularFile => 1,
    }]);
    hasher.update(&file.identity.mode().to_le_bytes());
    hasher.update(&file.mode.to_le_bytes());
}

fn hash_file(
    hasher: &mut blake3::Hasher,
    file: &PlannedFile,
    deadline: Instant,
) -> Result<(), ValidationError> {
    frame(hasher, file.digest.as_str().as_bytes());
    hasher.update(&file.mode.to_le_bytes());
    hasher.update(&(file.content.len() as u64).to_le_bytes());
    for chunk in file.content.chunks(64 * 1024) {
        check_deadline(deadline)?;
        hasher.update(chunk);
    }
    Ok(())
}

fn frame(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn hex_id<const N: usize, const M: usize>(prefix: u8, bytes: &[u8; N]) -> [u8; M] {
    assert_eq!(M, N * 2 + 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = [0_u8; M];
    output[0] = prefix;
    output[1] = b':';
    for (index, byte) in bytes.iter().copied().enumerate() {
        output[2 + index * 2] = HEX[(byte >> 4) as usize];
        output[3 + index * 2] = HEX[(byte & 0x0f) as usize];
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::edit::ir::RevisionToken;

    #[test]
    fn expired_deadline_stops_ir_scans() {
        let limits = EditLimits::default();
        let path = RootRelativePath::parse("file", limits.max_path_bytes).unwrap();
        let ir = EditIr::new(
            RevisionToken::parse(format!("r:{}", "0".repeat(64))).unwrap(),
            vec![EditOperation::ReplaceRange {
                path: path.clone(),
                base_digest: ContentDigest::parse(&format!("blake3:{}", "0".repeat(64))).unwrap(),
                range: ByteRange::new(0, 0).unwrap(),
                expected: TextContent::empty(Newline::Lf),
                replacement: TextContent::empty(Newline::Lf),
                executable: ExecutableMode::Preserve,
            }],
            limits,
        )
        .unwrap();

        assert!(matches!(
            enforce_active_ir_limits(&ir, limits, Instant::now()),
            Err(ValidationError::LimitExceeded(ValidationLimit::Time))
        ));
        let mut budget = ValidationBudget::new(limits);
        assert!(matches!(
            replacement_operations(&ir, &path, &mut budget, Instant::now()),
            Err(ValidationError::LimitExceeded(ValidationLimit::Time))
        ));
    }

    #[test]
    fn reservation_budget_fails_before_allocation() {
        let limits = EditLimits {
            max_validation_memory_bytes: std::mem::size_of::<u64>() - 1,
            ..EditLimits::default()
        };
        let mut budget = ValidationBudget::new(limits);
        let mut values = Vec::<u64>::new();

        assert!(matches!(
            reserve_exact(&mut values, 1, &mut budget),
            Err(ValidationError::LimitExceeded(ValidationLimit::Memory))
        ));
        assert_eq!(values.capacity(), 0);
    }

    #[test]
    fn edit_authority_requires_the_authenticated_project_write_grant() {
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let other_project = ProjectId::generate().unwrap();
        let read = GrantSnapshot::new(principal, project, [Grant::WorkspaceRead]);
        let authenticated = AuthenticatedPrincipal::from_grants(read.clone());
        assert!(
            AuthenticatedEditAuthority::from_authenticated(&authenticated, &read, project).is_err()
        );

        let write = GrantSnapshot::new(principal, project, [Grant::WorkspaceWrite]);
        let authenticated = AuthenticatedPrincipal::from_grants(write.clone());
        assert!(
            AuthenticatedEditAuthority::from_authenticated(&authenticated, &write, other_project)
                .is_err()
        );
        assert!(
            AuthenticatedEditAuthority::from_authenticated(&authenticated, &write, project).is_ok()
        );
    }
}
