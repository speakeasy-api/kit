use std::{
    fmt, io,
    time::{Duration, Instant},
};

use crate::{
    domain::{
        ids::{CommandId, WorkspaceId},
        lifecycle::{AttemptOwnership, ProcessClaim, ProcessOwnership},
    },
    executor::process::tree::{BoundaryIdentity, Inspection, PersistedBoundary},
    workspace::acquire::AcquisitionResult,
};

mod coordinator;

pub(crate) use coordinator::DurableCancellationConfirmation;
pub use coordinator::{
    DurableBoundaryState, ExecutorCancellationCoordinator, ExecutorCancellationOutcome,
    SqliteCancellationCoordinator,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceIdentity {
    pub workspace_id: WorkspaceId,
    pub acquisition_id: String,
    pub revision: String,
}

impl WorkspaceIdentity {
    pub fn new(
        workspace_id: WorkspaceId,
        acquisition_id: impl Into<String>,
        revision: impl Into<String>,
    ) -> Result<Self, IntentError> {
        let identity = Self {
            workspace_id,
            acquisition_id: acquisition_id.into(),
            revision: revision.into(),
        };
        if valid_identity(&identity.acquisition_id) && valid_identity(&identity.revision) {
            Ok(identity)
        } else {
            Err(IntentError::InvalidWorkspaceIdentity)
        }
    }

    pub fn from_acquisition(workspace_id: WorkspaceId, acquisition: &AcquisitionResult) -> Self {
        Self {
            workspace_id,
            acquisition_id: acquisition.acquisition_id.as_str().to_owned(),
            revision: acquisition.workspace_revision.hash.as_str().to_owned(),
        }
    }
}

/// The policy is part of the durable intent. A clean inspection may repair an
/// unknown outcome only when this persisted policy explicitly permits it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancellationPolicy {
    pub require_kill_confirmation: bool,
    pub require_reap_confirmation: bool,
    pub resolve_unknown_with_matching_zero_survivor_inspection: bool,
}

impl Default for CancellationPolicy {
    fn default() -> Self {
        Self {
            require_kill_confirmation: true,
            require_reap_confirmation: true,
            resolve_unknown_with_matching_zero_survivor_inspection: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationIntent {
    pub request_id: CommandId,
    pub owner: AttemptOwnership,
    pub process: ProcessClaim,
    pub boundary: PersistedBoundary,
    pub workspace: WorkspaceIdentity,
    pub grace_period: Duration,
    pub policy: CancellationPolicy,
}

impl CancellationIntent {
    pub fn new(
        request_id: CommandId,
        owner: AttemptOwnership,
        process: ProcessClaim,
        boundary: PersistedBoundary,
        workspace: WorkspaceIdentity,
        grace_period: Duration,
    ) -> Result<Self, IntentError> {
        if process.owner != ProcessOwnership::Attempt(owner) {
            return Err(IntentError::ProcessOwnerMismatch);
        }
        let encoded_owner = serde_json::to_string(&ProcessOwnership::Attempt(owner))
            .map_err(|_| IntentError::BoundaryOwnerMismatch)?;
        if boundary.ownership.daemon_service() != encoded_owner
            || boundary.ownership.attempt() != process.process_id.to_string()
        {
            return Err(IntentError::BoundaryOwnerMismatch);
        }
        let mut policy = CancellationPolicy::default();
        if boundary.identity.kind()
            == crate::executor::process::tree::BoundaryKind::WindowsComposite
        {
            policy.resolve_unknown_with_matching_zero_survivor_inspection = false;
        }
        Ok(Self {
            request_id,
            owner,
            process,
            boundary,
            workspace,
            grace_period,
            policy,
        })
    }

    pub const fn with_policy(mut self, policy: CancellationPolicy) -> Self {
        self.policy = policy;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancellationPhase {
    IntentPersisted,
    GraceRequested,
    KillRequested,
    ReapRequested,
    InspectRequested,
    Quiescent,
    OutcomeUnknown,
}

impl CancellationPhase {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Quiescent)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancellationOperationKind {
    GraceAndWait,
    KillBoundary,
    ReapDirectChild,
    InspectBoundary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationOperationAttempt {
    pub kind: CancellationOperationKind,
    pub attempt: u32,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutorQuiescence {
    /// Authority that proved quiescence. The process and boundary remain the
    /// immutable identities from the original cancellation intent.
    pub owner: AttemptOwnership,
    pub process: ProcessClaim,
    pub boundary: BoundaryIdentity,
    pub workspace: WorkspaceIdentity,
    pub survivors: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationRecord {
    pub intent: CancellationIntent,
    pub phase: CancellationPhase,
    pub operations: Vec<CancellationOperationAttempt>,
    pub quiescence: Option<ExecutorQuiescence>,
    pub outcome_unknown: Option<String>,
}

impl CancellationRecord {
    pub const fn workspace_reassignable(&self) -> bool {
        matches!(self.phase, CancellationPhase::Quiescent) && self.quiescence.is_some()
    }

    pub const fn blocks_auto_resume(&self) -> bool {
        matches!(self.phase, CancellationPhase::OutcomeUnknown)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancellationEffectKind {
    GraceRequested,
    BoundaryKilled,
    DirectChildReaped,
    BoundaryInspected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationEffect {
    pub request_id: CommandId,
    pub owner: AttemptOwnership,
    pub kind: CancellationEffectKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancellationCompletionStatus {
    Cancelled,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationCompletion {
    pub request_id: CommandId,
    pub owner: AttemptOwnership,
    pub status: CancellationCompletionStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancellationPublication {
    Effect(CancellationEffect),
    Completion(CancellationCompletion),
}

impl CancellationPublication {
    pub const fn owner(&self) -> AttemptOwnership {
        match self {
            Self::Effect(effect) => effect.owner,
            Self::Completion(completion) => completion.owner,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationCommit {
    pub request_id: CommandId,
    /// Current cleanup/reconciliation authority, which may be a successor fence.
    pub owner: AttemptOwnership,
    pub expected_phase: CancellationPhase,
    pub phase: CancellationPhase,
    pub operation: Option<CancellationOperationAttempt>,
    pub quiescence: Option<ExecutorQuiescence>,
    pub outcome_unknown: Option<String>,
    pub publications: Vec<CancellationPublication>,
}

/// Implementations atomically compare current authority, update the record,
/// append the operation result, and publish effects. `load` must permit a newer
/// fence for the same attempt/principal to reconcile an older intent.
pub trait DurableCancellationStore {
    fn request(
        &mut self,
        intent: CancellationIntent,
    ) -> Result<CancellationRecord, CancellationStoreError>;

    fn load(
        &mut self,
        request_id: CommandId,
        authority: AttemptOwnership,
    ) -> Result<CancellationRecord, CancellationStoreError>;

    fn commit(
        &mut self,
        commit: CancellationCommit,
    ) -> Result<CancellationRecord, CancellationStoreError>;
}

/// Operations are idempotent: a crash after the external effect but before the
/// durable commit deliberately repeats the operation named by the durable phase.
pub trait CancellationControl {
    fn boundary_identity(&self) -> &BoundaryIdentity;

    /// Requests graceful termination and waits until the direct child exits or
    /// the supplied grace deadline is reached.
    fn request_grace_and_wait(
        &mut self,
        process: &ProcessClaim,
        boundary: &PersistedBoundary,
        deadline: Instant,
    ) -> io::Result<()>;

    fn kill_complete_boundary(
        &mut self,
        boundary: &PersistedBoundary,
        deadline: Instant,
    ) -> io::Result<()>;

    fn reap_direct_child(&mut self, process: &ProcessClaim, deadline: Instant) -> io::Result<()>;

    fn inspect_boundary(
        &mut self,
        boundary: &PersistedBoundary,
        deadline: Instant,
    ) -> io::Result<Inspection>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancellationStoreError {
    Unauthorized,
    StaleOwner,
    IdempotencyConflict,
    PhaseConflict,
    NotFound,
    Unavailable(String),
}

impl fmt::Display for CancellationStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => formatter.write_str("cancellation owner is not authorized"),
            Self::StaleOwner => formatter.write_str("cancellation attempt or fence is stale"),
            Self::IdempotencyConflict => {
                formatter.write_str("cancellation request ID was reused for another intent")
            }
            Self::PhaseConflict => formatter.write_str("cancellation phase changed concurrently"),
            Self::NotFound => formatter.write_str("cancellation intent was not found"),
            Self::Unavailable(detail) => {
                write!(formatter, "cancellation store unavailable: {detail}")
            }
        }
    }
}

impl std::error::Error for CancellationStoreError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentError {
    ProcessOwnerMismatch,
    BoundaryOwnerMismatch,
    InvalidWorkspaceIdentity,
}

impl fmt::Display for IntentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProcessOwnerMismatch => {
                formatter.write_str("process is not owned by the attempt")
            }
            Self::BoundaryOwnerMismatch => {
                formatter.write_str("process boundary is not owned by the process and attempt")
            }
            Self::InvalidWorkspaceIdentity => formatter.write_str("invalid workspace identity"),
        }
    }
}

impl std::error::Error for IntentError {}

#[derive(Debug)]
pub enum CancellationError {
    Store(CancellationStoreError),
    InvalidTimeout,
}

impl fmt::Display for CancellationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(formatter),
            Self::InvalidTimeout => {
                formatter.write_str("cancellation timeout is zero or overflows")
            }
        }
    }
}

impl std::error::Error for CancellationError {}

impl From<CancellationStoreError> for CancellationError {
    fn from(error: CancellationStoreError) -> Self {
        Self::Store(error)
    }
}

pub fn request_cancellation(
    store: &mut impl DurableCancellationStore,
    control: &mut impl CancellationControl,
    intent: CancellationIntent,
    operation_timeout: Duration,
) -> Result<CancellationRecord, CancellationError> {
    let request_id = intent.request_id;
    let authority = intent.owner;
    store.request(intent)?;
    reconcile_cancellation(store, control, request_id, authority, operation_timeout)
}

pub fn reconcile_cancellation(
    store: &mut impl DurableCancellationStore,
    control: &mut impl CancellationControl,
    request_id: CommandId,
    authority: AttemptOwnership,
    operation_timeout: Duration,
) -> Result<CancellationRecord, CancellationError> {
    if operation_timeout.is_zero() {
        return Err(CancellationError::InvalidTimeout);
    }

    loop {
        let record = store.load(request_id, authority)?;
        if record.phase == CancellationPhase::Quiescent {
            return Ok(record);
        }
        if control.boundary_identity() != &record.intent.boundary.identity {
            return if record.phase == CancellationPhase::OutcomeUnknown {
                Ok(record)
            } else {
                mark_outcome_unknown(
                    store,
                    &record,
                    authority,
                    None,
                    "persisted boundary identity cannot be targeted safely".to_owned(),
                )
            };
        }

        match record.phase {
            CancellationPhase::IntentPersisted => {
                commit_phase(
                    store,
                    &record,
                    authority,
                    CancellationPhase::GraceRequested,
                    None,
                    Vec::new(),
                )?;
            }
            CancellationPhase::GraceRequested => {
                let grace = record.intent.grace_period.min(operation_timeout);
                let result = operation_deadline(grace).and_then(|deadline| {
                    control.request_grace_and_wait(
                        &record.intent.process,
                        &record.intent.boundary,
                        deadline,
                    )
                });
                commit_operation(
                    store,
                    &record,
                    authority,
                    CancellationPhase::KillRequested,
                    CancellationOperationKind::GraceAndWait,
                    CancellationEffectKind::GraceRequested,
                    result,
                )?;
            }
            CancellationPhase::KillRequested => {
                let result = operation_deadline(operation_timeout).and_then(|deadline| {
                    control.kill_complete_boundary(&record.intent.boundary, deadline)
                });
                commit_operation(
                    store,
                    &record,
                    authority,
                    CancellationPhase::ReapRequested,
                    CancellationOperationKind::KillBoundary,
                    CancellationEffectKind::BoundaryKilled,
                    result,
                )?;
            }
            CancellationPhase::ReapRequested => {
                let result = operation_deadline(operation_timeout).and_then(|deadline| {
                    control.reap_direct_child(&record.intent.process, deadline)
                });
                commit_operation(
                    store,
                    &record,
                    authority,
                    CancellationPhase::InspectRequested,
                    CancellationOperationKind::ReapDirectChild,
                    CancellationEffectKind::DirectChildReaped,
                    result,
                )?;
            }
            CancellationPhase::InspectRequested => {
                return inspect_and_finish(
                    store,
                    control,
                    &record,
                    authority,
                    operation_timeout,
                    false,
                );
            }
            CancellationPhase::OutcomeUnknown => {
                // One re-inspection per call prevents a hot loop while preserving
                // a durable path from unknown to proven quiescence.
                return inspect_and_finish(
                    store,
                    control,
                    &record,
                    authority,
                    operation_timeout,
                    true,
                );
            }
            CancellationPhase::Quiescent => unreachable!(),
        }
    }
}

fn commit_operation(
    store: &mut impl DurableCancellationStore,
    record: &CancellationRecord,
    authority: AttemptOwnership,
    phase: CancellationPhase,
    kind: CancellationOperationKind,
    effect_kind: CancellationEffectKind,
    result: io::Result<()>,
) -> Result<CancellationRecord, CancellationStoreError> {
    let operation = operation_attempt(record, kind, result.as_ref().err());
    let publications = if result.is_ok() {
        vec![effect(record.intent.request_id, authority, effect_kind)]
    } else {
        Vec::new()
    };
    commit_phase(
        store,
        record,
        authority,
        phase,
        Some(operation),
        publications,
    )
}

fn commit_phase(
    store: &mut impl DurableCancellationStore,
    record: &CancellationRecord,
    authority: AttemptOwnership,
    phase: CancellationPhase,
    operation: Option<CancellationOperationAttempt>,
    publications: Vec<CancellationPublication>,
) -> Result<CancellationRecord, CancellationStoreError> {
    store.commit(CancellationCommit {
        request_id: record.intent.request_id,
        owner: authority,
        expected_phase: record.phase,
        phase,
        operation,
        quiescence: None,
        outcome_unknown: None,
        publications,
    })
}

fn inspect_and_finish(
    store: &mut impl DurableCancellationStore,
    control: &mut impl CancellationControl,
    record: &CancellationRecord,
    authority: AttemptOwnership,
    operation_timeout: Duration,
    reconciling_unknown: bool,
) -> Result<CancellationRecord, CancellationError> {
    let inspected = operation_deadline(operation_timeout)
        .and_then(|deadline| control.inspect_boundary(&record.intent.boundary, deadline));
    let operation = operation_attempt(
        record,
        CancellationOperationKind::InspectBoundary,
        inspected.as_ref().err(),
    );
    let inspection = match inspected {
        Ok(inspection) => inspection,
        Err(error) => {
            return mark_outcome_unknown(
                store,
                record,
                authority,
                Some(operation),
                format!("boundary inspection was not confirmed: {error}"),
            );
        }
    };
    if inspection.identity != record.intent.boundary.identity {
        return mark_outcome_unknown(
            store,
            record,
            authority,
            Some(operation),
            "inspection returned a different boundary identity".to_owned(),
        );
    }
    if inspection.survivors != Some(0) || !inspection.quiescent {
        return mark_outcome_unknown(
            store,
            record,
            authority,
            Some(operation),
            format!(
                "inspection did not prove quiescence ({:?} survivors)",
                inspection.survivors
            ),
        );
    }

    let mandatory_error = (!operation_succeeded(record, CancellationOperationKind::KillBoundary)
        && record.intent.policy.require_kill_confirmation)
        || (!operation_succeeded(record, CancellationOperationKind::ReapDirectChild)
            && record.intent.policy.require_reap_confirmation);
    if mandatory_error
        && !(reconciling_unknown
            && record
                .intent
                .policy
                .resolve_unknown_with_matching_zero_survivor_inspection
            && record.intent.boundary.identity.kind()
                != crate::executor::process::tree::BoundaryKind::WindowsComposite)
    {
        return mark_outcome_unknown(
            store,
            record,
            authority,
            Some(operation),
            "mandatory kill or reap confirmation is missing".to_owned(),
        );
    }

    let quiescence = ExecutorQuiescence {
        owner: authority,
        process: record.intent.process,
        boundary: inspection.identity,
        workspace: record.intent.workspace.clone(),
        survivors: 0,
    };
    Ok(store.commit(CancellationCommit {
        request_id: record.intent.request_id,
        owner: authority,
        expected_phase: record.phase,
        phase: CancellationPhase::Quiescent,
        operation: Some(operation),
        quiescence: Some(quiescence),
        outcome_unknown: None,
        publications: vec![
            effect(
                record.intent.request_id,
                authority,
                CancellationEffectKind::BoundaryInspected,
            ),
            CancellationPublication::Completion(CancellationCompletion {
                request_id: record.intent.request_id,
                owner: authority,
                status: CancellationCompletionStatus::Cancelled,
            }),
        ],
    })?)
}

fn mark_outcome_unknown(
    store: &mut impl DurableCancellationStore,
    record: &CancellationRecord,
    authority: AttemptOwnership,
    operation: Option<CancellationOperationAttempt>,
    reason: String,
) -> Result<CancellationRecord, CancellationError> {
    let publications = if record.phase == CancellationPhase::OutcomeUnknown {
        Vec::new()
    } else {
        vec![CancellationPublication::Completion(
            CancellationCompletion {
                request_id: record.intent.request_id,
                owner: authority,
                status: CancellationCompletionStatus::OutcomeUnknown,
            },
        )]
    };
    Ok(store.commit(CancellationCommit {
        request_id: record.intent.request_id,
        owner: authority,
        expected_phase: record.phase,
        phase: CancellationPhase::OutcomeUnknown,
        operation,
        quiescence: None,
        outcome_unknown: Some(reason),
        publications,
    })?)
}

fn operation_attempt(
    record: &CancellationRecord,
    kind: CancellationOperationKind,
    error: Option<&io::Error>,
) -> CancellationOperationAttempt {
    CancellationOperationAttempt {
        kind,
        attempt: record
            .operations
            .iter()
            .filter(|operation| operation.kind == kind)
            .count()
            .saturating_add(1)
            .try_into()
            .unwrap_or(u32::MAX),
        error: error.map(ToString::to_string),
    }
}

fn operation_succeeded(record: &CancellationRecord, kind: CancellationOperationKind) -> bool {
    record
        .operations
        .iter()
        .rev()
        .find(|operation| operation.kind == kind)
        .is_some_and(|operation| operation.error.is_none())
}

fn effect(
    request_id: CommandId,
    owner: AttemptOwnership,
    kind: CancellationEffectKind,
) -> CancellationPublication {
    CancellationPublication::Effect(CancellationEffect {
        request_id,
        owner,
        kind,
    })
}

fn operation_deadline(timeout: Duration) -> io::Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "operation deadline overflowed"))
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.contains(['\0', '\n', '\r'])
}
