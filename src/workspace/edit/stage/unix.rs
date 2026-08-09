use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CStr, CString, OsString},
    fmt,
    fs::File,
    io::{self, Read, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStringExt, fs::MetadataExt as _},
    },
    path::{Path, PathBuf},
    sync::Mutex,
    time::Instant,
};

use sha2::{Digest as _, Sha256};

use super::{StageError, StageLimit, StageLimits, StagedOperation, SyntaxRequirements, change};
use crate::{
    executor::syntax::{SyntaxExecutor, SyntaxExecutorError},
    workspace::{
        edit::{
            format::{
                NATIVE_JSON_VERSION, NATIVE_TEXT_VERSION, RUST_GRAMMAR_VERSION, SyntaxRequest,
                SyntaxRequirement, SyntaxStatus,
            },
            ir::RootRelativePath,
            validate::{
                AuthenticatedEditAuthority, ExpectedPath, PlanConsumption, PlannedEffect,
                ValidatedPlan, ValidationError, ValidationLimit,
            },
        },
        path_auth::AcceptedPathCapability,
        revision::{LimitKind, RevisionError, RevisionId, WorkspaceMutationGuard},
    },
};

const MARKER_NAME: &str = ".kit-stage-marker";
const CLEANUP_QUEUE_NAME: &CStr = c".kit-stage-cleanup.queue";
const CLEANUP_QUEUE_LIMIT: usize = 1024 * 1024;
const CLEANUP_RECORD_LIMIT: usize = 1024;

pub struct StagedEdit<'workspace> {
    allocation: Allocation,
    _guard: WorkspaceMutationGuard<'workspace>,
    _capabilities: Vec<AcceptedPathCapability>,
    _binding: [u8; 32],
    revision: RevisionId,
    plan_digest: String,
    state_digest: String,
    evidence_digest: String,
    workspace_digest: String,
    changes: Vec<super::StageChange>,
    operations: Vec<StagedOperation>,
    expected_change_diff_digest: Option<String>,
    final_snapshot: Snapshot,
    limits: StageLimits,
    authority: Option<AuthenticatedEditAuthority>,
    operation_context: crate::workspace::edit::validate::EditOperationContext,
}

impl<'workspace> StagedEdit<'workspace> {
    pub fn revision(&self) -> RevisionId {
        self.revision
    }

    pub fn plan_digest(&self) -> &str {
        &self.plan_digest
    }

    pub fn digest(&self) -> &str {
        &self.state_digest
    }

    pub fn state_digest(&self) -> &str {
        &self.state_digest
    }

    pub fn evidence_digest(&self) -> &str {
        &self.evidence_digest
    }

    pub fn operation_context(&self) -> &crate::workspace::edit::validate::EditOperationContext {
        &self.operation_context
    }

    pub(crate) fn workspace_digest(&self) -> &str {
        &self.workspace_digest
    }

    pub(crate) fn operations(&self) -> &[StagedOperation] {
        &self.operations
    }

    pub(crate) fn expected_change_diff_digest(&self) -> Option<&str> {
        self.expected_change_diff_digest.as_deref()
    }

    pub(crate) fn final_root(&self) -> &File {
        &self.allocation.final_view
    }

    pub(crate) fn guard_mut(&mut self) -> &mut WorkspaceMutationGuard<'workspace> {
        &mut self._guard
    }

    pub(crate) fn authority(&self) -> Option<AuthenticatedEditAuthority> {
        self.authority
    }

    pub(crate) fn base_epoch(&self) -> crate::workspace::revision::EpochId {
        self._guard.revision().epoch()
    }

    pub(crate) fn base_workspace_digest(&self) -> &str {
        self._guard.revision().digest().as_str()
    }

    pub(crate) fn cleanup(&mut self) -> Result<(), StageError> {
        self.allocation
            .cleanup()
            .map_err(|_| StageError::CleanupFailed)?;
        self.allocation.cleaned = true;
        Ok(())
    }

    pub fn changes(&self) -> &[super::StageChange] {
        &self.changes
    }

    pub fn read_file(
        &self,
        path: &RootRelativePath,
        max_bytes: usize,
    ) -> Result<Vec<u8>, StageError> {
        let deadline = Instant::now()
            .checked_add(self.limits.max_time)
            .ok_or(StageError::LimitExceeded(StageLimit::Time))?;
        self.read_file_before(path, max_bytes, deadline)
    }

    pub fn read_file_before(
        &self,
        path: &RootRelativePath,
        max_bytes: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>, StageError> {
        check_deadline(deadline)?;
        let expected = self
            .final_snapshot
            .entries
            .get(Path::new(path.as_str()))
            .ok_or(StageError::StageChanged)?;
        check_deadline(deadline)?;
        let file = open_relative_file(&self.allocation.final_view, path, libc::O_RDONLY)?;
        check_deadline(deadline)?;
        let before = stat_file(&file).map_err(|_| StageError::StageChanged)?;
        check_deadline(deadline)?;
        if expected.kind != Kind::File
            || before.kind() != libc::S_IFREG as u32
            || before.links != 1
            || !supported_metadata(&file, before)
            || before.mode & 0o777 != 0o400
            || before.size != expected.size
            || before.size > max_bytes.min(self.limits.max_file_bytes) as u64
        {
            return Err(StageError::StageChanged);
        }
        let bytes = read_file_checked(
            file,
            before,
            max_bytes.min(self.limits.max_file_bytes),
            deadline,
        )?;
        check_deadline(deadline)?;
        if expected.digest != *blake3::hash(&bytes).as_bytes() {
            return Err(StageError::StageChanged);
        }
        check_deadline(deadline)?;
        Ok(bytes)
    }

    pub fn close(mut self) -> Result<(), StageError> {
        self.cleanup()
    }
}

pub fn stage<'workspace>(
    plan: ValidatedPlan<'workspace>,
    limits: StageLimits,
    syntax: SyntaxRequirements<'_>,
    syntax_executors: &mut [&mut SyntaxExecutor],
) -> Result<StagedEdit<'workspace>, StageError> {
    validate_limits(limits)?;
    let deadline = Instant::now()
        .checked_add(limits.max_time)
        .ok_or(StageError::LimitExceeded(StageLimit::Time))?;
    let mut budget = Budget::new(limits, deadline);
    let mut plan = plan.consume_before(deadline).map_err(map_validation)?;
    let (source, _workspace_path) = plan
        .guard
        .path_authorization_root()
        .map_err(|_| StageError::Unavailable)?;
    let (stage_root, stage_root_path) = plan
        .guard
        .stage_allocation_root()
        .map_err(|_| StageError::Unavailable)?;
    let binding = plan.guard.stage_binding(&plan.digest);
    let mut stage_fence = plan
        .guard
        .path_authorization_fence(limits.max_metadata_bytes)
        .map_err(map_fence_error)?;
    let allocation = Allocation::new(stage_root, stage_root_path)?;
    watch_stage_tree(
        &allocation.root_path,
        &allocation.root,
        &mut stage_fence,
        false,
    )?;
    stage_fence
        .reset_after_verified_read()
        .map_err(|_| StageError::StageChanged)?;
    copy_tree(&source, &allocation.view, &mut budget)?;
    plan.revalidate_before(deadline).map_err(map_validation)?;

    watch_stage_tree(
        &allocation.root_path,
        &allocation.root,
        &mut stage_fence,
        false,
    )?;
    stage_fence
        .reset_after_verified_read()
        .map_err(|_| StageError::StageChanged)?;
    let initial = snapshot_tree(&allocation.view, &mut budget)?;
    stage_fence
        .ensure_clean()
        .map_err(|_| StageError::StageChanged)?;
    apply_plan(&allocation.view, &plan, limits, deadline, &mut budget)?;
    watch_stage_tree(
        &allocation.root_path,
        &allocation.root,
        &mut stage_fence,
        false,
    )?;
    stage_fence
        .reset_after_verified_read()
        .map_err(|_| StageError::StageChanged)?;
    let mut current = snapshot_tree(&allocation.view, &mut budget)?;
    require_exact_changed_set(&initial, &current, &plan.changed_files)?;
    verify_expected_paths(
        &allocation.view,
        &plan.expected_paths,
        limits,
        deadline,
        &mut budget,
    )?;
    run_syntax(
        &allocation.view,
        syntax,
        syntax_executors,
        &plan.changed_files,
        &current,
        limits,
        deadline,
        &mut budget,
    )?;
    stage_fence
        .ensure_clean()
        .map_err(|_| StageError::StageChanged)?;

    let final_source = &allocation.view;
    let expected_final = snapshot_tree(final_source, &mut budget)?;
    stage_fence
        .ensure_clean()
        .map_err(|_| StageError::StageChanged)?;
    copy_tree(final_source, &allocation.final_view, &mut budget)?;
    apply_snapshot_modes(&allocation.final_view, &expected_final)?;
    freeze_tree(&allocation.final_view)?;
    watch_stage_tree(
        &allocation.root_path,
        &allocation.root,
        &mut stage_fence,
        true,
    )?;
    verify_frozen_tree(&allocation.final_view, &expected_final, &mut budget)?;
    stage_fence
        .ensure_clean()
        .map_err(|_| StageError::StageChanged)?;
    let final_baseline = snapshot_tree(&allocation.final_view, &mut budget)?;
    stage_fence
        .ensure_clean()
        .map_err(|_| StageError::StageChanged)?;
    let final_generation = stage_fence.generation();
    current = expected_final;
    run_syntax(
        &allocation.final_view,
        syntax,
        syntax_executors,
        &plan.changed_files,
        &current,
        limits,
        deadline,
        &mut budget,
    )?;

    plan.revalidate_before(deadline).map_err(map_validation)?;
    require_exact_changed_set(&initial, &current, &plan.changed_files)?;
    let changes = stage_changes(&initial, &current, &plan.changed_files)?;
    let syntax_digest_requirements =
        derive_syntax_requirements(syntax, &plan.changed_files, &current)?;
    let state_digest = stage_state_digest(
        &plan,
        &current,
        &changes,
        &syntax_digest_requirements,
        limits,
    );
    let evidence_digest = stage_evidence_digest(&state_digest);
    let workspace_digest = workspace_content_digest(&allocation.final_view, &current, deadline)?;
    let operations = staged_operations(&plan.effects);
    verify_quiescent_tree(
        &allocation.final_view,
        &final_baseline,
        final_generation,
        &mut stage_fence,
        &mut budget,
    )?;
    Ok(StagedEdit {
        allocation,
        _guard: plan.guard,
        _capabilities: plan.capabilities,
        _binding: binding,
        revision: plan.revision,
        plan_digest: plan.digest,
        state_digest,
        evidence_digest,
        workspace_digest,
        changes,
        operations,
        expected_change_diff_digest: plan.expected_change_diff_digest,
        final_snapshot: current,
        limits,
        authority: plan.authority,
        operation_context: plan.operation_context,
    })
}

fn staged_operations(effects: &[PlannedEffect]) -> Vec<StagedOperation> {
    effects
        .iter()
        .map(|effect| match effect {
            PlannedEffect::Add { path, .. } => StagedOperation::Add(path.clone()),
            PlannedEffect::Delete { path, .. } => StagedOperation::Delete(path.clone()),
            PlannedEffect::Move { from, to, .. } => StagedOperation::Move {
                from: from.clone(),
                to: to.clone(),
            },
            PlannedEffect::Replace { path, .. } => StagedOperation::Replace(path.clone()),
        })
        .collect()
}

fn workspace_content_digest(
    root: &File,
    snapshot: &Snapshot,
    deadline: Instant,
) -> Result<String, StageError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"kit-workspace-content-v1\0");
    for (path, state) in &snapshot.entries {
        hasher.update(&[if state.kind == Kind::Directory {
            b'd'
        } else {
            b'f'
        }]);
        frame(&mut hasher, path.as_os_str().as_encoded_bytes());
        hasher.update(&[u8::from(
            state.kind == Kind::File && state.mode & 0o111 != 0,
        )]);
        hasher.update(&state.size.to_le_bytes());
        if state.kind == Kind::File {
            let path = path.to_str().ok_or(StageError::StageChanged)?;
            let path =
                RootRelativePath::parse(path, usize::MAX).map_err(|_| StageError::StageChanged)?;
            let bytes = read_relative(root, &path, usize::MAX, deadline)?;
            if *blake3::hash(&bytes).as_bytes() != state.digest {
                return Err(StageError::StageChanged);
            }
            hasher.update(&bytes);
        }
    }
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

fn validate_limits(limits: StageLimits) -> Result<(), StageError> {
    if limits.max_entries == 0 {
        return Err(StageError::LimitExceeded(StageLimit::Entries));
    }
    if limits.max_total_bytes == 0 {
        return Err(StageError::LimitExceeded(StageLimit::TotalBytes));
    }
    if limits.max_file_bytes == 0 {
        return Err(StageError::LimitExceeded(StageLimit::FileBytes));
    }
    if limits.max_syntax_output_bytes == 0 {
        return Err(StageError::LimitExceeded(StageLimit::SyntaxOutput));
    }
    if limits.max_name_bytes == 0 {
        return Err(StageError::LimitExceeded(StageLimit::NameBytes));
    }
    if limits.max_path_bytes == 0 {
        return Err(StageError::LimitExceeded(StageLimit::PathBytes));
    }
    if limits.max_metadata_bytes == 0 {
        return Err(StageError::LimitExceeded(StageLimit::MetadataMemory));
    }
    if limits.max_time.is_zero() {
        return Err(StageError::LimitExceeded(StageLimit::Time));
    }
    Ok(())
}

fn map_validation(error: ValidationError) -> StageError {
    if error == ValidationError::LimitExceeded(ValidationLimit::Time) {
        StageError::LimitExceeded(StageLimit::Time)
    } else {
        StageError::Validation(error)
    }
}

fn map_fence_error(error: RevisionError) -> StageError {
    if matches!(error, RevisionError::LimitExceeded(LimitKind::Memory)) {
        StageError::LimitExceeded(StageLimit::MetadataMemory)
    } else {
        StageError::Unavailable
    }
}

fn apply_plan(
    root: &File,
    plan: &PlanConsumption<'_>,
    limits: StageLimits,
    deadline: Instant,
    budget: &mut Budget,
) -> Result<(), StageError> {
    for effect in &plan.effects {
        check_deadline(deadline)?;
        match effect {
            PlannedEffect::Add { path, after, .. } => {
                budget.file(after.content().len() as u64)?;
                write_relative(root, path, after.content(), after.mode(), true, limits)?;
            }
            PlannedEffect::Delete { path, before, .. } => {
                verify_before(
                    root,
                    path,
                    before.digest().as_str(),
                    before.mode(),
                    limits,
                    budget,
                )?;
                unlink_relative(root, path)?;
            }
            PlannedEffect::Move {
                from,
                to,
                before,
                after,
                ..
            } => {
                verify_before(
                    root,
                    from,
                    before.digest().as_str(),
                    before.mode(),
                    limits,
                    budget,
                )?;
                unlink_relative(root, from)?;
                budget.file(after.content().len() as u64)?;
                write_relative(root, to, after.content(), after.mode(), true, limits)?;
            }
            PlannedEffect::Replace {
                path,
                before,
                after,
                ..
            } => {
                verify_before(
                    root,
                    path,
                    before.digest().as_str(),
                    before.mode(),
                    limits,
                    budget,
                )?;
                budget.file(after.content().len() as u64)?;
                write_relative(root, path, after.content(), after.mode(), false, limits)?;
            }
        }
    }
    Ok(())
}

fn verify_before(
    root: &File,
    path: &RootRelativePath,
    expected_digest: &str,
    expected_mode: u32,
    limits: StageLimits,
    budget: &mut Budget,
) -> Result<(), StageError> {
    let file = open_relative_file(root, path, libc::O_RDONLY)?;
    let before = stat_file(&file).map_err(|_| StageError::StageChanged)?;
    if before.kind() != libc::S_IFREG as u32
        || before.links != 1
        || !supported_metadata(&file, before)
        || before.mode & 0o777 != expected_mode
        || before.size > limits.max_file_bytes as u64
    {
        return Err(StageError::PlanMismatch);
    }
    budget.file(before.size)?;
    let bytes = read_file_checked(file, before, limits.max_file_bytes, budget.deadline)?;
    if !digest_matches(expected_digest, &bytes) {
        return Err(StageError::PlanMismatch);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_syntax(
    root: &File,
    requirements: SyntaxRequirements<'_>,
    executors: &mut [&mut SyntaxExecutor],
    changed_files: &[RootRelativePath],
    snapshot: &Snapshot,
    limits: StageLimits,
    deadline: Instant,
    budget: &mut Budget,
) -> Result<(), StageError> {
    let requirements = derive_syntax_requirements(requirements, changed_files, snapshot)?;
    for requirement in &requirements {
        check_deadline(deadline)?;
        if changed_files.binary_search(requirement.path()).is_err() {
            return Err(StageError::PlanMismatch);
        }
        let state = snapshot
            .entries
            .get(Path::new(requirement.path().as_str()))
            .ok_or(StageError::PlanMismatch)?;
        budget.file(state.size)?;
        budget.metadata(
            usize::try_from(state.size)
                .map_err(|_| StageError::LimitExceeded(StageLimit::MetadataMemory))?,
        )?;
        let source = match read_relative(root, requirement.path(), limits.max_file_bytes, deadline)
        {
            Ok(source) => source,
            Err(StageError::LimitExceeded(StageLimit::Time)) => {
                return Err(StageError::LimitExceeded(StageLimit::Time));
            }
            Err(_) => return Err(StageError::SyntaxFailed(requirement.path().clone())),
        };
        let mut matches = executors.iter_mut().filter(|executor| {
            executor.language() == requirement.language()
                && executor.version() == requirement.version()
        });
        let first = matches.next();
        let status = if matches.next().is_some() {
            None
        } else {
            let mut production;
            let executor = match first {
                Some(executor) => executor,
                None => {
                    production =
                        SyntaxExecutor::production(requirement.language(), requirement.version());
                    &mut production
                }
            };
            match executor.execute(
                SyntaxRequest::new(requirement.path(), &source),
                limits.max_metadata_bytes,
                limits.max_syntax_output_bytes,
                deadline,
            ) {
                Ok(completion)
                    if completion.authoritative()
                        && completion.contract_version()
                            == crate::workspace::edit::format::SYNTAX_EXECUTOR_CONTRACT_VERSION =>
                {
                    Some(completion.status())
                }
                Ok(_) | Err(SyntaxExecutorError::Unavailable | SyntaxExecutorError::Rejected) => {
                    None
                }
                Err(SyntaxExecutorError::Timeout) => {
                    return Err(StageError::SyntaxTimeout(requirement.path().clone()));
                }
            }
        };
        match status {
            Some(SyntaxStatus::Pass) => {}
            Some(SyntaxStatus::Fail) => {
                return Err(StageError::SyntaxFailed(requirement.path().clone()));
            }
            Some(SyntaxStatus::Unavailable) | None if requirement.required() => {
                return Err(StageError::SyntaxUnavailable(requirement.path().clone()));
            }
            Some(SyntaxStatus::Unavailable) | None => {}
        }
    }
    Ok(())
}

fn derive_syntax_requirements(
    configured: SyntaxRequirements<'_>,
    changed_files: &[RootRelativePath],
    snapshot: &Snapshot,
) -> Result<Vec<SyntaxRequirement>, StageError> {
    let mut derived = Vec::new();
    derived
        .try_reserve(changed_files.len())
        .map_err(|_| StageError::LimitExceeded(StageLimit::Entries))?;
    for path in changed_files {
        if !matches!(
            snapshot.entries.get(Path::new(path.as_str())),
            Some(state) if state.kind == Kind::File
        ) {
            continue;
        }
        let extension = Path::new(path.as_str())
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("");
        let native = match extension {
            "json" => Some(("json", NATIVE_JSON_VERSION)),
            "md" | "txt" => Some(("text", NATIVE_TEXT_VERSION)),
            _ => None,
        };
        if let Some((language, version)) = native {
            derived.push(
                SyntaxRequirement::new(path.clone(), language, version, true)
                    .map_err(|_| StageError::SyntaxUnavailable(path.clone()))?,
            );
            continue;
        }
        let (language, version) = match extension {
            "rs" => ("rust", RUST_GRAMMAR_VERSION),
            "sh" => ("shell", "kit-tree-sitter-shell-v1"),
            "js" | "jsx" => ("javascript", "kit-tree-sitter-javascript-v1"),
            "ts" | "tsx" => ("typescript", "kit-tree-sitter-typescript-v1"),
            "py" => ("python", "kit-tree-sitter-python-v1"),
            "go" => ("go", "kit-tree-sitter-go-v1"),
            "c" | "h" => ("c", "kit-tree-sitter-c-v1"),
            "cc" | "cpp" | "cxx" | "hpp" => ("cpp", "kit-tree-sitter-cpp-v1"),
            "java" => ("java", "kit-tree-sitter-java-v1"),
            "rb" => ("ruby", "kit-tree-sitter-ruby-v1"),
            "php" => ("php", "kit-tree-sitter-php-v1"),
            "swift" => ("swift", "kit-tree-sitter-swift-v1"),
            "kt" | "kts" => ("kotlin", "kit-tree-sitter-kotlin-v1"),
            "cs" => ("c-sharp", "kit-tree-sitter-c-sharp-v1"),
            "toml" => ("toml", "kit-tree-sitter-toml-v1"),
            "yaml" | "yml" => ("yaml", "kit-tree-sitter-yaml-v1"),
            _ => return Err(StageError::SyntaxUnavailable(path.clone())),
        };
        let Some(requirement) = configured.iter().find(|requirement| {
            requirement.path() == path
                && requirement.language() == language
                && requirement.version() == version
                && requirement.required()
        }) else {
            return Err(StageError::SyntaxUnavailable(path.clone()));
        };
        derived.push(requirement.clone());
    }
    if configured.iter().any(|configured| {
        derived.iter().all(|requirement| {
            requirement.path() != configured.path()
                || requirement.language() != configured.language()
                || requirement.version() != configured.version()
        })
    }) {
        return Err(StageError::PlanMismatch);
    }
    Ok(derived)
}

fn verify_expected_paths(
    root: &File,
    expected: &[ExpectedPath],
    limits: StageLimits,
    deadline: Instant,
    budget: &mut Budget,
) -> Result<(), StageError> {
    for expected in expected {
        check_deadline(deadline)?;
        match expected {
            ExpectedPath::Absent(path) => {
                let (parent, leaf) = open_parent(root, path)?;
                match stat_at(&parent, &leaf) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    _ => return Err(StageError::PlanMismatch),
                }
            }
            ExpectedPath::File { path, file } => {
                let descriptor = open_relative_file(root, path, libc::O_RDONLY)?;
                let state = stat_file(&descriptor).map_err(|_| StageError::StageChanged)?;
                if state.kind() != libc::S_IFREG as u32
                    || state.links != 1
                    || state.mode & 0o777 != file.mode()
                    || state.mode & 0o7000 != 0
                    || state.size > limits.max_file_bytes as u64
                {
                    return Err(StageError::PlanMismatch);
                }
                budget.file(state.size)?;
                let bytes = read_file_checked(descriptor, state, limits.max_file_bytes, deadline)?;
                check_deadline(deadline)?;
                if !digest_matches(file.digest().as_str(), &bytes) {
                    return Err(StageError::PlanMismatch);
                }
            }
        }
    }
    Ok(())
}

fn snapshot_digest(snapshot: &Snapshot) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"kit-stage-tree-v2");
    frame(&mut hasher, b"reject-acl-xattr-v1");
    for (path, state) in &snapshot.entries {
        frame(&mut hasher, path.as_os_str().as_encoded_bytes());
        hasher.update(&[match state.kind {
            Kind::Directory => 0,
            Kind::File => 1,
        }]);
        hasher.update(&state.mode.to_le_bytes());
        hasher.update(&state.size.to_le_bytes());
        hasher.update(&state.digest);
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn require_exact_changed_set(
    before: &Snapshot,
    after: &Snapshot,
    expected: &[RootRelativePath],
) -> Result<(), StageError> {
    let actual = differing_paths(before, after);
    if actual.len() != expected.len()
        || actual
            .iter()
            .zip(expected)
            .any(|(actual, expected)| actual != Path::new(expected.as_str()))
    {
        return Err(StageError::PlanMismatch);
    }
    Ok(())
}

fn stage_changes(
    before: &Snapshot,
    after: &Snapshot,
    paths: &[RootRelativePath],
) -> Result<Vec<super::StageChange>, StageError> {
    paths
        .iter()
        .map(|path| {
            let before = before.entries.get(Path::new(path.as_str()));
            let after = after.entries.get(Path::new(path.as_str()));
            if before.is_some_and(|state| state.kind != Kind::File)
                || after.is_some_and(|state| state.kind != Kind::File)
            {
                return Err(StageError::PlanMismatch);
            }
            Ok(change(
                path.clone(),
                before.map(FileState::digest_string),
                after.map(FileState::digest_string),
                before.map(|state| state.mode),
                after.map(|state| state.mode),
            ))
        })
        .collect()
}

fn stage_state_digest(
    plan: &PlanConsumption<'_>,
    final_tree: &Snapshot,
    changes: &[super::StageChange],
    syntax: &[SyntaxRequirement],
    limits: StageLimits,
) -> String {
    let mut hasher = blake3::Hasher::new();
    frame(&mut hasher, b"kit-staged-edit-state-v2");
    frame(&mut hasher, plan.digest.as_bytes());
    frame(&mut hasher, plan.revision.to_string().as_bytes());
    frame(&mut hasher, plan.epoch.to_string().as_bytes());
    frame(&mut hasher, plan.workspace_digest.as_bytes());
    frame(&mut hasher, snapshot_digest(final_tree).as_bytes());
    hasher.update(&(limits.max_entries as u64).to_le_bytes());
    hasher.update(&limits.max_total_bytes.to_le_bytes());
    hasher.update(&(limits.max_file_bytes as u64).to_le_bytes());
    hasher.update(&(limits.max_syntax_output_bytes as u64).to_le_bytes());
    hasher.update(&(limits.max_name_bytes as u64).to_le_bytes());
    hasher.update(&(limits.max_path_bytes as u64).to_le_bytes());
    hasher.update(&(limits.max_metadata_bytes as u64).to_le_bytes());
    hasher.update(&(limits.max_time.as_nanos().min(u128::from(u64::MAX)) as u64).to_le_bytes());
    for change in changes {
        frame(&mut hasher, change.path().as_str().as_bytes());
        frame(&mut hasher, change.before_hash().unwrap_or("").as_bytes());
        frame(&mut hasher, change.after_hash().unwrap_or("").as_bytes());
        hasher.update(&change.before_mode().unwrap_or(0).to_le_bytes());
        hasher.update(&change.after_mode().unwrap_or(0).to_le_bytes());
    }
    for requirement in syntax {
        frame(&mut hasher, requirement.path().as_str().as_bytes());
        frame(&mut hasher, requirement.language().as_bytes());
        frame(&mut hasher, requirement.version().as_bytes());
        hasher.update(&[u8::from(requirement.required()), 1]);
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn stage_evidence_digest(state_digest: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    frame(&mut hasher, b"kit-staged-edit-evidence-v2");
    frame(&mut hasher, state_digest.as_bytes());
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn frame(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Directory,
    File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PhysicalState {
    device: u64,
    inode: u64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    mode: u32,
    size: u64,
}

#[derive(Clone, Debug)]
struct FileState {
    kind: Kind,
    digest: [u8; 32],
    mode: u32,
    size: u64,
    physical: PhysicalState,
}

impl FileState {
    fn digest_string(&self) -> String {
        format!("blake3:{}", blake3::Hash::from_bytes(self.digest).to_hex())
    }
}

impl PartialEq for FileState {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.digest == other.digest
            && self.mode == other.mode
            && self.size == other.size
    }
}

impl Eq for FileState {}

#[derive(Clone)]
struct Snapshot {
    entries: BTreeMap<PathBuf, FileState>,
}

impl Snapshot {
    fn physically_matches(&self, other: &Self) -> bool {
        self.entries.len() == other.entries.len()
            && self.entries.iter().all(|(path, state)| {
                other
                    .entries
                    .get(path)
                    .is_some_and(|other| state == other && state.physical == other.physical)
            })
    }
}

fn differing_paths(before: &Snapshot, after: &Snapshot) -> Vec<PathBuf> {
    before
        .entries
        .keys()
        .chain(after.entries.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.entries.get(*path) != after.entries.get(*path))
        .cloned()
        .collect()
}

struct Budget {
    entries: usize,
    bytes: u64,
    name_bytes: usize,
    path_bytes: usize,
    metadata_bytes: usize,
    limits: StageLimits,
    deadline: Instant,
}

impl Budget {
    fn new(limits: StageLimits, deadline: Instant) -> Self {
        Self {
            entries: 0,
            bytes: 0,
            name_bytes: 0,
            path_bytes: 0,
            metadata_bytes: 0,
            limits,
            deadline,
        }
    }

    fn entry(&mut self) -> Result<(), StageError> {
        check_deadline(self.deadline)?;
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or(StageError::LimitExceeded(StageLimit::Entries))?;
        if self.entries > self.limits.max_entries {
            Err(StageError::LimitExceeded(StageLimit::Entries))
        } else {
            Ok(())
        }
    }

    fn name(&mut self, bytes: usize) -> Result<(), StageError> {
        check_deadline(self.deadline)?;
        self.name_bytes = self
            .name_bytes
            .checked_add(bytes)
            .ok_or(StageError::LimitExceeded(StageLimit::NameBytes))?;
        if self.name_bytes > self.limits.max_name_bytes {
            Err(StageError::LimitExceeded(StageLimit::NameBytes))
        } else {
            Ok(())
        }
    }

    fn path(&mut self, bytes: usize) -> Result<(), StageError> {
        check_deadline(self.deadline)?;
        self.path_bytes = self
            .path_bytes
            .checked_add(bytes)
            .ok_or(StageError::LimitExceeded(StageLimit::PathBytes))?;
        if self.path_bytes > self.limits.max_path_bytes {
            Err(StageError::LimitExceeded(StageLimit::PathBytes))
        } else {
            Ok(())
        }
    }

    fn metadata(&mut self, bytes: usize) -> Result<(), StageError> {
        check_deadline(self.deadline)?;
        self.metadata_bytes = self
            .metadata_bytes
            .checked_add(bytes)
            .ok_or(StageError::LimitExceeded(StageLimit::MetadataMemory))?;
        if self.metadata_bytes > self.limits.max_metadata_bytes {
            Err(StageError::LimitExceeded(StageLimit::MetadataMemory))
        } else {
            Ok(())
        }
    }

    fn file(&mut self, bytes: u64) -> Result<(), StageError> {
        if bytes > self.limits.max_file_bytes as u64 {
            return Err(StageError::LimitExceeded(StageLimit::FileBytes));
        }
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(StageError::LimitExceeded(StageLimit::TotalBytes))?;
        if self.bytes > self.limits.max_total_bytes {
            Err(StageError::LimitExceeded(StageLimit::TotalBytes))
        } else {
            Ok(())
        }
    }
}

fn copy_tree(source: &File, target: &File, budget: &mut Budget) -> Result<(), StageError> {
    let root_mount = mount_identity(source).map_err(|_| StageError::Unavailable)?;
    copy_directory(source, target, Path::new(""), root_mount, budget)
}

fn freeze_tree(root: &File) -> Result<(), StageError> {
    fn freeze_directory(directory: &File) -> Result<(), StageError> {
        let mut stream = DirectoryStream::open(directory).map_err(|_| StageError::StageChanged)?;
        let mut names = Vec::new();
        while let Some(name) = stream.next().map_err(|_| StageError::StageChanged)? {
            if name.to_bytes() != b"." && name.to_bytes() != b".." {
                names.push(name.to_owned());
            }
        }
        drop(stream);
        for name in names {
            let before = stat_at(directory, &name).map_err(|_| StageError::StageChanged)?;
            if before.kind() == libc::S_IFDIR as u32 {
                let child = open_component(directory, &name, libc::O_RDONLY | libc::O_DIRECTORY)
                    .map_err(|_| StageError::StageChanged)?;
                if !before.same_bound(stat_file(&child).map_err(|_| StageError::StageChanged)?) {
                    return Err(StageError::StageChanged);
                }
                freeze_directory(&child)?;
                set_mode(&child, 0o500)?;
            } else if before.kind() == libc::S_IFREG as u32 && before.links == 1 {
                let child = open_component(directory, &name, libc::O_RDONLY)
                    .map_err(|_| StageError::StageChanged)?;
                if !before.same_bound(stat_file(&child).map_err(|_| StageError::StageChanged)?) {
                    return Err(StageError::StageChanged);
                }
                set_mode(&child, 0o400)?;
            } else {
                return Err(StageError::StageChanged);
            }
        }
        Ok(())
    }

    freeze_directory(root)?;
    set_mode(root, 0o500)
}

fn apply_snapshot_modes(root: &File, snapshot: &Snapshot) -> Result<(), StageError> {
    for (path, state) in snapshot
        .entries
        .iter()
        .filter(|(_, state)| state.kind == Kind::File)
        .chain(
            snapshot
                .entries
                .iter()
                .rev()
                .filter(|(_, state)| state.kind == Kind::Directory),
        )
    {
        let path = path.to_str().ok_or(StageError::StageChanged)?;
        let path =
            RootRelativePath::parse(path, usize::MAX).map_err(|_| StageError::StageChanged)?;
        let file = open_relative_file(
            root,
            &path,
            if state.kind == Kind::Directory {
                libc::O_RDONLY | libc::O_DIRECTORY
            } else {
                libc::O_RDONLY
            },
        )?;
        set_mode(&file, state.mode)?;
    }
    Ok(())
}

fn watch_stage_tree(
    root_path: &Path,
    root: &File,
    fence: &mut crate::workspace::revision::WorkspaceKernelMutationFence,
    include_final: bool,
) -> Result<(), StageError> {
    let mut pending = vec![(
        root_path.to_owned(),
        root.try_clone().map_err(|_| StageError::Unavailable)?,
    )];
    while let Some((path, directory)) = pending.pop() {
        fence
            .watch(&path, &directory, true)
            .map_err(map_fence_error)?;
        let mut stream = DirectoryStream::open(&directory).map_err(|_| StageError::StageChanged)?;
        let mut names = Vec::new();
        while let Some(name) = stream.next().map_err(|_| StageError::StageChanged)? {
            if name.to_bytes() != b"." && name.to_bytes() != b".." {
                names.push(name.to_owned());
            }
        }
        drop(stream);
        for name in names {
            if path == root_path && !include_final && name.to_bytes() == b"final" {
                continue;
            }
            let before = stat_at(&directory, &name).map_err(|_| StageError::StageChanged)?;
            let child_path = path.join(OsString::from_vec(name.to_bytes().to_vec()));
            if before.kind() == libc::S_IFDIR as u32 {
                let child = open_component(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY)
                    .map_err(|_| StageError::StageChanged)?;
                if !before.same_bound(stat_file(&child).map_err(|_| StageError::StageChanged)?) {
                    return Err(StageError::StageChanged);
                }
                pending.push((child_path, child));
            } else if before.kind() == libc::S_IFREG as u32 && before.links == 1 {
                let child = open_component(&directory, &name, libc::O_RDONLY)
                    .map_err(|_| StageError::StageChanged)?;
                if !before.same_bound(stat_file(&child).map_err(|_| StageError::StageChanged)?) {
                    return Err(StageError::StageChanged);
                }
                fence
                    .watch(&child_path, &child, false)
                    .map_err(map_fence_error)?;
            } else {
                return Err(StageError::StageChanged);
            }
        }
    }
    Ok(())
}

fn verify_frozen_tree(
    root: &File,
    expected: &Snapshot,
    budget: &mut Budget,
) -> Result<(), StageError> {
    let actual = snapshot_tree(root, budget)?;
    if actual.entries.len() != expected.entries.len() {
        return Err(StageError::StageChanged);
    }
    for (path, expected) in &expected.entries {
        let Some(actual) = actual.entries.get(path) else {
            return Err(StageError::StageChanged);
        };
        let frozen_mode = match expected.kind {
            Kind::Directory => 0o500,
            Kind::File => 0o400,
        };
        if actual.kind != expected.kind
            || actual.digest != expected.digest
            || actual.size != expected.size
            || actual.mode != frozen_mode
        {
            return Err(StageError::StageChanged);
        }
    }
    Ok(())
}

fn verify_quiescent_tree(
    root: &File,
    baseline: &Snapshot,
    generation: u64,
    fence: &mut crate::workspace::revision::WorkspaceKernelMutationFence,
    budget: &mut Budget,
) -> Result<(), StageError> {
    fence.ensure_clean().map_err(|_| StageError::StageChanged)?;
    let first = snapshot_tree(root, budget)?;
    fence.ensure_clean().map_err(|_| StageError::StageChanged)?;
    let second = snapshot_tree(root, budget)?;
    fence.ensure_clean().map_err(|_| StageError::StageChanged)?;
    if fence.generation() != generation
        || !baseline.physically_matches(&first)
        || !baseline.physically_matches(&second)
    {
        return Err(StageError::StageChanged);
    }
    Ok(())
}

fn copy_directory(
    source: &File,
    target: &File,
    relative: &Path,
    root_mount: MountIdentity,
    budget: &mut Budget,
) -> Result<(), StageError> {
    let directory_before = stat_file(source).map_err(|_| StageError::StageChanged)?;
    if directory_before.kind() != libc::S_IFDIR as u32
        || mount_identity(source).map_err(|_| StageError::Unavailable)? != root_mount
        || !supported_directory_metadata(source, directory_before)
    {
        return Err(StageError::UnsafeSource);
    }
    let mut names = directory_names(source, budget)?;
    names.sort();
    for bytes in names {
        let name = CString::new(bytes.clone()).map_err(|_| StageError::UnsafeSource)?;
        let path = relative.join(OsString::from_vec(bytes));
        budget.path(path.as_os_str().as_encoded_bytes().len())?;
        let before = stat_at(source, &name).map_err(|_| StageError::StageChanged)?;
        match before.kind() {
            kind if kind == libc::S_IFDIR as u32 => {
                mkdir_at(target, &name, 0o700)?;
                let source_child =
                    open_component(source, &name, libc::O_RDONLY | libc::O_DIRECTORY)
                        .map_err(|_| StageError::UnsafeSource)?;
                let target_child =
                    open_component(target, &name, libc::O_RDONLY | libc::O_DIRECTORY)
                        .map_err(|_| StageError::Unavailable)?;
                if !before
                    .same_bound(stat_file(&source_child).map_err(|_| StageError::StageChanged)?)
                {
                    return Err(StageError::StageChanged);
                }
                if !supported_directory_metadata(&source_child, before) {
                    return Err(StageError::UnsafeSource);
                }
                copy_directory(&source_child, &target_child, &path, root_mount, budget)?;
                set_mode(&target_child, before.mode & 0o777)?;
            }
            kind if kind == libc::S_IFREG as u32 => {
                if before.links != 1 {
                    return Err(StageError::UnsafeSource);
                }
                budget.file(before.size)?;
                budget.file(before.size)?;
                let mut source_file = open_component(source, &name, libc::O_RDONLY)
                    .map_err(|_| StageError::UnsafeSource)?;
                if !before
                    .same_bound(stat_file(&source_file).map_err(|_| StageError::StageChanged)?)
                {
                    return Err(StageError::StageChanged);
                }
                if mount_identity(&source_file).map_err(|_| StageError::Unavailable)? != root_mount
                {
                    return Err(StageError::UnsafeSource);
                }
                if !supported_metadata(&source_file, before) {
                    return Err(StageError::UnsafeSource);
                }
                let mut target_file = create_file(target, &name)?;
                copy_file_bytes(&mut source_file, &mut target_file, before, budget.deadline)?;
                strip_creation_metadata(&target_file)?;
                set_mode(&target_file, before.mode & 0o777)?;
            }
            _ => return Err(StageError::UnsafeSource),
        }
    }
    if !directory_before.same_bound(stat_file(source).map_err(|_| StageError::StageChanged)?) {
        return Err(StageError::StageChanged);
    }
    Ok(())
}

fn snapshot_tree(root: &File, budget: &mut Budget) -> Result<Snapshot, StageError> {
    let root_mount = mount_identity(root).map_err(|_| StageError::Unavailable)?;
    let mut entries = BTreeMap::new();
    snapshot_directory(root, Path::new(""), root_mount, budget, &mut entries)?;
    Ok(Snapshot { entries })
}

fn snapshot_directory(
    directory: &File,
    relative: &Path,
    root_mount: MountIdentity,
    budget: &mut Budget,
    entries: &mut BTreeMap<PathBuf, FileState>,
) -> Result<(), StageError> {
    let directory_before = stat_file(directory).map_err(|_| StageError::StageChanged)?;
    if directory_before.kind() != libc::S_IFDIR as u32
        || mount_identity(directory).map_err(|_| StageError::Unavailable)? != root_mount
        || !supported_directory_metadata(directory, directory_before)
    {
        return Err(StageError::UnsafeSource);
    }
    let mut names = directory_names(directory, budget)?;
    names.sort();
    for bytes in names {
        let name = CString::new(bytes.clone()).map_err(|_| StageError::UnsafeSource)?;
        let path = relative.join(OsString::from_vec(bytes));
        budget.path(path.as_os_str().as_encoded_bytes().len())?;
        let before = stat_at(directory, &name).map_err(|_| StageError::StageChanged)?;
        match before.kind() {
            kind if kind == libc::S_IFDIR as u32 => {
                let child = open_component(directory, &name, libc::O_RDONLY | libc::O_DIRECTORY)
                    .map_err(|_| StageError::UnsafeSource)?;
                if !before.same_bound(stat_file(&child).map_err(|_| StageError::StageChanged)?) {
                    return Err(StageError::StageChanged);
                }
                if !supported_directory_metadata(&child, before) {
                    return Err(StageError::UnsafeSource);
                }
                entries.insert(
                    path.clone(),
                    FileState {
                        kind: Kind::Directory,
                        digest: [0; 32],
                        mode: before.mode & 0o777,
                        size: 0,
                        physical: PhysicalState {
                            device: before.device,
                            inode: before.inode,
                            changed_seconds: before.changed_seconds,
                            changed_nanoseconds: before.changed_nanoseconds,
                            mode: before.mode & 0o777,
                            size: before.size,
                        },
                    },
                );
                snapshot_directory(&child, &path, root_mount, budget, entries)?;
            }
            kind if kind == libc::S_IFREG as u32 => {
                if before.links != 1 {
                    return Err(StageError::UnsafeSource);
                }
                budget.file(before.size)?;
                let file = open_component(directory, &name, libc::O_RDONLY)
                    .map_err(|_| StageError::UnsafeSource)?;
                if !before.same_bound(stat_file(&file).map_err(|_| StageError::StageChanged)?) {
                    return Err(StageError::StageChanged);
                }
                if mount_identity(&file).map_err(|_| StageError::Unavailable)? != root_mount {
                    return Err(StageError::UnsafeSource);
                }
                if !supported_metadata(&file, before) {
                    return Err(StageError::UnsafeSource);
                }
                let digest = hash_file_checked(file, before, budget.deadline)?;
                entries.insert(
                    path,
                    FileState {
                        kind: Kind::File,
                        digest,
                        mode: before.mode & 0o777,
                        size: before.size,
                        physical: PhysicalState {
                            device: before.device,
                            inode: before.inode,
                            changed_seconds: before.changed_seconds,
                            changed_nanoseconds: before.changed_nanoseconds,
                            mode: before.mode & 0o777,
                            size: before.size,
                        },
                    },
                );
            }
            _ => return Err(StageError::UnsafeSource),
        }
    }
    if !directory_before.same_bound(stat_file(directory).map_err(|_| StageError::StageChanged)?) {
        return Err(StageError::StageChanged);
    }
    Ok(())
}

fn directory_names(directory: &File, budget: &mut Budget) -> Result<Vec<Vec<u8>>, StageError> {
    let mut stream = DirectoryStream::open(directory).map_err(|_| StageError::Unavailable)?;
    let mut names = Vec::new();
    while let Some(name) = stream.next().map_err(|_| StageError::StageChanged)? {
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            budget.entry()?;
            budget.name(name.to_bytes().len())?;
            budget.metadata(std::mem::size_of::<Stat>() + std::mem::size_of::<Vec<u8>>())?;
            names
                .try_reserve(1)
                .map_err(|_| StageError::LimitExceeded(StageLimit::Entries))?;
            names.push(name.to_bytes().to_vec());
        }
    }
    Ok(names)
}

fn copy_file_bytes(
    source: &mut File,
    target: &mut File,
    before: Stat,
    deadline: Instant,
) -> Result<(), StageError> {
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_deadline(deadline)?;
        let count = source
            .read(&mut buffer)
            .map_err(|_| StageError::StageChanged)?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(count as u64)
            .ok_or(StageError::LimitExceeded(StageLimit::TotalBytes))?;
        if copied > before.size {
            return Err(StageError::StageChanged);
        }
        target
            .write_all(&buffer[..count])
            .map_err(|_| StageError::Unavailable)?;
    }
    if copied != before.size
        || !before.same_bound(stat_file(source).map_err(|_| StageError::StageChanged)?)
    {
        return Err(StageError::StageChanged);
    }
    Ok(())
}

fn hash_file_checked(
    mut file: File,
    before: Stat,
    deadline: Instant,
) -> Result<[u8; 32], StageError> {
    let mut hasher = blake3::Hasher::new();
    let mut read = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_deadline(deadline)?;
        let count = file
            .read(&mut buffer)
            .map_err(|_| StageError::StageChanged)?;
        if count == 0 {
            break;
        }
        read += count as u64;
        if read > before.size {
            return Err(StageError::StageChanged);
        }
        hasher.update(&buffer[..count]);
    }
    if read != before.size
        || !before.same_bound(stat_file(&file).map_err(|_| StageError::StageChanged)?)
    {
        return Err(StageError::StageChanged);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn read_relative(
    root: &File,
    path: &RootRelativePath,
    max_bytes: usize,
    deadline: Instant,
) -> Result<Vec<u8>, StageError> {
    check_deadline(deadline)?;
    let file = open_relative_file(root, path, libc::O_RDONLY)?;
    check_deadline(deadline)?;
    let before = stat_file(&file).map_err(|_| StageError::StageChanged)?;
    check_deadline(deadline)?;
    if before.kind() != libc::S_IFREG as u32 || before.links != 1 {
        return Err(StageError::UnsafeSource);
    }
    if !supported_metadata(&file, before) {
        return Err(StageError::UnsafeSource);
    }
    if before.size > max_bytes as u64 {
        return Err(StageError::LimitExceeded(StageLimit::FileBytes));
    }
    read_file_checked(file, before, max_bytes, deadline)
}

fn read_file_checked(
    file: File,
    before: Stat,
    max_bytes: usize,
    deadline: Instant,
) -> Result<Vec<u8>, StageError> {
    read_file_checked_with(file, before, max_bytes, deadline, || {})
}

fn read_file_checked_with(
    mut file: File,
    before: Stat,
    max_bytes: usize,
    deadline: Instant,
    mut after_chunk: impl FnMut(),
) -> Result<Vec<u8>, StageError> {
    check_deadline(deadline)?;
    let size = usize::try_from(before.size)
        .map_err(|_| StageError::LimitExceeded(StageLimit::FileBytes))?;
    if size > max_bytes {
        return Err(StageError::LimitExceeded(StageLimit::FileBytes));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| StageError::LimitExceeded(StageLimit::FileBytes))?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_deadline(deadline)?;
        let count = file
            .read(&mut buffer)
            .map_err(|_| StageError::StageChanged)?;
        after_chunk();
        check_deadline(deadline)?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > size {
            return Err(StageError::StageChanged);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    check_deadline(deadline)?;
    let after = stat_file(&file).map_err(|_| StageError::StageChanged)?;
    check_deadline(deadline)?;
    if bytes.len() != size || !before.same_bound(after) {
        return Err(StageError::StageChanged);
    }
    Ok(bytes)
}

fn write_relative(
    root: &File,
    path: &RootRelativePath,
    content: &[u8],
    mode: u32,
    create: bool,
    limits: StageLimits,
) -> Result<(), StageError> {
    if content.len() > limits.max_file_bytes || mode & !0o777 != 0 || mode & 0o7000 != 0 {
        return Err(StageError::LimitExceeded(StageLimit::FileBytes));
    }
    let (parent, leaf) = open_parent(root, path)?;
    let flags = libc::O_WRONLY
        | if create {
            libc::O_CREAT | libc::O_EXCL
        } else {
            libc::O_TRUNC
        };
    let mut file =
        open_component_mode(&parent, &leaf, flags, 0o600).map_err(|_| StageError::PlanMismatch)?;
    strip_creation_metadata(&file)?;
    let stat = stat_file(&file).map_err(|_| StageError::StageChanged)?;
    if stat.kind() != libc::S_IFREG as u32 || stat.links != 1 {
        return Err(StageError::UnsafeSource);
    }
    file.write_all(content)
        .map_err(|_| StageError::Unavailable)?;
    strip_creation_metadata(&file)?;
    set_mode(&file, mode)?;
    Ok(())
}

fn unlink_relative(root: &File, path: &RootRelativePath) -> Result<(), StageError> {
    let (parent, leaf) = open_parent(root, path)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } != 0 {
        Err(StageError::PlanMismatch)
    } else {
        Ok(())
    }
}

fn open_relative_file(
    root: &File,
    path: &RootRelativePath,
    flags: libc::c_int,
) -> Result<File, StageError> {
    let (parent, leaf) = open_parent(root, path)?;
    open_component(&parent, &leaf, flags).map_err(|_| StageError::StageChanged)
}

fn open_parent(root: &File, path: &RootRelativePath) -> Result<(File, CString), StageError> {
    let mut components = path.as_str().split('/').peekable();
    let mut directory = root.try_clone().map_err(|_| StageError::Unavailable)?;
    while let Some(component) = components.next() {
        let name = CString::new(component).map_err(|_| StageError::UnsafeSource)?;
        if components.peek().is_none() {
            return Ok((directory, name));
        }
        directory = open_component(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(|_| StageError::UnsafeSource)?;
    }
    Err(StageError::UnsafeSource)
}

fn digest_matches(expected: &str, bytes: &[u8]) -> bool {
    let Some((algorithm, value)) = expected.split_once(':') else {
        return false;
    };
    match algorithm {
        "blake3" => blake3::hash(bytes).to_hex().as_str() == value,
        "sha256" => format!("{:x}", Sha256::digest(bytes)) == value,
        _ => false,
    }
}

struct Allocation {
    parent: File,
    name: CString,
    quarantine: CString,
    cleanup_state: Mutex<CleanupState>,
    root_path: PathBuf,
    root: File,
    view: File,
    final_view: File,
    device: u64,
    inode: u64,
    nonce: [u8; 16],
    marker: [u8; 32],
    cleaned: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupState {
    Unbound,
    Original,
    Quarantined,
    Deleted,
}

struct AllocationBootstrap<'a> {
    parent: &'a File,
    name: &'a CStr,
    armed: bool,
}

impl Drop for AllocationBootstrap<'_> {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(root) =
                open_component(self.parent, self.name, libc::O_RDONLY | libc::O_DIRECTORY)
            {
                let _ = make_tree_removable(&root);
                let _ = remove_tree_contents(&root);
            }
            unsafe {
                libc::unlinkat(
                    self.parent.as_raw_fd(),
                    self.name.as_ptr(),
                    libc::AT_REMOVEDIR,
                );
            }
        }
    }
}

impl Allocation {
    fn new(parent: File, parent_path: PathBuf) -> Result<Self, StageError> {
        let parent_stat = stat_file(&parent).map_err(|_| StageError::Unavailable)?;
        if parent_stat.kind() != libc::S_IFDIR as u32 {
            return Err(StageError::Unavailable);
        }
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| StageError::Unavailable)?;
        let name = CString::new(format!(".kit-stage-{}", hex(&random)))
            .map_err(|_| StageError::Unavailable)?;
        let quarantine = CString::new(format!(".kit-stage-drop-{}", hex(&random)))
            .map_err(|_| StageError::Unavailable)?;
        let mut marker = [0_u8; 32];
        getrandom::fill(&mut marker).map_err(|_| StageError::Unavailable)?;
        record_cleanup_state(
            &parent,
            &name,
            &name,
            CleanupState::Unbound,
            0,
            0,
            &random,
            &marker,
        )?;
        mkdir_at(&parent, &name, 0o700)?;
        let mut bootstrap = AllocationBootstrap {
            parent: &parent,
            name: &name,
            armed: true,
        };
        let root = open_component(&parent, &name, libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(|_| StageError::Unavailable)?;
        set_mode(&root, 0o700)?;
        let metadata = root.metadata().map_err(|_| StageError::Unavailable)?;
        let marker_name = CString::new(MARKER_NAME).expect("static marker name has no NUL");
        let mut marker_file = create_file(&root, &marker_name)?;
        marker_file
            .write_all(&marker)
            .map_err(|_| StageError::Unavailable)?;
        strip_creation_metadata(&marker_file)?;
        set_mode(&marker_file, 0o600)?;
        marker_file
            .sync_all()
            .map_err(|_| StageError::Unavailable)?;
        root.sync_all().map_err(|_| StageError::Unavailable)?;
        record_cleanup_state(
            &parent,
            &name,
            &name,
            CleanupState::Original,
            metadata.dev(),
            metadata.ino(),
            &random,
            &marker,
        )?;
        let path = parent_path.join(name.to_string_lossy().as_ref());
        for directory in ["view", "final"] {
            let directory = CString::new(directory).expect("static stage name has no NUL");
            mkdir_at(&root, &directory, 0o700)?;
        }
        let open_directory = |name: &'static CStr| {
            open_component(&root, name, libc::O_RDONLY | libc::O_DIRECTORY)
                .map_err(|_| StageError::Unavailable)
        };
        let view = open_directory(c"view")?;
        let final_view = open_directory(c"final")?;
        bootstrap.armed = false;
        drop(bootstrap);
        Ok(Self {
            parent,
            name,
            quarantine,
            cleanup_state: Mutex::new(CleanupState::Original),
            root_path: path,
            root,
            view,
            final_view,
            device: metadata.dev(),
            inode: metadata.ino(),
            nonce: random,
            marker,
            cleaned: false,
        })
    }

    fn cleanup(&self) -> io::Result<()> {
        let mut state = self
            .cleanup_state
            .lock()
            .map_err(|_| io::Error::other("cleanup state lock poisoned"))?;
        if *state == CleanupState::Deleted {
            return Ok(());
        }
        let root_stat = stat_file(&self.root)?;
        if root_stat.device != self.device || root_stat.inode != self.inode {
            return Err(io::Error::other("stage allocation identity changed"));
        }
        let marker_name = CString::new(MARKER_NAME).expect("static marker name has no NUL");
        let marker_file = open_component(&self.root, &marker_name, libc::O_RDONLY)?;
        let marker_stat = stat_file(&marker_file)?;
        if marker_stat.kind() != libc::S_IFREG as u32
            || marker_stat.links != 1
            || marker_stat.size != self.marker.len() as u64
        {
            return Err(io::Error::other("stage allocation marker identity changed"));
        }
        let marker = read_marker(marker_file, self.marker.len())?;
        if marker != self.marker {
            return Err(io::Error::other("stage allocation marker changed"));
        }
        if *state == CleanupState::Original {
            if unsafe {
                libc::renameat(
                    self.parent.as_raw_fd(),
                    self.name.as_ptr(),
                    self.parent.as_raw_fd(),
                    self.quarantine.as_ptr(),
                )
            } != 0
            {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::NotFound
                    || !stat_at(&self.parent, &self.quarantine)
                        .is_ok_and(|stat| stat.device == self.device && stat.inode == self.inode)
                {
                    return Err(error);
                }
            }
            self.parent.sync_all()?;
            record_cleanup_state_io(
                &self.parent,
                &self.name,
                &self.quarantine,
                CleanupState::Quarantined,
                self.device,
                self.inode,
                &self.nonce,
                &self.marker,
            )?;
            *state = CleanupState::Quarantined;
        }
        let renamed = match stat_at(&self.parent, &self.quarantine) {
            Ok(renamed) => renamed,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                record_cleanup_state_io(
                    &self.parent,
                    &self.name,
                    &self.quarantine,
                    CleanupState::Deleted,
                    self.device,
                    self.inode,
                    &self.nonce,
                    &self.marker,
                )?;
                *state = CleanupState::Deleted;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if renamed.kind() != libc::S_IFDIR as u32
            || renamed.device != self.device
            || renamed.inode != self.inode
        {
            return Err(io::Error::other("stage allocation identity changed"));
        }
        make_tree_removable(&self.root)?;
        remove_tree_contents(&self.root)?;
        if unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.quarantine.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        self.parent.sync_all()?;
        record_cleanup_state_io(
            &self.parent,
            &self.name,
            &self.quarantine,
            CleanupState::Deleted,
            self.device,
            self.inode,
            &self.nonce,
            &self.marker,
        )?;
        *state = CleanupState::Deleted;
        Ok(())
    }
}

fn make_tree_removable(directory: &File) -> io::Result<()> {
    let mut stream = DirectoryStream::open(directory)?;
    let mut names = Vec::new();
    while let Some(name) = stream.next()? {
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
    drop(stream);
    for name in names {
        let stat = stat_at(directory, &name)?;
        if stat.kind() == libc::S_IFDIR as u32 {
            let child = open_component(directory, &name, libc::O_RDONLY | libc::O_DIRECTORY)?;
            if !stat.same_bound(stat_file(&child)?) {
                return Err(io::Error::other("cleanup directory identity changed"));
            }
            make_tree_removable(&child)?;
            if unsafe { libc::fchmod(child.as_raw_fd(), 0o700) } != 0 {
                return Err(io::Error::last_os_error());
            }
        } else if stat.kind() == libc::S_IFREG as u32 && stat.links == 1 {
            let child = open_component(directory, &name, libc::O_RDONLY)?;
            if !stat.same_bound(stat_file(&child)?) {
                return Err(io::Error::other("cleanup file identity changed"));
            }
            if unsafe { libc::fchmod(child.as_raw_fd(), 0o600) } != 0 {
                return Err(io::Error::last_os_error());
            }
        } else {
            return Err(io::Error::other("cleanup encountered unsafe entry"));
        }
    }
    Ok(())
}

fn remove_tree_contents(directory: &File) -> io::Result<()> {
    let mut stream = DirectoryStream::open(directory)?;
    let mut names = Vec::new();
    while let Some(name) = stream.next()? {
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
    drop(stream);
    for name in names {
        let stat = stat_at(directory, &name)?;
        let flags = if stat.kind() == libc::S_IFDIR as u32 {
            let child = open_component(directory, &name, libc::O_RDONLY | libc::O_DIRECTORY)?;
            if !stat.same_bound(stat_file(&child)?) {
                return Err(io::Error::other("cleanup directory identity changed"));
            }
            remove_tree_contents(&child)?;
            libc::AT_REMOVEDIR
        } else if stat.kind() == libc::S_IFREG as u32 && stat.links == 1 {
            let child = open_component(directory, &name, libc::O_RDONLY)?;
            if !stat.same_bound(stat_file(&child)?) {
                return Err(io::Error::other("cleanup file identity changed"));
            }
            0
        } else {
            return Err(io::Error::other("cleanup encountered unsafe entry"));
        };
        if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), flags) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn read_marker(mut file: File, size: usize) -> io::Result<Vec<u8>> {
    let mut marker = vec![0_u8; size];
    file.read_exact(&mut marker)?;
    let mut extra = [0_u8; 1];
    if file.read(&mut extra)? != 0 {
        return Err(io::Error::other("stage allocation marker grew"));
    }
    Ok(marker)
}

impl Drop for Allocation {
    fn drop(&mut self) {
        if !self.cleaned
            && let Err(error) = self.cleanup()
        {
            record_cleanup_failure(self, &error);
        }
    }
}

fn record_cleanup_failure(allocation: &Allocation, error: &io::Error) {
    let state = allocation
        .cleanup_state
        .lock()
        .map(|state| *state)
        .unwrap_or(CleanupState::Original);
    let current = if state == CleanupState::Original {
        &allocation.name
    } else {
        &allocation.quarantine
    };
    let _ = record_cleanup_state_io(
        &allocation.parent,
        &allocation.name,
        current,
        state,
        allocation.device,
        allocation.inode,
        &allocation.nonce,
        &allocation.marker,
    );
    let _ = error;
}

#[allow(clippy::too_many_arguments)]
fn record_cleanup_state(
    parent: &File,
    original: &CStr,
    current: &CStr,
    state: CleanupState,
    device: u64,
    inode: u64,
    nonce: &[u8; 16],
    marker: &[u8; 32],
) -> Result<(), StageError> {
    record_cleanup_state_io(
        parent, original, current, state, device, inode, nonce, marker,
    )
    .map_err(|_| StageError::CleanupFailed)
}

#[allow(clippy::too_many_arguments)]
fn record_cleanup_state_io(
    parent: &File,
    original: &CStr,
    current: &CStr,
    state: CleanupState,
    device: u64,
    inode: u64,
    nonce: &[u8; 16],
    marker: &[u8; 32],
) -> io::Result<()> {
    let mut file = open_component_mode(
        parent,
        CLEANUP_QUEUE_NAME,
        libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT,
        0o600,
    )?;
    let metadata = stat_file(&file)?;
    if metadata.kind() != libc::S_IFREG as u32
        || metadata.links != 1
        || metadata.mode & 0o777 != 0o600
        || !supported_metadata(&file, metadata)
    {
        return Err(io::Error::other("unsafe stage cleanup queue"));
    }
    let state = match state {
        CleanupState::Unbound => "unbound",
        CleanupState::Original => "original",
        CleanupState::Quarantined => "quarantined",
        CleanupState::Deleted => "deleted",
    };
    let parent_identity = stat_file(parent)?;
    let payload = format!(
        "v1\t{state}\t{}\t{}\t{}\t{}\t{device}\t{inode}\t{}\t{}",
        original.to_string_lossy(),
        current.to_string_lossy(),
        parent_identity.device,
        parent_identity.inode,
        hex(nonce),
        hex(marker)
    );
    if payload.len() > CLEANUP_RECORD_LIMIT - 66 {
        return Err(io::Error::other("stage cleanup record is too large"));
    }
    let checksum = blake3::hash(payload.as_bytes());
    if metadata
        .size
        .saturating_add((payload.len() + checksum.to_hex().len() + 2) as u64)
        > CLEANUP_QUEUE_LIMIT as u64
    {
        return Err(io::Error::other("stage cleanup queue is too large"));
    }
    writeln!(file, "{payload}\t{checksum}")?;
    file.sync_all()?;
    parent.sync_all()
}

#[derive(Clone)]
struct RecoveryRecord {
    state: CleanupState,
    original: CString,
    current: CString,
    parent_device: u64,
    parent_inode: u64,
    device: u64,
    inode: u64,
    nonce: [u8; 16],
    marker: [u8; 32],
}

pub(crate) fn recover_allocations(parent: &File, _parent_path: &Path) -> Result<(), StageError> {
    let records = read_cleanup_records(parent)?;
    for record in records.values() {
        recover_allocation(parent, record)?;
    }
    compact_cleanup_queue(parent)
}

fn read_cleanup_records(parent: &File) -> Result<BTreeMap<[u8; 16], RecoveryRecord>, StageError> {
    let file = match open_component(parent, CLEANUP_QUEUE_NAME, libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(_) => return Err(StageError::CleanupFailed),
    };
    let metadata = stat_file(&file).map_err(|_| StageError::CleanupFailed)?;
    if metadata.kind() != libc::S_IFREG as u32
        || metadata.links != 1
        || metadata.mode & 0o777 != 0o600
        || metadata.size > CLEANUP_QUEUE_LIMIT as u64
        || !supported_metadata(&file, metadata)
    {
        return Err(StageError::CleanupFailed);
    }
    let mut bytes = Vec::new();
    file.take((CLEANUP_QUEUE_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| StageError::CleanupFailed)?;
    if bytes.len() > CLEANUP_QUEUE_LIMIT || (!bytes.is_empty() && !bytes.ends_with(b"\n")) {
        return Err(StageError::CleanupFailed);
    }
    if bytes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let parent_state = stat_file(parent).map_err(|_| StageError::CleanupFailed)?;
    let mut records = BTreeMap::<[u8; 16], RecoveryRecord>::new();
    let records_bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    for line in records_bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() || line.len() > CLEANUP_RECORD_LIMIT {
            return Err(StageError::CleanupFailed);
        }
        let record = parse_cleanup_record(line)?;
        if record.parent_device != parent_state.device || record.parent_inode != parent_state.inode
        {
            return Err(StageError::CleanupFailed);
        }
        if let Some(previous) = records.get(&record.nonce) {
            let binding = previous.state == CleanupState::Unbound
                && record.state == CleanupState::Original
                && previous.device == 0
                && previous.inode == 0;
            if previous.original != record.original
                || previous.parent_device != record.parent_device
                || previous.parent_inode != record.parent_inode
                || !binding && previous.device != record.device
                || !binding && previous.inode != record.inode
                || previous.marker != record.marker
                || cleanup_state_rank(record.state) < cleanup_state_rank(previous.state)
            {
                return Err(StageError::CleanupFailed);
            }
        }
        records.insert(record.nonce, record);
    }
    Ok(records)
}

fn parse_cleanup_record(line: &[u8]) -> Result<RecoveryRecord, StageError> {
    let text = std::str::from_utf8(line).map_err(|_| StageError::CleanupFailed)?;
    let (payload, checksum) = text.rsplit_once('\t').ok_or(StageError::CleanupFailed)?;
    if checksum != blake3::hash(payload.as_bytes()).to_hex().as_str() {
        return Err(StageError::CleanupFailed);
    }
    let fields = payload.split('\t').collect::<Vec<_>>();
    let [
        "v1",
        state,
        original,
        current,
        parent_device,
        parent_inode,
        device,
        inode,
        nonce,
        marker,
    ] = fields.as_slice()
    else {
        return Err(StageError::CleanupFailed);
    };
    let state = match *state {
        "unbound" => CleanupState::Unbound,
        "original" => CleanupState::Original,
        "quarantined" => CleanupState::Quarantined,
        "deleted" => CleanupState::Deleted,
        _ => return Err(StageError::CleanupFailed),
    };
    let nonce: [u8; 16] = decode_hex(nonce)?
        .try_into()
        .map_err(|_| StageError::CleanupFailed)?;
    let marker: [u8; 32] = decode_hex(marker)?
        .try_into()
        .map_err(|_| StageError::CleanupFailed)?;
    let expected_original = format!(".kit-stage-{}", hex(&nonce));
    let expected_quarantine = format!(".kit-stage-drop-{}", hex(&nonce));
    if *original != expected_original
        || match state {
            CleanupState::Unbound => *current != expected_original,
            CleanupState::Original => *current != expected_original,
            CleanupState::Quarantined | CleanupState::Deleted => *current != expected_quarantine,
        }
    {
        return Err(StageError::CleanupFailed);
    }
    Ok(RecoveryRecord {
        state,
        original: CString::new(*original).map_err(|_| StageError::CleanupFailed)?,
        current: CString::new(*current).map_err(|_| StageError::CleanupFailed)?,
        parent_device: parent_device
            .parse()
            .map_err(|_| StageError::CleanupFailed)?,
        parent_inode: parent_inode
            .parse()
            .map_err(|_| StageError::CleanupFailed)?,
        device: device.parse().map_err(|_| StageError::CleanupFailed)?,
        inode: inode.parse().map_err(|_| StageError::CleanupFailed)?,
        nonce,
        marker,
    })
}

fn recover_allocation(parent: &File, record: &RecoveryRecord) -> Result<(), StageError> {
    if record.state == CleanupState::Unbound {
        let root =
            match open_component(parent, &record.original, libc::O_RDONLY | libc::O_DIRECTORY) {
                Ok(root) => root,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(());
                }
                Err(_) => return Err(StageError::CleanupFailed),
            };
        let stat = stat_file(&root).map_err(|_| StageError::CleanupFailed)?;
        if stat.kind() != libc::S_IFDIR as u32
            || stat.mode & 0o777 != 0o700
            || read_recovery_marker(&root) != Some(record.marker)
        {
            return Err(StageError::CleanupFailed);
        }
        record_cleanup_state(
            parent,
            &record.original,
            &record.original,
            CleanupState::Original,
            stat.device,
            stat.inode,
            &record.nonce,
            &record.marker,
        )?;
        let mut bound = record.clone();
        bound.state = CleanupState::Original;
        bound.device = stat.device;
        bound.inode = stat.inode;
        return recover_allocation(parent, &bound);
    }
    if record.state == CleanupState::Deleted {
        return Ok(());
    }
    let quarantine = CString::new(format!(".kit-stage-drop-{}", hex(&record.nonce)))
        .map_err(|_| StageError::CleanupFailed)?;
    let mut current = &record.current;
    if record.state == CleanupState::Original {
        match open_recorded_allocation(parent, &record.original, record) {
            Ok(root) => {
                if stat_at(parent, &quarantine).is_ok() {
                    return Err(StageError::CleanupFailed);
                }
                if unsafe {
                    libc::renameat(
                        parent.as_raw_fd(),
                        record.original.as_ptr(),
                        parent.as_raw_fd(),
                        quarantine.as_ptr(),
                    )
                } != 0
                {
                    return Err(StageError::CleanupFailed);
                }
                parent.sync_all().map_err(|_| StageError::CleanupFailed)?;
                drop(root);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match open_recorded_allocation(parent, &quarantine, record) {
                    Ok(root) => drop(root),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        record_cleanup_state(
                            parent,
                            &record.original,
                            &quarantine,
                            CleanupState::Deleted,
                            record.device,
                            record.inode,
                            &record.nonce,
                            &record.marker,
                        )?;
                        return Ok(());
                    }
                    Err(_) => return Err(StageError::CleanupFailed),
                }
            }
            Err(_) => return Err(StageError::CleanupFailed),
        }
        record_cleanup_state(
            parent,
            &record.original,
            &quarantine,
            CleanupState::Quarantined,
            record.device,
            record.inode,
            &record.nonce,
            &record.marker,
        )?;
        current = &quarantine;
    }
    let root = match open_recorded_allocation(parent, current, record) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            record_cleanup_state(
                parent,
                &record.original,
                &quarantine,
                CleanupState::Deleted,
                record.device,
                record.inode,
                &record.nonce,
                &record.marker,
            )?;
            return Ok(());
        }
        Err(_) => return Err(StageError::CleanupFailed),
    };
    make_tree_removable(&root).map_err(|_| StageError::CleanupFailed)?;
    remove_tree_contents(&root).map_err(|_| StageError::CleanupFailed)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), current.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(StageError::CleanupFailed);
    }
    parent.sync_all().map_err(|_| StageError::CleanupFailed)?;
    record_cleanup_state(
        parent,
        &record.original,
        &quarantine,
        CleanupState::Deleted,
        record.device,
        record.inode,
        &record.nonce,
        &record.marker,
    )
}

fn open_recorded_allocation(
    parent: &File,
    name: &CStr,
    record: &RecoveryRecord,
) -> io::Result<File> {
    let before = stat_at(parent, name)?;
    if before.kind() != libc::S_IFDIR as u32
        || before.device != record.device
        || before.inode != record.inode
    {
        return Err(io::Error::other("stage allocation identity changed"));
    }
    let root = open_component(parent, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
    if !before.same_bound(stat_file(&root)?) || read_recovery_marker(&root) != Some(record.marker) {
        return Err(io::Error::other("stage allocation marker changed"));
    }
    Ok(root)
}

const fn cleanup_state_rank(state: CleanupState) -> u8 {
    match state {
        CleanupState::Unbound => 0,
        CleanupState::Original => 1,
        CleanupState::Quarantined => 2,
        CleanupState::Deleted => 3,
    }
}

fn compact_cleanup_queue(parent: &File) -> Result<(), StageError> {
    let file = match open_component(parent, CLEANUP_QUEUE_NAME, libc::O_WRONLY | libc::O_TRUNC) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(StageError::CleanupFailed),
    };
    let metadata = stat_file(&file).map_err(|_| StageError::CleanupFailed)?;
    if metadata.kind() != libc::S_IFREG as u32
        || metadata.links != 1
        || metadata.mode & 0o777 != 0o600
        || !supported_metadata(&file, metadata)
    {
        return Err(StageError::CleanupFailed);
    }
    file.sync_all().map_err(|_| StageError::CleanupFailed)?;
    parent.sync_all().map_err(|_| StageError::CleanupFailed)
}

fn read_recovery_marker(root: &File) -> Option<[u8; 32]> {
    let marker = open_component(root, c".kit-stage-marker", libc::O_RDONLY).ok()?;
    let bytes = read_marker(marker, 32).ok()?;
    bytes.try_into().ok()
}

fn decode_hex(value: &str) -> Result<Vec<u8>, StageError> {
    if !value.len().is_multiple_of(2)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StageError::CleanupFailed);
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|_| StageError::CleanupFailed)?;
            u8::from_str_radix(pair, 16).map_err(|_| StageError::CleanupFailed)
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(value, "{byte:02x}").expect("writing to a string cannot fail");
    }
    value
}

fn set_mode(file: &File, mode: u32) -> Result<(), StageError> {
    if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0 {
        Err(StageError::Unavailable)
    } else {
        Ok(())
    }
}

fn mkdir_at(directory: &File, name: &CStr, mode: u32) -> Result<(), StageError> {
    if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) } != 0 {
        Err(StageError::Unavailable)
    } else {
        Ok(())
    }
}

fn create_file(directory: &File, name: &CStr) -> Result<File, StageError> {
    let file = open_component_mode(
        directory,
        name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )
    .map_err(|_| StageError::Unavailable)?;
    strip_creation_metadata(&file)?;
    Ok(file)
}

#[cfg(target_os = "macos")]
fn strip_creation_metadata(file: &File) -> Result<(), StageError> {
    let result =
        unsafe { libc::fremovexattr(file.as_raw_fd(), c"com.apple.provenance".as_ptr(), 0) };
    if result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ENOATTR) {
        Ok(())
    } else {
        Err(StageError::Unavailable)
    }
}

#[cfg(target_os = "linux")]
fn strip_creation_metadata(_file: &File) -> Result<(), StageError> {
    Ok(())
}

fn open_component(directory: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    open_component_mode(directory, name, flags, 0)
}

fn open_component_mode(
    directory: &File,
    name: &CStr,
    requested_flags: libc::c_int,
    mode: u32,
) -> io::Result<File> {
    let flags = requested_flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    #[cfg(target_os = "linux")]
    {
        let mut how = unsafe { MaybeUninit::<libc::open_how>::zeroed().assume_init() };
        how.flags = flags as u64;
        how.mode = mode as u64;
        how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
        let descriptor = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                directory.as_raw_fd(),
                name.as_ptr(),
                &how,
                std::mem::size_of::<libc::open_how>(),
            ) as libc::c_int
        };
        if descriptor >= 0 {
            return Ok(unsafe { File::from_raw_fd(descriptor) });
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOSYS) {
            return Err(error);
        }
    }
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags,
            mode as libc::c_uint,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

fn stat_at(directory: &File, name: &CStr) -> io::Result<Stat> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Stat::from_raw(unsafe { stat.assume_init() }))
}

fn stat_file(file: &File) -> io::Result<Stat> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Stat::from_raw(unsafe { stat.assume_init() }))
}

#[derive(Clone, Copy)]
struct Stat {
    device: u64,
    inode: u64,
    links: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl Stat {
    #[cfg(target_os = "macos")]
    fn from_raw(value: libc::stat) -> Self {
        Self {
            device: value.st_dev as u64,
            inode: value.st_ino,
            links: value.st_nlink as u64,
            mode: u32::from(value.st_mode),
            uid: value.st_uid,
            gid: value.st_gid,
            size: value.st_size as u64,
            modified_seconds: value.st_mtime,
            modified_nanoseconds: value.st_mtime_nsec,
            changed_seconds: value.st_ctime,
            changed_nanoseconds: value.st_ctime_nsec,
        }
    }

    #[cfg(target_os = "linux")]
    fn from_raw(value: libc::stat) -> Self {
        Self {
            device: value.st_dev,
            inode: value.st_ino,
            links: value.st_nlink,
            mode: value.st_mode,
            uid: value.st_uid,
            gid: value.st_gid,
            size: value.st_size as u64,
            modified_seconds: value.st_mtime,
            modified_nanoseconds: value.st_mtime_nsec,
            changed_seconds: value.st_ctime,
            changed_nanoseconds: value.st_ctime_nsec,
        }
    }

    fn kind(self) -> u32 {
        self.mode & libc::S_IFMT as u32
    }

    fn same_bound(self, other: Self) -> bool {
        self.device == other.device
            && self.inode == other.inode
            && self.links == other.links
            && self.mode == other.mode
            && self.size == other.size
            && self.modified_seconds == other.modified_seconds
            && self.modified_nanoseconds == other.modified_nanoseconds
            && self.changed_seconds == other.changed_seconds
            && self.changed_nanoseconds == other.changed_nanoseconds
    }
}

fn supported_metadata(file: &File, stat: Stat) -> bool {
    stat.mode & 0o7000 == 0
        && stat.uid == unsafe { libc::geteuid() }
        && stat.gid == unsafe { libc::getegid() }
        && supported_xattrs(file)
        && supported_acl(file)
}

fn supported_directory_metadata(file: &File, stat: Stat) -> bool {
    stat.mode & 0o7000 == 0
        && stat.uid == unsafe { libc::geteuid() }
        && stat.gid == unsafe { libc::getegid() }
        && supported_xattrs(file)
        && supported_acl(file)
}

#[cfg(target_os = "linux")]
fn supported_acl(_file: &File) -> bool {
    // POSIX ACLs are exposed as system.posix_acl_* xattrs and rejected above.
    true
}

#[cfg(target_os = "macos")]
fn supported_acl(file: &File) -> bool {
    use std::{ffi::c_void, os::fd::AsRawFd as _};

    type Acl = *mut c_void;
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> Acl;
        fn acl_get_entry(acl: Acl, entry_id: libc::c_int, entry: *mut *mut c_void) -> libc::c_int;
        fn acl_free(object: *mut c_void) -> libc::c_int;
    }
    const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
    const ACL_FIRST_ENTRY: libc::c_int = 0;

    // SAFETY: the descriptor is live and all returned ACL storage is released below.
    unsafe {
        let acl = acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED);
        if acl.is_null() {
            return io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT);
        }
        let mut entry = std::ptr::null_mut();
        let result = acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry);
        let freed = acl_free(acl);
        result == 0 && entry.is_null() && freed == 0
    }
}

#[cfg(target_os = "linux")]
fn xattr_bytes(file: &File) -> Option<usize> {
    let count = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0) };
    (count >= 0).then_some(count as usize)
}

#[cfg(target_os = "linux")]
fn supported_xattrs(file: &File) -> bool {
    xattr_bytes(file) == Some(0)
}

#[cfg(target_os = "macos")]
fn xattr_bytes(file: &File) -> Option<usize> {
    let count = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0, 0) };
    (count >= 0).then_some(count as usize)
}

#[cfg(target_os = "macos")]
fn supported_xattrs(file: &File) -> bool {
    let Some(size) = xattr_bytes(file) else {
        return false;
    };
    if size == 0 {
        return true;
    }
    if size > 4096 {
        return false;
    }
    let mut names = vec![0u8; size];
    let count =
        unsafe { libc::flistxattr(file.as_raw_fd(), names.as_mut_ptr().cast(), names.len(), 0) };
    count == size as isize
        && names
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .all(|name| name == b"com.apple.provenance")
}

struct DirectoryStream {
    stream: *mut libc::DIR,
}

impl DirectoryStream {
    fn open(directory: &File) -> io::Result<Self> {
        let descriptor = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(descriptor) };
        if stream.is_null() {
            unsafe { libc::close(descriptor) };
            return Err(io::Error::last_os_error());
        }
        unsafe { libc::rewinddir(stream) };
        Ok(Self { stream })
    }

    fn next(&mut self) -> io::Result<Option<&CStr>> {
        clear_errno();
        let entry = unsafe { libc::readdir(self.stream) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(0) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        Ok(Some(unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }))
    }
}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.stream) };
    }
}

fn clear_errno() {
    #[cfg(target_os = "macos")]
    unsafe {
        *libc::__error() = 0;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location() = 0;
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct MountIdentity([u8; 32]);

#[cfg(target_os = "linux")]
fn mount_identity(file: &File) -> io::Result<MountIdentity> {
    const STATX_MNT_ID: u32 = 0x1000;
    let mut statx = MaybeUninit::<libc::statx>::zeroed();
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            STATX_MNT_ID,
            statx.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let statx = unsafe { statx.assume_init() };
    if statx.stx_mask & STATX_MNT_ID == 0 {
        return Err(io::Error::other("mount identity unavailable"));
    }
    Ok(MountIdentity(
        *blake3::hash(&statx.stx_mnt_id.to_le_bytes()).as_bytes(),
    ))
}

#[cfg(target_os = "macos")]
fn mount_identity(file: &File) -> io::Result<MountIdentity> {
    let mut metadata = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(file.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let metadata = unsafe { metadata.assume_init() };
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&metadata.f_fsid as *const libc::fsid_t).cast::<u8>(),
            std::mem::size_of::<libc::fsid_t>(),
        )
    };
    Ok(MountIdentity(*blake3::hash(bytes).as_bytes()))
}

fn check_deadline(deadline: Instant) -> Result<(), StageError> {
    if Instant::now() >= deadline {
        Err(StageError::LimitExceeded(StageLimit::Time))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{mem::ManuallyDrop, os::unix::fs::PermissionsExt as _, time::Duration};

    #[test]
    fn canonical_tree_digest_vector() {
        let snapshot = Snapshot {
            entries: BTreeMap::from([
                (
                    PathBuf::from("src"),
                    FileState {
                        kind: Kind::Directory,
                        digest: [0; 32],
                        mode: 0o755,
                        size: 0,
                        physical: PhysicalState {
                            device: 1,
                            inode: 1,
                            changed_seconds: 1,
                            changed_nanoseconds: 1,
                            mode: 0o755,
                            size: 0,
                        },
                    },
                ),
                (
                    PathBuf::from("src/main.rs"),
                    FileState {
                        kind: Kind::File,
                        digest: [0x5a; 32],
                        mode: 0o644,
                        size: 17,
                        physical: PhysicalState {
                            device: 1,
                            inode: 2,
                            changed_seconds: 1,
                            changed_nanoseconds: 1,
                            mode: 0o644,
                            size: 17,
                        },
                    },
                ),
            ]),
        };
        assert_eq!(
            snapshot_digest(&snapshot),
            "blake3:dad1c04efa2413a0978b806ebc8107267128365ec8c67273d2efea167c477279"
        );
    }

    #[test]
    fn evidence_digest_vector() {
        assert_eq!(
            stage_evidence_digest(
                "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            "blake3:383ef44611deca83e24e20a824957d54c8bc18716387568ca979dab080e32aec"
        );
    }

    #[test]
    fn cleanup_recovery_resumes_every_allocation_state() {
        let root = std::env::temp_dir().join(format!(
            "kit-stage-recovery-{}-{}",
            std::process::id(),
            hex(&random_bytes())
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let parent = File::open(&root).unwrap();

        let bare = CString::new(format!(".kit-stage-{}", "a".repeat(32))).unwrap();
        mkdir_at(&parent, &bare, 0o700).unwrap();
        recover_allocations(&parent, &root).unwrap();
        assert!(stat_at(&parent, &bare).is_ok());

        let original =
            ManuallyDrop::new(Allocation::new(parent.try_clone().unwrap(), root.clone()).unwrap());
        let original_name = original.name.clone();
        recover_allocations(&parent, &root).unwrap();
        assert!(stat_at(&parent, &original_name).is_err());

        let quarantined =
            ManuallyDrop::new(Allocation::new(parent.try_clone().unwrap(), root.clone()).unwrap());
        assert_eq!(
            unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    quarantined.name.as_ptr(),
                    parent.as_raw_fd(),
                    quarantined.quarantine.as_ptr(),
                )
            },
            0
        );
        let quarantine_name = quarantined.quarantine.clone();
        recover_allocations(&parent, &root).unwrap();
        assert!(stat_at(&parent, &quarantine_name).is_err());

        let mut deleted = Allocation::new(parent.try_clone().unwrap(), root.clone()).unwrap();
        deleted.cleanup().unwrap();
        deleted.cleaned = true;
        recover_allocations(&parent, &root).unwrap();

        drop(parent);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_recovery_never_removes_a_replaced_recorded_allocation() {
        let root = std::env::temp_dir().join(format!(
            "kit-stage-replaced-{}-{}",
            std::process::id(),
            hex(&random_bytes())
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let parent = File::open(&root).unwrap();
        let allocation =
            ManuallyDrop::new(Allocation::new(parent.try_clone().unwrap(), root.clone()).unwrap());
        let detached = CString::new(format!(".detached-{}", hex(&random_bytes()))).unwrap();
        assert_eq!(
            unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    allocation.name.as_ptr(),
                    parent.as_raw_fd(),
                    detached.as_ptr(),
                )
            },
            0
        );
        mkdir_at(&parent, &allocation.name, 0o700).unwrap();
        let replacement = open_component(
            &parent,
            &allocation.name,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .unwrap();
        std::mem::forget(replacement);

        assert!(matches!(
            recover_allocations(&parent, &root),
            Err(StageError::CleanupFailed)
        ));
        assert!(stat_at(&parent, &allocation.name).is_ok());
        assert!(stat_at(&parent, &detached).is_ok());

        drop(parent);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checked_read_enforces_deadline_after_each_chunk() {
        let path = std::env::temp_dir().join(format!(
            "kit-stage-slow-read-{}-{}",
            std::process::id(),
            hex(&random_bytes())
        ));
        std::fs::write(&path, vec![b'x'; 128 * 1024]).unwrap();
        let file = File::open(&path).unwrap();
        let before = stat_file(&file).unwrap();
        let mut delayed = false;
        let result = read_file_checked_with(
            file,
            before,
            128 * 1024,
            Instant::now() + Duration::from_millis(5),
            || {
                if !delayed {
                    delayed = true;
                    std::thread::sleep(Duration::from_millis(10));
                }
            },
        );
        assert!(matches!(
            result,
            Err(StageError::LimitExceeded(StageLimit::Time))
        ));
        std::fs::remove_file(path).unwrap();
    }

    fn random_bytes() -> [u8; 8] {
        let mut bytes = [0; 8];
        getrandom::fill(&mut bytes).unwrap();
        bytes
    }
}
