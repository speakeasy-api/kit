use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::domain::ids::{
    ArtifactId, EventId, ExperimentId, PrincipalId, ProjectId, TerminalId, ThreadId,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct StoreTimestamp(i64);

impl StoreTimestamp {
    pub const fn from_unix_micros(unix_micros: i64) -> Self {
        Self(unix_micros)
    }

    pub const fn unix_micros(self) -> i64 {
        self.0
    }
}

macro_rules! numeric_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(transparent)]
        pub struct $name(u128);

        impl $name {
            pub const fn new(value: u128) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u128 {
                self.0
            }
        }
    };
}

numeric_id!(LegalHoldId);
numeric_id!(ArtifactReferenceId);
numeric_id!(BackupGenerationId);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RetentionClass {
    Event,
    Transcript,
    Terminal,
    Artifact,
    Experiment,
    Backup,
}

impl RetentionClass {
    pub const ALL: [Self; 6] = [
        Self::Event,
        Self::Transcript,
        Self::Terminal,
        Self::Artifact,
        Self::Experiment,
        Self::Backup,
    ];
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionPeriod {
    ForMicros(u64),
    Forever,
}

impl RetentionPeriod {
    pub const fn for_micros(micros: u64) -> Self {
        Self::ForMicros(micros)
    }

    pub fn expiration_from(self, stored_at: StoreTimestamp) -> Expiration {
        match self {
            Self::ForMicros(micros) => i64::try_from(micros)
                .ok()
                .and_then(|micros| stored_at.0.checked_add(micros))
                .map(StoreTimestamp)
                .map(Expiration::At)
                .unwrap_or(Expiration::Never),
            Self::Forever => Expiration::Never,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RetentionPolicy {
    pub event: RetentionPeriod,
    pub transcript: RetentionPeriod,
    pub terminal: RetentionPeriod,
    pub artifact: RetentionPeriod,
    pub experiment: RetentionPeriod,
    pub backup: RetentionPeriod,
}

impl RetentionPolicy {
    pub const FOREVER: Self = Self {
        event: RetentionPeriod::Forever,
        transcript: RetentionPeriod::Forever,
        terminal: RetentionPeriod::Forever,
        artifact: RetentionPeriod::Forever,
        experiment: RetentionPeriod::Forever,
        backup: RetentionPeriod::Forever,
    };

    pub const fn period_for(self, class: RetentionClass) -> RetentionPeriod {
        match class {
            RetentionClass::Event => self.event,
            RetentionClass::Transcript => self.transcript,
            RetentionClass::Terminal => self.terminal,
            RetentionClass::Artifact => self.artifact,
            RetentionClass::Experiment => self.experiment,
            RetentionClass::Backup => self.backup,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RetentionObjectId {
    Event(EventId),
    Transcript(ThreadId),
    Terminal(TerminalId),
    Artifact(ArtifactId),
    Experiment(ExperimentId),
    Backup(BackupGenerationId),
}

impl RetentionObjectId {
    pub const fn class(self) -> RetentionClass {
        match self {
            Self::Event(_) => RetentionClass::Event,
            Self::Transcript(_) => RetentionClass::Transcript,
            Self::Terminal(_) => RetentionClass::Terminal,
            Self::Artifact(_) => RetentionClass::Artifact,
            Self::Experiment(_) => RetentionClass::Experiment,
            Self::Backup(_) => RetentionClass::Backup,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetainedObject {
    pub id: RetentionObjectId,
    pub principal_id: PrincipalId,
    pub project_id: ProjectId,
    pub stored_at: StoreTimestamp,
}

impl RetainedObject {
    pub const fn new(
        id: RetentionObjectId,
        principal_id: PrincipalId,
        project_id: ProjectId,
        stored_at: StoreTimestamp,
    ) -> Self {
        Self {
            id,
            principal_id,
            project_id,
            stored_at,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionIntent {
    Archive,
    Delete,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Expiration {
    At(StoreTimestamp),
    Never,
}

impl Expiration {
    pub const fn blocks_at(self, now: StoreTimestamp) -> bool {
        match self {
            Self::At(expires_at) => now.0 < expires_at.0,
            Self::Never => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegalHoldScope {
    Principal(PrincipalId),
    Project(ProjectId),
    Object(RetentionObjectId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegalHold {
    pub id: LegalHoldId,
    pub scope: LegalHoldScope,
    pub placed_at: StoreTimestamp,
    pub released_at: Option<StoreTimestamp>,
}

impl LegalHold {
    pub const fn active(id: LegalHoldId, scope: LegalHoldScope, placed_at: StoreTimestamp) -> Self {
        Self {
            id,
            scope,
            placed_at,
            released_at: None,
        }
    }

    pub const fn released(
        id: LegalHoldId,
        scope: LegalHoldScope,
        placed_at: StoreTimestamp,
        released_at: StoreTimestamp,
    ) -> Self {
        Self {
            id,
            scope,
            placed_at,
            released_at: Some(released_at),
        }
    }

    pub fn covers(self, object: &RetainedObject) -> bool {
        match self.scope {
            LegalHoldScope::Principal(id) => id == object.principal_id,
            LegalHoldScope::Project(id) => id == object.project_id,
            LegalHoldScope::Object(id) => id == object.id,
        }
    }

    pub const fn blocks_at(self, now: StoreTimestamp) -> bool {
        self.placed_at.0 <= now.0
            && match self.released_at {
                Some(released_at) => now.0 < released_at.0,
                None => true,
            }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactReference {
    pub id: ArtifactReferenceId,
    pub artifact_id: ArtifactId,
    pub principal_id: PrincipalId,
    pub project_id: ProjectId,
    pub expires_at: Expiration,
}

impl ArtifactReference {
    pub fn is_shared_with(self, object: &RetainedObject) -> bool {
        self.principal_id != object.principal_id || self.project_id != object.project_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupGeneration {
    pub id: BackupGenerationId,
    pub created_at: StoreTimestamp,
    pub expires_at: Expiration,
    pub contents: BTreeSet<RetentionObjectId>,
}

impl BackupGeneration {
    pub fn new(
        id: BackupGenerationId,
        created_at: StoreTimestamp,
        retention: RetentionPeriod,
        contents: impl IntoIterator<Item = RetentionObjectId>,
    ) -> Self {
        Self {
            id,
            created_at,
            expires_at: retention.expiration_from(created_at),
            contents: contents.into_iter().collect(),
        }
    }

    pub fn from_policy(
        id: BackupGenerationId,
        created_at: StoreTimestamp,
        policy: RetentionPolicy,
        contents: impl IntoIterator<Item = RetentionObjectId>,
    ) -> Self {
        Self::new(id, created_at, policy.backup, contents)
    }

    pub fn can_restore(&self, object: RetentionObjectId, now: StoreTimestamp) -> bool {
        self.contents.contains(&object) && self.expires_at.blocks_at(now)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DeletionBlocker {
    ArchiveIntent,
    Retention(Expiration),
    LegalHold(LegalHoldId),
    ArtifactReference(ArtifactReferenceId),
    BackupGeneration(BackupGenerationId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EarliestPhysicalDeletion {
    At(StoreTimestamp),
    Never,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalDeletionDecision {
    pub physically_deletable: bool,
    pub blockers: Vec<DeletionBlocker>,
    pub earliest: EarliestPhysicalDeletion,
}

pub fn evaluate_physical_deletion_at(
    now: StoreTimestamp,
    object: &RetainedObject,
    intent: RetentionIntent,
    policy: RetentionPolicy,
    holds: &[LegalHold],
    references: &[ArtifactReference],
    backups: &[BackupGeneration],
) -> PhysicalDeletionDecision {
    let retention = policy
        .period_for(object.id.class())
        .expiration_from(object.stored_at);
    let mut blockers = BTreeSet::new();

    if intent == RetentionIntent::Archive {
        blockers.insert(DeletionBlocker::ArchiveIntent);
    }
    if retention.blocks_at(now) {
        blockers.insert(DeletionBlocker::Retention(retention));
    }
    for hold in holds {
        if hold.covers(object) && hold.blocks_at(now) {
            blockers.insert(DeletionBlocker::LegalHold(hold.id));
        }
    }
    if let RetentionObjectId::Artifact(artifact_id) = object.id {
        for reference in references {
            if reference.artifact_id == artifact_id && reference.expires_at.blocks_at(now) {
                blockers.insert(DeletionBlocker::ArtifactReference(reference.id));
            }
        }
    }
    for backup in backups {
        if backup.can_restore(object.id, now) {
            blockers.insert(DeletionBlocker::BackupGeneration(backup.id));
        }
    }

    let earliest =
        earliest_physical_deletion(now, object, intent, retention, holds, references, backups);
    PhysicalDeletionDecision {
        physically_deletable: blockers.is_empty(),
        blockers: blockers.into_iter().collect(),
        earliest,
    }
}

fn earliest_physical_deletion(
    now: StoreTimestamp,
    object: &RetainedObject,
    intent: RetentionIntent,
    retention: Expiration,
    holds: &[LegalHold],
    references: &[ArtifactReference],
    backups: &[BackupGeneration],
) -> EarliestPhysicalDeletion {
    if intent == RetentionIntent::Archive {
        return EarliestPhysicalDeletion::Never;
    }

    let mut candidate = now;
    if !advance_past(&mut candidate, retention) {
        return EarliestPhysicalDeletion::Never;
    }
    if let RetentionObjectId::Artifact(artifact_id) = object.id {
        for reference in references {
            if reference.artifact_id == artifact_id
                && reference.expires_at.blocks_at(now)
                && !advance_past(&mut candidate, reference.expires_at)
            {
                return EarliestPhysicalDeletion::Never;
            }
        }
    }
    for backup in backups {
        if backup.can_restore(object.id, now) && !advance_past(&mut candidate, backup.expires_at) {
            return EarliestPhysicalDeletion::Never;
        }
    }

    loop {
        let before = candidate;
        for hold in holds {
            if !hold.covers(object) || !hold.blocks_at(candidate) {
                continue;
            }
            match hold.released_at {
                Some(released_at) => candidate = candidate.max(released_at),
                None => return EarliestPhysicalDeletion::Never,
            }
        }
        if candidate == before {
            return EarliestPhysicalDeletion::At(candidate);
        }
    }
}

fn advance_past(candidate: &mut StoreTimestamp, expiration: Expiration) -> bool {
    match expiration {
        Expiration::At(expires_at) => {
            *candidate = (*candidate).max(expires_at);
            true
        }
        Expiration::Never => false,
    }
}
