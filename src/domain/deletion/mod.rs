use std::{collections::BTreeMap, fmt, str::FromStr};

use crate::domain::{
    ids::{PrincipalId, ProjectId},
    lifecycle::FencingToken,
    retention::{
        ArtifactReference, ArtifactReferenceId, BackupGeneration, BackupGenerationId,
        DeletionBlocker, EarliestPhysicalDeletion, LegalHold, LegalHoldId,
        PhysicalDeletionDecision, RetainedObject, RetentionIntent, RetentionObjectId,
        RetentionPolicy, StoreTimestamp, evaluate_physical_deletion_at,
    },
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct DeletionJobId(u128);

impl DeletionJobId {
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u128 {
        self.0
    }
}

impl fmt::Display for DeletionJobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "deletion_{:032x}", self.0)
    }
}

impl FromStr for DeletionJobId {
    type Err = DeletionJobIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value
            .strip_prefix("deletion_")
            .ok_or(DeletionJobIdParseError)?;
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DeletionJobIdParseError);
        }
        u128::from_str_radix(value, 16)
            .map(Self)
            .map_err(|_| DeletionJobIdParseError)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeletionJobIdParseError;

impl fmt::Display for DeletionJobIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid deletion job identifier")
    }
}

impl std::error::Error for DeletionJobIdParseError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeletionActor {
    pub principal_id: PrincipalId,
    pub project_id: ProjectId,
}

impl DeletionActor {
    pub const fn new(principal_id: PrincipalId, project_id: ProjectId) -> Self {
        Self {
            principal_id,
            project_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeletionJobState {
    Requested,
    Evaluating,
    WaitingForPolicy,
    PhysicallyDeleting,
    Completed,
    Blocked,
    Failed,
}

impl DeletionJobState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Evaluating => "evaluating",
            Self::WaitingForPolicy => "waiting_for_policy",
            Self::PhysicallyDeleting => "physically_deleting",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "requested" => Some(Self::Requested),
            "evaluating" => Some(Self::Evaluating),
            "waiting_for_policy" => Some(Self::WaitingForPolicy),
            "physically_deleting" => Some(Self::PhysicallyDeleting),
            "completed" => Some(Self::Completed),
            "blocked" => Some(Self::Blocked),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PublicDeletionBlocker {
    RetentionPolicy,
    LegalHold,
    ActiveReference,
    BackupGeneration,
}

impl PublicDeletionBlocker {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RetentionPolicy => "retention_policy",
            Self::LegalHold => "legal_hold",
            Self::ActiveReference => "active_reference",
            Self::BackupGeneration => "backup_generation",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "retention_policy" => Some(Self::RetentionPolicy),
            "legal_hold" => Some(Self::LegalHold),
            "active_reference" => Some(Self::ActiveReference),
            "backup_generation" => Some(Self::BackupGeneration),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectiveRetention {
    pub policy: RetentionPolicy,
    pub earliest_physical_deletion: EarliestPhysicalDeletion,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionAuditEntry {
    pub sequence: u64,
    pub state: DeletionJobState,
    pub at: StoreTimestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionJob {
    pub id: DeletionJobId,
    pub object_id: RetentionObjectId,
    pub state: DeletionJobState,
    pub version: u64,
    pub actor: DeletionActor,
    pub resource_version: u64,
    pub effective_retention: EffectiveRetention,
    pub blockers: Vec<PublicDeletionBlocker>,
    pub fence: FencingToken,
    pub requested_at: StoreTimestamp,
    pub completed_at: Option<StoreTimestamp>,
    pub failure: Option<String>,
    pub audit: Vec<DeletionAuditEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveStatus {
    pub object_id: RetentionObjectId,
    pub archived: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeletionError {
    NotFound,
    InvalidIdempotencyKey,
    IdempotencyConflict,
    LegalHold {
        job_id: DeletionJobId,
        earliest_physical_deletion: EarliestPhysicalDeletion,
    },
    StaleFence {
        current: FencingToken,
    },
    InvalidState(DeletionJobState),
}

impl fmt::Display for DeletionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("resource not found"),
            Self::InvalidIdempotencyKey => f.write_str("invalid idempotency key"),
            Self::IdempotencyConflict => f.write_str("idempotency key reused with different input"),
            Self::LegalHold { .. } => f.write_str("physical deletion is blocked by legal hold"),
            Self::StaleFence { .. } => f.write_str("deletion worker fence is stale"),
            Self::InvalidState(state) => write!(f, "job cannot execute from {}", state.as_str()),
        }
    }
}

impl std::error::Error for DeletionError {}

#[derive(Clone, Copy)]
struct ObjectRecord {
    object: RetainedObject,
    archived: bool,
}

pub struct DeletionService {
    objects: BTreeMap<RetentionObjectId, ObjectRecord>,
    project_owners: BTreeMap<ProjectId, PrincipalId>,
    policies: BTreeMap<ProjectId, RetentionPolicy>,
    holds: BTreeMap<LegalHoldId, LegalHold>,
    references: BTreeMap<ArtifactReferenceId, ArtifactReference>,
    backups: BTreeMap<BackupGenerationId, BackupGeneration>,
    jobs: BTreeMap<DeletionJobId, DeletionJob>,
    deletion_keys: BTreeMap<(PrincipalId, RetentionObjectId, String), DeletionJobId>,
    archive_keys: BTreeMap<(PrincipalId, RetentionObjectId, String), bool>,
    policy_keys: BTreeMap<(PrincipalId, ProjectId, String), RetentionPolicy>,
    inventory_fence: u64,
    next_job_id: u128,
}

impl Default for DeletionService {
    fn default() -> Self {
        Self {
            objects: BTreeMap::new(),
            project_owners: BTreeMap::new(),
            policies: BTreeMap::new(),
            holds: BTreeMap::new(),
            references: BTreeMap::new(),
            backups: BTreeMap::new(),
            jobs: BTreeMap::new(),
            deletion_keys: BTreeMap::new(),
            archive_keys: BTreeMap::new(),
            policy_keys: BTreeMap::new(),
            inventory_fence: 1,
            next_job_id: 1,
        }
    }
}

impl DeletionService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_object(&mut self, object: RetainedObject, policy: RetentionPolicy) {
        self.project_owners
            .entry(object.project_id)
            .or_insert(object.principal_id);
        self.policies.entry(object.project_id).or_insert(policy);
        self.objects.insert(
            object.id,
            ObjectRecord {
                object,
                archived: false,
            },
        );
        self.advance_inventory_fence();
    }

    pub fn set_effective_policy(
        &mut self,
        actor: DeletionActor,
        project_id: ProjectId,
        policy: RetentionPolicy,
        idempotency_key: &str,
    ) -> Result<RetentionPolicy, DeletionError> {
        validate_key(idempotency_key)?;
        self.authorize_project(actor, project_id)?;
        let scope = (actor.principal_id, project_id, idempotency_key.to_owned());
        if let Some(existing) = self.policy_keys.get(&scope) {
            return if *existing == policy {
                Ok(*existing)
            } else {
                Err(DeletionError::IdempotencyConflict)
            };
        }
        self.policies.insert(project_id, policy);
        self.policy_keys.insert(scope, policy);
        self.advance_inventory_fence();
        Ok(policy)
    }

    pub fn effective_project_policy(
        &self,
        actor: DeletionActor,
        project_id: ProjectId,
    ) -> Result<RetentionPolicy, DeletionError> {
        self.authorize_project(actor, project_id)?;
        self.policies
            .get(&project_id)
            .copied()
            .ok_or(DeletionError::NotFound)
    }

    pub fn archive(
        &mut self,
        actor: DeletionActor,
        object_id: RetentionObjectId,
        archived: bool,
        idempotency_key: &str,
    ) -> Result<ArchiveStatus, DeletionError> {
        validate_key(idempotency_key)?;
        self.authorize_object(actor, object_id)?;
        let scope = (actor.principal_id, object_id, idempotency_key.to_owned());
        if let Some(existing) = self.archive_keys.get(&scope) {
            return if *existing == archived {
                Ok(ArchiveStatus {
                    object_id,
                    archived: *existing,
                })
            } else {
                Err(DeletionError::IdempotencyConflict)
            };
        }
        self.objects
            .get_mut(&object_id)
            .expect("authorized object exists")
            .archived = archived;
        self.archive_keys.insert(scope, archived);
        Ok(ArchiveStatus {
            object_id,
            archived,
        })
    }

    pub fn archive_status(
        &self,
        actor: DeletionActor,
        object_id: RetentionObjectId,
    ) -> Result<ArchiveStatus, DeletionError> {
        self.authorize_object(actor, object_id)?;
        Ok(ArchiveStatus {
            object_id,
            archived: self.objects[&object_id].archived,
        })
    }

    pub fn put_legal_hold(&mut self, hold: LegalHold) {
        self.holds.insert(hold.id, hold);
        self.advance_inventory_fence();
    }

    pub fn remove_legal_hold(&mut self, id: LegalHoldId) -> Option<LegalHold> {
        let removed = self.holds.remove(&id);
        if removed.is_some() {
            self.advance_inventory_fence();
        }
        removed
    }

    pub fn put_artifact_reference(&mut self, reference: ArtifactReference) {
        self.references.insert(reference.id, reference);
        self.advance_inventory_fence();
    }

    pub fn remove_artifact_reference(
        &mut self,
        id: ArtifactReferenceId,
    ) -> Option<ArtifactReference> {
        let removed = self.references.remove(&id);
        if removed.is_some() {
            self.advance_inventory_fence();
        }
        removed
    }

    pub fn put_backup_generation(&mut self, generation: BackupGeneration) {
        self.backups.insert(generation.id, generation);
        self.advance_inventory_fence();
    }

    pub fn remove_backup_generation(&mut self, id: BackupGenerationId) -> Option<BackupGeneration> {
        let removed = self.backups.remove(&id);
        if removed.is_some() {
            self.advance_inventory_fence();
        }
        removed
    }

    pub fn retention_for_object(
        &self,
        actor: DeletionActor,
        object_id: RetentionObjectId,
        now: StoreTimestamp,
    ) -> Result<EffectiveRetention, DeletionError> {
        let (_, policy, decision) = self.evaluate(actor, object_id, now)?;
        Ok(EffectiveRetention {
            policy,
            earliest_physical_deletion: decision.earliest,
        })
    }

    pub fn request_deletion(
        &mut self,
        actor: DeletionActor,
        object_id: RetentionObjectId,
        idempotency_key: &str,
        now: StoreTimestamp,
    ) -> Result<DeletionJob, DeletionError> {
        validate_key(idempotency_key)?;
        self.authorize_object(actor, object_id)?;
        let scope = (actor.principal_id, object_id, idempotency_key.to_owned());
        if let Some(id) = self.deletion_keys.get(&scope) {
            return self.job_result(*id);
        }

        let id = DeletionJobId::new(self.next_job_id);
        self.next_job_id = self.next_job_id.checked_add(1).unwrap_or(1);
        let policy = self.policy_for(object_id)?;
        let job = DeletionJob {
            id,
            object_id,
            state: DeletionJobState::Requested,
            version: 1,
            actor,
            resource_version: 1,
            effective_retention: EffectiveRetention {
                policy,
                earliest_physical_deletion: EarliestPhysicalDeletion::At(now),
            },
            blockers: Vec::new(),
            fence: FencingToken::new(self.inventory_fence),
            requested_at: now,
            completed_at: None,
            failure: None,
            audit: vec![DeletionAuditEntry {
                sequence: 1,
                state: DeletionJobState::Requested,
                at: now,
            }],
        };
        self.jobs.insert(id, job);
        self.deletion_keys.insert(scope, id);
        self.reevaluate_job(id, now)?;
        self.job_result(id)
    }

    pub fn reevaluate_job(
        &mut self,
        id: DeletionJobId,
        now: StoreTimestamp,
    ) -> Result<DeletionJob, DeletionError> {
        let job = self.jobs.get(&id).ok_or(DeletionError::NotFound)?;
        if job.state == DeletionJobState::Completed {
            return Ok(job.clone());
        }
        let object_id = job.object_id;
        let (_, policy, decision) = self.evaluate_unchecked(object_id, now)?;
        self.transition(id, DeletionJobState::Evaluating, now);
        self.apply_decision(id, policy, &decision, now);
        self.job_result(id)
    }

    pub fn job(
        &self,
        actor: DeletionActor,
        id: DeletionJobId,
    ) -> Result<DeletionJob, DeletionError> {
        let job = self.jobs.get(&id).ok_or(DeletionError::NotFound)?;
        self.authorize_object(actor, job.object_id)?;
        Ok(job.clone())
    }

    pub fn execute_job<E>(
        &mut self,
        actor: DeletionActor,
        id: DeletionJobId,
        expected_fence: FencingToken,
        now: StoreTimestamp,
        physically_delete: impl FnOnce(RetainedObject) -> Result<(), E>,
    ) -> Result<DeletionJob, DeletionError>
    where
        E: fmt::Display,
    {
        let object_id = self.jobs.get(&id).ok_or(DeletionError::NotFound)?.object_id;
        self.authorize_object(actor, object_id)?;
        let current_job = self.jobs.get(&id).expect("job exists");
        if current_job.state == DeletionJobState::Completed {
            return Ok(current_job.clone());
        }
        if current_job.fence != expected_fence || expected_fence.get() != self.inventory_fence {
            let _ = self.reevaluate_job(id, now);
            return Err(DeletionError::StaleFence {
                current: self.jobs[&id].fence,
            });
        }

        let (object, policy, decision) = self.evaluate_unchecked(object_id, now)?;
        self.transition(id, DeletionJobState::Evaluating, now);
        self.apply_decision(id, policy, &decision, now);
        if !decision.physically_deletable {
            return self.job_result(id);
        }

        self.transition(id, DeletionJobState::PhysicallyDeleting, now);
        match physically_delete(object) {
            Ok(()) => {
                self.transition(id, DeletionJobState::Completed, now);
                self.jobs.get_mut(&id).expect("job exists").completed_at = Some(now);
                Ok(self.jobs[&id].clone())
            }
            Err(error) => {
                self.transition(id, DeletionJobState::Failed, now);
                self.jobs.get_mut(&id).expect("job exists").failure = Some(error.to_string());
                Ok(self.jobs[&id].clone())
            }
        }
    }

    fn evaluate(
        &self,
        actor: DeletionActor,
        object_id: RetentionObjectId,
        now: StoreTimestamp,
    ) -> Result<(RetainedObject, RetentionPolicy, PhysicalDeletionDecision), DeletionError> {
        self.authorize_object(actor, object_id)?;
        self.evaluate_unchecked(object_id, now)
    }

    fn evaluate_unchecked(
        &self,
        object_id: RetentionObjectId,
        now: StoreTimestamp,
    ) -> Result<(RetainedObject, RetentionPolicy, PhysicalDeletionDecision), DeletionError> {
        let object = self
            .objects
            .get(&object_id)
            .map(|record| record.object)
            .ok_or(DeletionError::NotFound)?;
        let policy = self.policy_for(object_id)?;
        let holds = self.holds.values().copied().collect::<Vec<_>>();
        let references = self.references.values().copied().collect::<Vec<_>>();
        let backups = self.backups.values().cloned().collect::<Vec<_>>();
        let decision = evaluate_physical_deletion_at(
            now,
            &object,
            RetentionIntent::Delete,
            policy,
            &holds,
            &references,
            &backups,
        );
        Ok((object, policy, decision))
    }

    fn apply_decision(
        &mut self,
        id: DeletionJobId,
        policy: RetentionPolicy,
        decision: &PhysicalDeletionDecision,
        now: StoreTimestamp,
    ) {
        let blockers = public_blockers(&decision.blockers);
        let held = blockers.contains(&PublicDeletionBlocker::LegalHold);
        let fence = self.inventory_fence;
        let job = self.jobs.get_mut(&id).expect("job exists");
        job.effective_retention = EffectiveRetention {
            policy,
            earliest_physical_deletion: decision.earliest,
        };
        job.blockers = blockers;
        job.fence = FencingToken::new(fence);
        self.transition(
            id,
            if held {
                DeletionJobState::Blocked
            } else {
                DeletionJobState::WaitingForPolicy
            },
            now,
        );
    }

    fn transition(&mut self, id: DeletionJobId, state: DeletionJobState, at: StoreTimestamp) {
        let job = self.jobs.get_mut(&id).expect("job exists");
        job.state = state;
        job.version = job.version.saturating_add(1);
        job.audit.push(DeletionAuditEntry {
            sequence: job.audit.len() as u64 + 1,
            state,
            at,
        });
    }

    fn job_result(&self, id: DeletionJobId) -> Result<DeletionJob, DeletionError> {
        let job = self.jobs.get(&id).ok_or(DeletionError::NotFound)?.clone();
        if job.blockers.contains(&PublicDeletionBlocker::LegalHold) {
            Err(DeletionError::LegalHold {
                job_id: id,
                earliest_physical_deletion: job.effective_retention.earliest_physical_deletion,
            })
        } else {
            Ok(job)
        }
    }

    fn policy_for(&self, object_id: RetentionObjectId) -> Result<RetentionPolicy, DeletionError> {
        let project_id = self
            .objects
            .get(&object_id)
            .map(|record| record.object.project_id)
            .ok_or(DeletionError::NotFound)?;
        self.policies
            .get(&project_id)
            .copied()
            .ok_or(DeletionError::NotFound)
    }

    fn authorize_object(
        &self,
        actor: DeletionActor,
        object_id: RetentionObjectId,
    ) -> Result<(), DeletionError> {
        match self.objects.get(&object_id) {
            Some(record)
                if record.object.principal_id == actor.principal_id
                    && record.object.project_id == actor.project_id =>
            {
                Ok(())
            }
            _ => Err(DeletionError::NotFound),
        }
    }

    fn authorize_project(
        &self,
        actor: DeletionActor,
        project_id: ProjectId,
    ) -> Result<(), DeletionError> {
        match self.project_owners.get(&project_id) {
            Some(owner) if *owner == actor.principal_id && actor.project_id == project_id => Ok(()),
            _ => Err(DeletionError::NotFound),
        }
    }

    fn advance_inventory_fence(&mut self) {
        self.inventory_fence = self.inventory_fence.checked_add(1).unwrap_or(1);
    }
}

fn validate_key(key: &str) -> Result<(), DeletionError> {
    if key.is_empty() || key.len() > 255 || key.bytes().any(|byte| !byte.is_ascii_graphic()) {
        Err(DeletionError::InvalidIdempotencyKey)
    } else {
        Ok(())
    }
}

fn public_blockers(blockers: &[DeletionBlocker]) -> Vec<PublicDeletionBlocker> {
    let mut public = blockers
        .iter()
        .filter_map(|blocker| match blocker {
            DeletionBlocker::ArchiveIntent => None,
            DeletionBlocker::Retention(_) => Some(PublicDeletionBlocker::RetentionPolicy),
            DeletionBlocker::LegalHold(_) => Some(PublicDeletionBlocker::LegalHold),
            DeletionBlocker::ArtifactReference(_) => Some(PublicDeletionBlocker::ActiveReference),
            DeletionBlocker::BackupGeneration(_) => Some(PublicDeletionBlocker::BackupGeneration),
        })
        .collect::<Vec<_>>();
    public.sort_unstable();
    public.dedup();
    public
}
