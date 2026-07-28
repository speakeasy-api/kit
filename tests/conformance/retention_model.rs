use std::collections::BTreeSet;

use kit::domain::ids::{
    ArtifactId, EventId, ExperimentId, PrincipalId, ProjectId, TerminalId, ThreadId,
};
use kit::domain::retention::{
    ArtifactReference, ArtifactReferenceId, BackupGeneration, BackupGenerationId, DeletionBlocker,
    EarliestPhysicalDeletion, Expiration, LegalHold, LegalHoldId, LegalHoldScope, RetainedObject,
    RetentionClass, RetentionIntent, RetentionObjectId, RetentionPeriod, RetentionPolicy,
    StoreTimestamp, evaluate_physical_deletion_at,
};

fn at(micros: i64) -> StoreTimestamp {
    StoreTimestamp::from_unix_micros(micros)
}

fn policy(artifact_micros: u64) -> RetentionPolicy {
    RetentionPolicy {
        event: RetentionPeriod::for_micros(1),
        transcript: RetentionPeriod::for_micros(2),
        terminal: RetentionPeriod::for_micros(3),
        artifact: RetentionPeriod::for_micros(artifact_micros),
        experiment: RetentionPeriod::for_micros(5),
        backup: RetentionPeriod::for_micros(6),
    }
}

fn artifact() -> RetainedObject {
    RetainedObject::new(
        RetentionObjectId::Artifact(ArtifactId::generate().unwrap()),
        PrincipalId::generate().unwrap(),
        ProjectId::generate().unwrap(),
        at(0),
    )
}

fn reference(
    id: u128,
    object: &RetainedObject,
    principal_id: PrincipalId,
    project_id: ProjectId,
    expires_at: Expiration,
) -> ArtifactReference {
    let RetentionObjectId::Artifact(artifact_id) = object.id else {
        panic!("fixture object is not an artifact")
    };
    ArtifactReference {
        id: ArtifactReferenceId::new(id),
        artifact_id,
        principal_id,
        project_id,
        expires_at,
    }
}

fn assert_boundary(
    object: &RetainedObject,
    policy: RetentionPolicy,
    holds: &[LegalHold],
    references: &[ArtifactReference],
    backups: &[BackupGeneration],
    boundary: i64,
    expected_before: DeletionBlocker,
) {
    let before = evaluate_physical_deletion_at(
        at(boundary - 1),
        object,
        RetentionIntent::Delete,
        policy,
        holds,
        references,
        backups,
    );
    assert!(!before.physically_deletable);
    assert!(before.blockers.contains(&expected_before));
    assert_eq!(before.earliest, EarliestPhysicalDeletion::At(at(boundary)));

    let at_boundary = evaluate_physical_deletion_at(
        at(boundary),
        object,
        RetentionIntent::Delete,
        policy,
        holds,
        references,
        backups,
    );
    assert!(at_boundary.physically_deletable, "{at_boundary:?}");
    assert!(at_boundary.blockers.is_empty());
    assert_eq!(
        at_boundary.earliest,
        EarliestPhysicalDeletion::At(at(boundary))
    );

    let after = evaluate_physical_deletion_at(
        at(boundary + 1),
        object,
        RetentionIntent::Delete,
        policy,
        holds,
        references,
        backups,
    );
    assert!(after.physically_deletable, "{after:?}");
    assert!(after.blockers.is_empty());
    assert_eq!(
        after.earliest,
        EarliestPhysicalDeletion::At(at(boundary + 1))
    );
}

#[test]
fn all_retention_classes_are_independently_configurable() {
    let policy = policy(4);
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let fixtures = [
        (
            RetentionObjectId::Event(EventId::generate().unwrap()),
            RetentionClass::Event,
            1,
        ),
        (
            RetentionObjectId::Transcript(ThreadId::generate().unwrap()),
            RetentionClass::Transcript,
            2,
        ),
        (
            RetentionObjectId::Terminal(TerminalId::generate().unwrap()),
            RetentionClass::Terminal,
            3,
        ),
        (
            RetentionObjectId::Artifact(ArtifactId::generate().unwrap()),
            RetentionClass::Artifact,
            4,
        ),
        (
            RetentionObjectId::Experiment(ExperimentId::generate().unwrap()),
            RetentionClass::Experiment,
            5,
        ),
        (
            RetentionObjectId::Backup(BackupGenerationId::new(1)),
            RetentionClass::Backup,
            6,
        ),
    ];

    assert_eq!(RetentionClass::ALL.len(), fixtures.len());
    for (id, class, boundary) in fixtures {
        assert_eq!(id.class(), class);
        assert_eq!(
            policy.period_for(class).expiration_from(at(0)),
            Expiration::At(at(boundary))
        );
        let object = RetainedObject::new(id, principal, project, at(0));
        assert_boundary(
            &object,
            policy,
            &[],
            &[],
            &[],
            boundary,
            DeletionBlocker::Retention(Expiration::At(at(boundary))),
        );
    }
}

#[test]
fn archive_is_reversible_visibility_not_deletion_intent() {
    let object = artifact();
    let decision = evaluate_physical_deletion_at(
        at(100),
        &object,
        RetentionIntent::Archive,
        policy(0),
        &[],
        &[],
        &[],
    );
    assert!(!decision.physically_deletable);
    assert_eq!(decision.blockers, vec![DeletionBlocker::ArchiveIntent]);
    assert_eq!(decision.earliest, EarliestPhysicalDeletion::Never);

    let delete = evaluate_physical_deletion_at(
        at(100),
        &object,
        RetentionIntent::Delete,
        policy(0),
        &[],
        &[],
        &[],
    );
    assert!(delete.physically_deletable);
    assert_eq!(delete.earliest, EarliestPhysicalDeletion::At(at(100)));
}

#[test]
fn shared_and_unshared_artifact_reachability_are_global() {
    let object = artifact();
    let unrelated = RetainedObject::new(
        RetentionObjectId::Artifact(ArtifactId::generate().unwrap()),
        object.principal_id,
        object.project_id,
        at(0),
    );
    let unrelated_reference = reference(
        1,
        &unrelated,
        object.principal_id,
        object.project_id,
        Expiration::Never,
    );
    let unshared = evaluate_physical_deletion_at(
        at(10),
        &object,
        RetentionIntent::Delete,
        policy(0),
        &[],
        &[unrelated_reference],
        &[],
    );
    assert!(unshared.physically_deletable);
    assert_eq!(unshared.earliest, EarliestPhysicalDeletion::At(at(10)));

    let shared = reference(
        2,
        &object,
        PrincipalId::generate().unwrap(),
        ProjectId::generate().unwrap(),
        Expiration::At(at(20)),
    );
    assert!(shared.is_shared_with(&object));
    assert_boundary(
        &object,
        policy(0),
        &[],
        &[shared],
        &[],
        20,
        DeletionBlocker::ArtifactReference(shared.id),
    );

    let live = ArtifactReference {
        expires_at: Expiration::Never,
        ..shared
    };
    let blocked = evaluate_physical_deletion_at(
        at(100),
        &object,
        RetentionIntent::Delete,
        policy(0),
        &[],
        &[live],
        &[],
    );
    assert!(!blocked.physically_deletable);
    assert_eq!(blocked.earliest, EarliestPhysicalDeletion::Never);
}

#[test]
fn hold_add_remove_and_all_scopes_obey_exact_boundaries() {
    let object = artifact();
    let scopes = [
        LegalHoldScope::Principal(object.principal_id),
        LegalHoldScope::Project(object.project_id),
        LegalHoldScope::Object(object.id),
    ];

    for (index, scope) in scopes.into_iter().enumerate() {
        let id = LegalHoldId::new(index as u128 + 1);
        let hold = LegalHold::released(id, scope, at(5), at(20));
        assert_boundary(
            &object,
            policy(0),
            &[hold],
            &[],
            &[],
            20,
            DeletionBlocker::LegalHold(id),
        );
    }

    let active = LegalHold::active(
        LegalHoldId::new(10),
        LegalHoldScope::Object(object.id),
        at(5),
    );
    let held = evaluate_physical_deletion_at(
        at(10),
        &object,
        RetentionIntent::Delete,
        policy(0),
        &[active],
        &[],
        &[],
    );
    assert!(!held.physically_deletable);
    assert_eq!(held.earliest, EarliestPhysicalDeletion::Never);

    let before_add = evaluate_physical_deletion_at(
        at(4),
        &object,
        RetentionIntent::Delete,
        policy(0),
        &[active],
        &[],
        &[],
    );
    assert!(before_add.physically_deletable);
    assert_eq!(before_add.earliest, EarliestPhysicalDeletion::At(at(4)));
}

#[test]
fn shorter_policy_changes_never_override_a_hold() {
    let object = artifact();
    let hold_id = LegalHoldId::new(1);
    let active = LegalHold::active(hold_id, LegalHoldScope::Project(object.project_id), at(10));
    let shortened = policy(1);
    let decision = evaluate_physical_deletion_at(
        at(50),
        &object,
        RetentionIntent::Delete,
        shortened,
        &[active],
        &[],
        &[],
    );
    assert!(!decision.physically_deletable);
    assert_eq!(decision.blockers, vec![DeletionBlocker::LegalHold(hold_id)]);
    assert_eq!(decision.earliest, EarliestPhysicalDeletion::Never);

    let released = LegalHold::released(
        hold_id,
        LegalHoldScope::Project(object.project_id),
        at(10),
        at(70),
    );
    assert_boundary(
        &object,
        shortened,
        &[released],
        &[],
        &[],
        70,
        DeletionBlocker::LegalHold(hold_id),
    );
}

#[test]
fn backup_generation_blocks_before_but_not_at_or_after_expiry() {
    let object = artifact();
    let backup_id = BackupGenerationId::new(1);
    let backup = BackupGeneration::new(
        backup_id,
        at(10),
        RetentionPeriod::for_micros(10),
        [object.id],
    );
    assert_eq!(backup.expires_at, Expiration::At(at(20)));
    assert!(backup.can_restore(object.id, at(19)));
    assert!(!backup.can_restore(object.id, at(20)));
    assert!(!backup.can_restore(object.id, at(21)));
    assert_boundary(
        &object,
        policy(0),
        &[],
        &[],
        &[backup],
        20,
        DeletionBlocker::BackupGeneration(backup_id),
    );
}

#[test]
fn every_live_restore_path_blocks_and_latest_boundary_wins() {
    let object = artifact();
    let hold_id = LegalHoldId::new(1);
    let reference_id = ArtifactReferenceId::new(2);
    let backup_id = BackupGenerationId::new(3);
    let hold = LegalHold::released(hold_id, LegalHoldScope::Object(object.id), at(0), at(20));
    let live_reference = reference(
        reference_id.get(),
        &object,
        object.principal_id,
        ProjectId::generate().unwrap(),
        Expiration::At(at(30)),
    );
    let backup = BackupGeneration::new(
        backup_id,
        at(0),
        RetentionPeriod::for_micros(40),
        [object.id],
    );
    let backups = [backup];
    let decision = evaluate_physical_deletion_at(
        at(10),
        &object,
        RetentionIntent::Delete,
        policy(15),
        &[hold],
        &[live_reference],
        &backups,
    );
    assert_eq!(
        decision.blockers.into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            DeletionBlocker::Retention(Expiration::At(at(15))),
            DeletionBlocker::LegalHold(hold_id),
            DeletionBlocker::ArtifactReference(reference_id),
            DeletionBlocker::BackupGeneration(backup_id),
        ])
    );
    assert_eq!(decision.earliest, EarliestPhysicalDeletion::At(at(40)));

    let at_boundary = evaluate_physical_deletion_at(
        at(40),
        &object,
        RetentionIntent::Delete,
        policy(15),
        &[hold],
        &[live_reference],
        &backups,
    );
    assert!(at_boundary.physically_deletable);
    assert_eq!(at_boundary.earliest, EarliestPhysicalDeletion::At(at(40)));
}

#[test]
fn perpetual_retention_or_backup_reports_never() {
    let object = artifact();
    let forever_policy = RetentionPolicy {
        artifact: RetentionPeriod::Forever,
        ..policy(0)
    };
    let retained = evaluate_physical_deletion_at(
        at(100),
        &object,
        RetentionIntent::Delete,
        forever_policy,
        &[],
        &[],
        &[],
    );
    assert_eq!(retained.earliest, EarliestPhysicalDeletion::Never);

    let backup = BackupGeneration::new(
        BackupGenerationId::new(1),
        at(0),
        RetentionPeriod::Forever,
        [object.id],
    );
    let backed_up = evaluate_physical_deletion_at(
        at(100),
        &object,
        RetentionIntent::Delete,
        policy(0),
        &[],
        &[],
        &[backup],
    );
    assert_eq!(backed_up.earliest, EarliestPhysicalDeletion::Never);
}

#[test]
fn policy_is_deterministic_and_contains_no_wall_clock_reads() {
    let source = include_str!("../../src/domain/retention/mod.rs");
    assert!(!source.contains("SystemTime"));
    assert!(!source.contains("Instant::now"));
    assert!(!source.contains("Utc::now"));

    let object = artifact();
    let first = evaluate_physical_deletion_at(
        at(5),
        &object,
        RetentionIntent::Delete,
        policy(10),
        &[],
        &[],
        &[],
    );
    let second = evaluate_physical_deletion_at(
        at(5),
        &object,
        RetentionIntent::Delete,
        policy(10),
        &[],
        &[],
        &[],
    );
    assert_eq!(first, second);
    assert_eq!(first.earliest, EarliestPhysicalDeletion::At(at(10)));
}
