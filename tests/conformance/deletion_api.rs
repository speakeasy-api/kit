use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use axum::{
    Extension,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use kit::{
    api::{
        auth::{
            contract::{Authenticator, GrantSnapshot},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        http::{
            errors::PROBLEM_MEDIA_TYPE,
            retention::{self, AuthoritativeStoreTime},
        },
    },
    domain::{
        config::Grant,
        deletion::{
            DeletionActor, DeletionError, DeletionJobState, DeletionService, PublicDeletionBlocker,
        },
        ids::{ArtifactId, EventId, ExperimentId, PrincipalId, ProjectId, TerminalId, ThreadId},
        retention::{
            ArtifactReference, ArtifactReferenceId, BackupGeneration, BackupGenerationId,
            EarliestPhysicalDeletion, Expiration, LegalHold, LegalHoldId, LegalHoldScope,
            RetainedObject, RetentionObjectId, RetentionPeriod, RetentionPolicy, StoreTimestamp,
        },
    },
};
use serde_json::Value;
use tower::ServiceExt;

fn at(value: i64) -> StoreTimestamp {
    StoreTimestamp::from_unix_micros(value)
}

fn policy(class: RetentionPeriod) -> RetentionPolicy {
    RetentionPolicy {
        event: class,
        transcript: class,
        terminal: class,
        artifact: class,
        experiment: class,
        backup: class,
    }
}

fn fixture(id: RetentionObjectId) -> (DeletionService, DeletionActor, RetainedObject) {
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let object = RetainedObject::new(id, principal_id, project_id, at(0));
    let actor = DeletionActor::new(principal_id, project_id);
    let mut service = DeletionService::new();
    service.register_object(object, policy(RetentionPeriod::for_micros(10)));
    (service, actor, object)
}

#[test]
fn every_retention_class_reports_both_sides_of_its_earliest_boundary() {
    let fixtures = [
        RetentionObjectId::Event(EventId::generate().unwrap()),
        RetentionObjectId::Transcript(ThreadId::generate().unwrap()),
        RetentionObjectId::Terminal(TerminalId::generate().unwrap()),
        RetentionObjectId::Artifact(ArtifactId::generate().unwrap()),
        RetentionObjectId::Experiment(ExperimentId::generate().unwrap()),
        RetentionObjectId::Backup(BackupGenerationId::new(91)),
    ];

    for object_id in fixtures {
        let (service, actor, _) = fixture(object_id);
        let before = service
            .retention_for_object(actor, object_id, at(9))
            .unwrap();
        assert_eq!(
            before.earliest_physical_deletion,
            EarliestPhysicalDeletion::At(at(10))
        );
        let at_boundary = service
            .retention_for_object(actor, object_id, at(10))
            .unwrap();
        assert_eq!(
            at_boundary.earliest_physical_deletion,
            EarliestPhysicalDeletion::At(at(10))
        );
        let after = service
            .retention_for_object(actor, object_id, at(11))
            .unwrap();
        assert_eq!(
            after.earliest_physical_deletion,
            EarliestPhysicalDeletion::At(at(11))
        );
    }
}

#[test]
fn archive_and_unarchive_are_reversible_visibility_only() {
    let thread = ThreadId::generate().unwrap();
    let (mut service, actor, object) = fixture(RetentionObjectId::Transcript(thread));

    let archived = service
        .archive(actor, object.id, true, "archive-key")
        .unwrap();
    assert!(archived.archived);
    assert!(
        service
            .archive(actor, object.id, true, "archive-key")
            .unwrap()
            .archived
    );
    assert_eq!(
        service.archive(actor, object.id, false, "archive-key"),
        Err(DeletionError::IdempotencyConflict)
    );
    assert!(
        !service
            .archive(actor, object.id, false, "unarchive-key")
            .unwrap()
            .archived
    );

    let job = service
        .request_deletion(actor, object.id, "delete-key", at(10))
        .unwrap();
    let mut physical_calls = 0;
    let completed = service
        .execute_job(actor, job.id, job.fence, at(10), |_| {
            physical_calls += 1;
            Ok::<_, &str>(())
        })
        .unwrap();
    assert_eq!(physical_calls, 1);
    assert_eq!(completed.state, DeletionJobState::Completed);
    assert_eq!(service.reevaluate_job(job.id, at(100)).unwrap(), completed);
}

#[test]
fn legal_hold_is_an_audited_typed_refusal_and_release_requires_a_fresh_fence() {
    let (mut service, actor, object) =
        fixture(RetentionObjectId::Transcript(ThreadId::generate().unwrap()));
    let hold_id = LegalHoldId::new(7);
    service.put_legal_hold(LegalHold::active(
        hold_id,
        LegalHoldScope::Object(object.id),
        at(0),
    ));

    let error = service
        .request_deletion(actor, object.id, "held-delete", at(20))
        .unwrap_err();
    let DeletionError::LegalHold { job_id, .. } = error else {
        panic!("expected typed legal-hold refusal")
    };
    let blocked = service.job(actor, job_id).unwrap();
    assert_eq!(blocked.state, DeletionJobState::Blocked);
    assert_eq!(blocked.blockers, vec![PublicDeletionBlocker::LegalHold]);
    assert_eq!(
        blocked
            .audit
            .iter()
            .map(|entry| entry.state)
            .collect::<Vec<_>>(),
        [
            DeletionJobState::Requested,
            DeletionJobState::Evaluating,
            DeletionJobState::Blocked,
        ]
    );

    service.remove_legal_hold(hold_id);
    assert!(matches!(
        service.execute_job(actor, job_id, blocked.fence, at(20), |_| Ok::<_, &str>(())),
        Err(DeletionError::StaleFence { .. })
    ));
    let ready = service.job(actor, job_id).unwrap();
    assert_eq!(ready.state, DeletionJobState::WaitingForPolicy);
    let completed = service
        .execute_job(actor, job_id, ready.fence, at(20), |_| Ok::<_, &str>(()))
        .unwrap();
    assert_eq!(completed.state, DeletionJobState::Completed);
}

#[test]
fn shared_artifact_waits_for_every_reference_without_disclosing_owners() {
    let artifact_id = ArtifactId::generate().unwrap();
    let (mut service, actor, object) = fixture(RetentionObjectId::Artifact(artifact_id));
    let first = ArtifactReference {
        id: ArtifactReferenceId::new(1),
        artifact_id,
        principal_id: actor.principal_id,
        project_id: actor.project_id,
        expires_at: Expiration::At(at(30)),
    };
    let second = ArtifactReference {
        id: ArtifactReferenceId::new(2),
        artifact_id,
        principal_id: PrincipalId::generate().unwrap(),
        project_id: ProjectId::generate().unwrap(),
        expires_at: Expiration::At(at(40)),
    };
    service.put_artifact_reference(first);
    service.put_artifact_reference(second);

    let job = service
        .request_deletion(actor, object.id, "shared", at(20))
        .unwrap();
    assert_eq!(job.blockers, vec![PublicDeletionBlocker::ActiveReference]);
    assert_eq!(
        job.effective_retention.earliest_physical_deletion,
        EarliestPhysicalDeletion::At(at(40))
    );
    service.remove_artifact_reference(first.id);
    let still_shared = service.reevaluate_job(job.id, at(31)).unwrap();
    assert_eq!(
        still_shared.blockers,
        vec![PublicDeletionBlocker::ActiveReference]
    );
    service.remove_artifact_reference(second.id);
    let unreferenced = service.reevaluate_job(job.id, at(31)).unwrap();
    assert!(unreferenced.blockers.is_empty());

    let outsider = DeletionActor::new(second.principal_id, second.project_id);
    assert_eq!(service.job(outsider, job.id), Err(DeletionError::NotFound));
    assert_eq!(
        service.retention_for_object(outsider, object.id, at(31)),
        Err(DeletionError::NotFound)
    );
}

#[test]
fn backup_inventory_blocks_only_before_advertised_expiry() {
    let (mut service, actor, object) =
        fixture(RetentionObjectId::Artifact(ArtifactId::generate().unwrap()));
    let backup = BackupGeneration::new(
        BackupGenerationId::new(3),
        at(10),
        RetentionPeriod::for_micros(20),
        [object.id],
    );
    service.put_backup_generation(backup);

    let before = service
        .request_deletion(actor, object.id, "backup", at(29))
        .unwrap();
    assert_eq!(
        before.blockers,
        vec![PublicDeletionBlocker::BackupGeneration]
    );
    assert_eq!(
        before.effective_retention.earliest_physical_deletion,
        EarliestPhysicalDeletion::At(at(30))
    );
    let boundary = service.reevaluate_job(before.id, at(30)).unwrap();
    assert!(boundary.blockers.is_empty());
    let after = service.reevaluate_job(before.id, at(31)).unwrap();
    assert!(after.blockers.is_empty());
}

#[test]
fn deletion_jobs_are_idempotent_asynchronous_and_record_failure() {
    let (mut service, actor, object) =
        fixture(RetentionObjectId::Transcript(ThreadId::generate().unwrap()));
    let first = service
        .request_deletion(actor, object.id, "same-key", at(10))
        .unwrap();
    let replay = service
        .request_deletion(actor, object.id, "same-key", at(99))
        .unwrap();
    assert_eq!(first.id, replay.id);
    assert_eq!(first.audit, replay.audit);
    assert_eq!(first.state, DeletionJobState::WaitingForPolicy);

    let failed = service
        .execute_job(actor, first.id, first.fence, at(10), |_| {
            Err::<(), _>("storage refused deletion")
        })
        .unwrap();
    assert_eq!(failed.state, DeletionJobState::Failed);
    assert_eq!(failed.failure.as_deref(), Some("storage refused deletion"));
    assert!(
        failed
            .audit
            .iter()
            .any(|entry| entry.state == DeletionJobState::PhysicallyDeleting)
    );
    assert!(
        failed
            .audit
            .iter()
            .any(|entry| entry.state == DeletionJobState::Failed)
    );
}

fn authenticated(
    principal_id: PrincipalId,
    project_id: ProjectId,
) -> kit::api::auth::contract::AuthenticatedPrincipal {
    LocalPeerAuthenticator::new(std::collections::BTreeMap::from([(
        1000,
        GrantSnapshot::new(
            principal_id,
            project_id,
            BTreeSet::from([Grant::WorkspaceRead, Grant::WorkspaceWrite]),
        ),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000))
    .unwrap()
}

fn http_request(method: Method, uri: &str, key: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", key)
        .body(Body::from(body.to_owned()))
        .unwrap()
}

#[tokio::test]
async fn http_returns_jobs_typed_hold_refusal_and_no_cross_principal_details() {
    let thread_id = ThreadId::generate().unwrap();
    let (mut service, actor, object) = fixture(RetentionObjectId::Transcript(thread_id));
    service.put_legal_hold(LegalHold::active(
        LegalHoldId::new(123456),
        LegalHoldScope::Object(object.id),
        at(0),
    ));
    let service = Arc::new(Mutex::new(service));
    let app = retention::routes(service.clone())
        .layer(Extension(AuthoritativeStoreTime(at(20))))
        .layer(Extension(authenticated(
            actor.principal_id,
            actor.project_id,
        )));

    let response = app
        .clone()
        .oneshot(http_request(
            Method::POST,
            &format!("/v1/threads/{thread_id}/deletion"),
            "http-delete",
            "{}",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::LOCKED);
    assert_eq!(response.headers()[header::CONTENT_TYPE], PROBLEM_MEDIA_TYPE);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert_eq!(body["code"], "legal_hold");
    let encoded = serde_json::to_string(&body).unwrap();
    assert!(!encoded.contains("123456"));
    assert!(!encoded.contains(&actor.principal_id.to_string()));

    let replay = app
        .oneshot(http_request(
            Method::POST,
            &format!("/v1/threads/{thread_id}/deletion"),
            "http-delete",
            "{}",
        ))
        .await
        .unwrap();
    let replay: Value =
        serde_json::from_slice(&to_bytes(replay.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert_eq!(body["deletion_job_id"], replay["deletion_job_id"]);

    let outsider = retention::routes(service)
        .layer(Extension(AuthoritativeStoreTime(at(20))))
        .layer(Extension(authenticated(
            PrincipalId::generate().unwrap(),
            ProjectId::generate().unwrap(),
        )));
    let missing = outsider
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/v1/deletion-jobs/{}",
                    body["deletion_job_id"].as_str().unwrap()
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}
