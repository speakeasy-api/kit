use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::{
    domain::{
        commands::ExpectedVersion,
        crypto::sha256,
        events::{CommitPosition, EntityId, EventType, SchemaVersion, TraceId, UtcDateTime},
        ids::{CommandId, EventId, McpCallbackId, PrincipalId, ProjectId, RunId},
        mcp_callback::{
            McpCallbackAction, McpCallbackArtifactRef, McpCallbackCommand, McpCallbackError,
            McpCallbackEvent, McpCallbackEventEnvelope, McpCallbackProjection, McpCallbackState,
        },
    },
    store::artifacts::{
        ArtifactDigest, ArtifactEnvelopeBinding, ArtifactPublication, ArtifactReference,
        ArtifactRetention, ArtifactStore,
    },
    store::sqlite::{
        append::{NewEvent, append_canonical_event},
        idempotency::IdempotencyKey,
    },
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const PUBLICATION_LOCK_STRIPES: usize = 64;

static PUBLICATION_LOCKS: OnceLock<[Mutex<()>; PUBLICATION_LOCK_STRIPES]> = OnceLock::new();

fn publication_lock(reference: ArtifactReference) -> &'static Mutex<()> {
    let mut hasher = DefaultHasher::new();
    reference.hash(&mut hasher);
    &PUBLICATION_LOCKS.get_or_init(|| std::array::from_fn(|_| Mutex::new(())))
        [hasher.finish() as usize % PUBLICATION_LOCK_STRIPES]
}

pub(crate) fn lock_artifact_publication(
    reference: ArtifactReference,
) -> std::sync::MutexGuard<'static, ()> {
    publication_lock(reference)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone)]
pub struct McpCallbackStore {
    database: PathBuf,
}

impl McpCallbackStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, McpCallbackError> {
        let database = path.as_ref().to_owned();
        let mut connection = connection(&database)?;
        migrate(&mut connection)?;
        Ok(Self { database })
    }

    pub fn request(
        &self,
        callback: McpCallbackProjection,
    ) -> Result<McpCallbackProjection, McpCallbackError> {
        callback.validate()?;
        if callback.state != McpCallbackState::Requested || callback.version != 1 {
            return Err(McpCallbackError::Invalid(
                "new callback must be requested at version 1",
            ));
        }
        let mut connection = connection(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        if let Some(existing) = load(&transaction, callback.id)? {
            if existing == callback || same_request(&existing, &callback) {
                transaction.commit().map_err(store)?;
                return Ok(existing);
            }
            return Err(McpCallbackError::IdempotencyConflict);
        }
        if now_unix_micros(&transaction)? >= callback.expires_at.unix_micros() {
            return Err(McpCallbackError::Expired);
        }
        let requested = McpCallbackEvent {
            callback_id: callback.id,
            expected_version: 0,
            state: McpCallbackState::Requested,
            resolver_actor: None,
            action: None,
            artifact_refs: Vec::new(),
            terminal_error: None,
        };
        insert_event(
            &transaction,
            &McpCallbackCommand::Request {
                callback: Box::new(callback.clone()),
            },
            &requested,
            &callback,
            1,
        )?;
        let mut awaiting = callback;
        let event = McpCallbackEvent {
            callback_id: awaiting.id,
            expected_version: 1,
            state: McpCallbackState::AwaitingResolution,
            resolver_actor: None,
            action: None,
            artifact_refs: Vec::new(),
            terminal_error: None,
        };
        awaiting.apply(&event)?;
        insert_event(
            &transaction,
            &McpCallbackCommand::AwaitResolution {
                callback_id: awaiting.id,
                expected_version: 1,
            },
            &event,
            &awaiting,
            2,
        )?;
        transaction.execute(
            "INSERT INTO mcp_callback_workspace_revisions (workspace_id, revision, lease_version)
             VALUES (?1,?2,1)
             ON CONFLICT(workspace_id) DO UPDATE SET
               revision=excluded.revision,
               lease_version=mcp_callback_workspace_revisions.lease_version +
                 (mcp_callback_workspace_revisions.revision <> excluded.revision)",
            params![
                awaiting.workspace_id.to_string(),
                awaiting.workspace_revision,
            ],
        ).map_err(store)?;
        save(&transaction, &awaiting)?;
        transaction.commit().map_err(store)?;
        Ok(awaiting)
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub fn resolve(
        &self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        idempotency_key: &IdempotencyKey,
        callback_id: McpCallbackId,
        expected_version: u64,
        challenge_generation: u64,
        schema_digest: &str,
        action: McpCallbackAction,
        artifact_refs: Vec<McpCallbackArtifactRef>,
        request_digest: [u8; 32],
        authority: &McpCallbackProjection,
    ) -> Result<(McpCallbackProjection, bool, Vec<CommitPosition>), McpCallbackError> {
        self.resolve_with_recheck(
            principal_id,
            project_id,
            idempotency_key,
            callback_id,
            expected_version,
            challenge_generation,
            schema_digest,
            action,
            artifact_refs,
            request_digest,
            authority,
            &|_| true,
            &authority.workspace_revision,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resolve_with_recheck(
        &self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        idempotency_key: &IdempotencyKey,
        callback_id: McpCallbackId,
        expected_version: u64,
        challenge_generation: u64,
        schema_digest: &str,
        action: McpCallbackAction,
        artifact_refs: Vec<McpCallbackArtifactRef>,
        request_digest: [u8; 32],
        authority: &McpCallbackProjection,
        authority_recheck: &dyn Fn(&McpCallbackProjection) -> bool,
        authoritative_workspace_revision: &str,
    ) -> Result<(McpCallbackProjection, bool, Vec<CommitPosition>), McpCallbackError> {
        if let Some((callback, positions)) = self.replay_resolution(
            principal_id,
            project_id,
            callback_id,
            idempotency_key,
            request_digest,
            expected_version,
            challenge_generation,
            schema_digest,
        )? {
            return Ok((callback, true, positions));
        }
        let publication = resolution_publication(
            &self.database,
            principal_id,
            project_id,
            authority.run_id,
            callback_id,
            action,
            &artifact_refs,
        )?;
        let publication_lock = publication
            .as_ref()
            .map(|publication| publication_lock(publication.reference()));
        let publication_guard = publication_lock
            .as_ref()
            .map(|lock| lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        let mut cleanup_publication = false;
        let result = (|| {
            if let Some(publication) = &publication {
                let mut connection = connection(&self.database)?;
                let transaction = connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(store)?;
                validate_resolution(
                    &transaction,
                    principal_id,
                    project_id,
                    callback_id,
                    expected_version,
                    challenge_generation,
                    schema_digest,
                    idempotency_key,
                    request_digest,
                    authority,
                    authority_recheck,
                    authoritative_workspace_revision,
                )?;
                transaction
                    .execute(
                        "INSERT INTO artifact_publication_journal
                     (artifact_reference, artifact_digest, purpose, subject_id, principal_id,
                      project_id, run_id, state, created_at_unix_micros)
                     VALUES (?1,?2,'mcp_callback_content',?3,?4,?5,?6,
                             'intent_recorded',?7)
                     ON CONFLICT(artifact_reference) DO UPDATE SET
                       artifact_digest=excluded.artifact_digest,
                       purpose=excluded.purpose,
                       subject_id=excluded.subject_id,
                       principal_id=excluded.principal_id,
                       project_id=excluded.project_id,
                       run_id=excluded.run_id,
                       state='intent_recorded'",
                        params![
                            publication.reference().to_string(),
                            publication.digest().to_string(),
                            callback_id.to_string(),
                            principal_id.to_string(),
                            project_id.to_string(),
                            authority.run_id.to_string(),
                            now_unix_micros(&transaction)?,
                        ],
                    )
                    .map_err(store)?;
                cleanup_publication = true;
                transaction.commit().map_err(store)?;
                set_publication_state(&self.database, publication.reference(), "staged_verified")?;
                callback_artifacts(&self.database)?
                    .promote_publication(publication)
                    .map_err(artifact_store)?;
                set_publication_state(&self.database, publication.reference(), "promoted")?;
            }

            let mut connection = connection(&self.database)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(store)?;
            let mut callback = validate_resolution(
                &transaction,
                principal_id,
                project_id,
                callback_id,
                expected_version,
                challenge_generation,
                schema_digest,
                idempotency_key,
                request_digest,
                authority,
                authority_recheck,
                authoritative_workspace_revision,
            )?;
            let resolution = McpCallbackEvent {
                callback_id,
                expected_version,
                state: McpCallbackState::Resolved,
                resolver_actor: Some(principal_id),
                action: Some(action),
                artifact_refs: artifact_refs.clone(),
                terminal_error: None,
            };
            callback.apply(&resolution)?;
            let command = McpCallbackCommand::Resolve {
                callback_id,
                expected_version,
                challenge_generation,
                schema_digest: schema_digest.to_owned(),
                resolver_actor: principal_id,
                action,
                artifact_refs: artifact_refs.clone(),
            };
            let position = insert_event(
                &transaction,
                &command,
                &resolution,
                &callback,
                callback.version,
            )?;
            save(&transaction, &callback)?;
            let commit_positions = vec![position];
            let response = serde_json::to_vec(&ResolutionResponse {
                callback: callback.clone(),
                commit_positions: commit_positions.clone(),
            })
            .map_err(|error| McpCallbackError::Store(error.to_string()))?;
            transaction.execute(
            "INSERT INTO mcp_callback_idempotency (principal_id, callback_id, idempotency_key, request_digest, response) VALUES (?1,?2,?3,?4,?5)",
            params![principal_id.to_string(), callback_id.to_string(), idempotency_key.as_str(), request_digest.as_slice(), response],
        ).map_err(store)?;
            transaction
                .execute(
                    "DELETE FROM mcp_callback_resolution_reservations WHERE callback_id=?1",
                    [callback_id.to_string()],
                )
                .map_err(store)?;
            if let Some(publication) = &publication {
                let changed = transaction
                    .execute(
                        "UPDATE artifact_publication_journal SET state='resolution_committed'
                 WHERE artifact_reference=?1 AND state='promoted'",
                        [publication.reference().to_string()],
                    )
                    .map_err(store)?;
                if changed != 1 {
                    return Err(McpCallbackError::Store(
                        "artifact publication was not promoted".to_owned(),
                    ));
                }
            }
            transaction.commit().map_err(store)?;
            if publication.is_some() {
                cleanup_publication = false;
            }
            Ok((callback, false, commit_positions))
        })();
        if result.is_err()
            && cleanup_publication
            && let Some(publication) = &publication
        {
            let artifacts = callback_artifacts(&self.database)?;
            artifacts
                .erase_owned_reference(
                    publication.reference(),
                    &principal_id.to_string(),
                    &project_id.to_string(),
                )
                .map_err(artifact_store)?;
            delete_publication_journal(&self.database, publication.reference())?;
        }
        drop(publication_guard);
        result
    }

    pub fn reconcile_artifact_publications(&self) -> Result<usize, McpCallbackError> {
        let artifacts = callback_artifacts(&self.database)?;
        let references = {
            let connection = connection(&self.database)?;
            let mut statement = connection
                .prepare(
                    "SELECT artifact_reference
                     FROM artifact_publication_journal ORDER BY artifact_reference",
                )
                .map_err(store)?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(store)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store)?
        };
        let mut reconciled = 0;
        for reference in &references {
            let reference = ArtifactReference::parse(reference).map_err(artifact_store)?;
            let lock = publication_lock(reference);
            let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some((digest, purpose, subject, principal, project, run)) =
                connection(&self.database)?
                    .query_row(
                        "SELECT artifact_digest, purpose, subject_id, principal_id, project_id,
                                run_id
                         FROM artifact_publication_journal WHERE artifact_reference=?1",
                        [reference.to_string()],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, String>(5)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(store)?
            else {
                continue;
            };
            let digest = ArtifactDigest::parse(&digest).map_err(artifact_store)?;
            let binding = ArtifactEnvelopeBinding {
                principal: principal.clone(),
                project: project.clone(),
                run: run.clone(),
                purpose: purpose.clone(),
                invocation_id: (purpose == "mcp_invocation_result").then(|| subject.clone()),
                callback_id: (purpose == "mcp_callback_content").then(|| subject.clone()),
            };
            if purpose == "mcp_callback_content" {
                let callback_id = McpCallbackId::from_str(&subject)
                    .map_err(|error| McpCallbackError::Store(error.to_string()))?;
                let callback = connection(&self.database)?
                    .query_row(
                        "SELECT projection FROM mcp_callback_projection WHERE callback_id=?1",
                        [&subject],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .optional()
                    .map_err(store)?
                    .and_then(|bytes| serde_json::from_slice::<McpCallbackProjection>(&bytes).ok());
                let committed = callback.as_ref().is_some_and(|callback| {
                    callback
                        .artifact_refs
                        .iter()
                        .any(|found| found.as_str() == reference.to_string())
                        && matches!(
                            callback.state,
                            McpCallbackState::Resolved
                                | McpCallbackState::ResponsePrepared
                                | McpCallbackState::Delivered
                                | McpCallbackState::DeliveryUnknown
                                | McpCallbackState::Interrupted
                        )
                });
                if !committed {
                    artifacts
                        .erase_owned_reference(reference, &principal, &project)
                        .map_err(artifact_store)?;
                    delete_publication_journal(&self.database, reference)?;
                    reconciled += 1;
                    continue;
                }
                if artifacts.open_reference(reference).is_err() {
                    match artifacts.publication(reference, digest) {
                        Ok(publication) => {
                            artifacts
                                .read_staged_publication(&publication, &binding, 8 * 1024 * 1024)
                                .map_err(artifact_store)?;
                            artifacts
                                .promote_publication(&publication)
                                .map_err(artifact_store)?;
                        }
                        Err(_) => {
                            let _ = self.interrupt(callback_id, "artifact_missing".to_owned());
                            delete_publication_journal(&self.database, reference)?;
                            reconciled += 1;
                            continue;
                        }
                    }
                }
                verify_committed_resolution_artifact(&self.database, &self.get(callback_id)?)?;
                if callback.is_some_and(|callback| callback.state.is_terminal()) {
                    delete_publication_journal(&self.database, reference)?;
                    reconciled += 1;
                }
                continue;
            }
            match artifacts.publication(reference, digest) {
                Ok(publication) => {
                    artifacts
                        .read_staged_publication(&publication, &binding, 8 * 1024 * 1024)
                        .map_err(artifact_store)?;
                    artifacts
                        .promote_publication(&publication)
                        .map_err(artifact_store)?;
                }
                Err(crate::store::artifacts::ArtifactError::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound =>
                {
                    let published = artifacts
                        .open_reference(reference)
                        .map_err(artifact_store)?;
                    if published.digest() != digest {
                        return Err(McpCallbackError::Store(
                            "published callback artifact journal digest mismatch".to_owned(),
                        ));
                    }
                    artifacts
                        .with_reference_reader(reference, |_, reader| {
                            let mut envelope = Vec::new();
                            reader
                                .take(8 * 1024 * 1024 + 16 * 1024)
                                .read_to_end(&mut envelope)?;
                            binding.open(&envelope)?;
                            Ok(())
                        })
                        .map_err(artifact_store)?;
                }
                Err(error) => return Err(artifact_store(error)),
            }
            delete_publication_journal(&self.database, reference)?;
            reconciled += 1;
        }
        for reference in artifacts.staged_publications().map_err(artifact_store)? {
            let lock = publication_lock(reference);
            let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let journaled: bool = connection(&self.database)?
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM artifact_publication_journal
                     WHERE artifact_reference=?1)",
                    [reference.to_string()],
                    |row| row.get(0),
                )
                .map_err(store)?;
            if !journaled {
                artifacts
                    .remove_publication_stage(reference)
                    .map_err(artifact_store)?;
            }
        }
        Ok(reconciled)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn replay_resolution(
        &self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        callback_id: McpCallbackId,
        idempotency_key: &IdempotencyKey,
        request_digest: [u8; 32],
        expected_version: u64,
        challenge_generation: u64,
        schema_digest: &str,
    ) -> Result<Option<(McpCallbackProjection, Vec<CommitPosition>)>, McpCallbackError> {
        let connection = connection(&self.database)?;
        let Some((digest, response)) = connection
            .query_row(
                "SELECT request_digest, response FROM mcp_callback_idempotency WHERE principal_id=?1 AND callback_id=?2 AND idempotency_key=?3",
                params![principal_id.to_string(), callback_id.to_string(), idempotency_key.as_str()],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(store)?
        else {
            return Ok(None);
        };
        if digest != request_digest {
            return Err(McpCallbackError::IdempotencyConflict);
        }
        let response: ResolutionResponse = serde_json::from_slice(&response)
            .map_err(|error| McpCallbackError::Store(error.to_string()))?;
        let current = load(&connection, callback_id)?.ok_or(McpCallbackError::NotFound)?;
        if current.principal_id != principal_id
            || current.project_id != project_id
            || response.callback.principal_id != principal_id
            || response.callback.project_id != project_id
            || response.callback.id != callback_id
            || response.callback.version != expected_version.saturating_add(1)
            || response.callback.challenge_generation != challenge_generation
            || response.callback.schema_digest != schema_digest
            || !same_authority(&current, &response.callback)
        {
            return Err(McpCallbackError::Authority);
        }
        if now_unix_micros(&connection)? >= response.callback.artifact_expires_at.unix_micros() {
            return Err(McpCallbackError::Expired);
        }
        verify_committed_resolution_artifact(&self.database, &response.callback)?;
        Ok(Some((response.callback, response.commit_positions)))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reserve_resolution(
        &self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        callback_id: McpCallbackId,
        idempotency_key: &IdempotencyKey,
        request_digest: [u8; 32],
        expected_version: u64,
        challenge_generation: u64,
        schema_digest: &str,
    ) -> Result<Option<(McpCallbackProjection, Vec<CommitPosition>)>, McpCallbackError> {
        if let Some(replay) = self.replay_resolution(
            principal_id,
            project_id,
            callback_id,
            idempotency_key,
            request_digest,
            expected_version,
            challenge_generation,
            schema_digest,
        )? {
            return Ok(Some(replay));
        }
        let mut connection = connection(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let callback = load(&transaction, callback_id)?.ok_or(McpCallbackError::NotFound)?;
        if callback.principal_id != principal_id || callback.project_id != project_id {
            return Err(McpCallbackError::NotFound);
        }
        if callback.version != expected_version {
            return Err(McpCallbackError::VersionConflict {
                expected: expected_version,
                actual: callback.version,
            });
        }
        if callback.state != McpCallbackState::AwaitingResolution {
            return Err(if callback.state.is_terminal() {
                McpCallbackError::Terminal(callback.state)
            } else {
                McpCallbackError::IllegalTransition {
                    from: callback.state,
                    to: McpCallbackState::Resolved,
                }
            });
        }
        if callback.challenge_generation != challenge_generation
            || callback.schema_digest != schema_digest
        {
            return Err(McpCallbackError::Authority);
        }
        if now_unix_micros(&transaction)? >= callback.expires_at.unix_micros() {
            return Err(McpCallbackError::Expired);
        }
        guard_claim(&transaction, &callback)?;
        let inserted = transaction
            .execute(
                "INSERT INTO mcp_callback_resolution_reservations
                 (callback_id, principal_id, idempotency_key, request_digest, expected_version)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(callback_id) DO NOTHING",
                params![
                    callback_id.to_string(),
                    principal_id.to_string(),
                    idempotency_key.as_str(),
                    request_digest.as_slice(),
                    expected_version
                ],
            )
            .map_err(store)?;
        if inserted == 0 {
            let exact: bool = transaction
                .query_row(
                    "SELECT principal_id=?2 AND idempotency_key=?3 AND request_digest=?4 AND expected_version=?5
                     FROM mcp_callback_resolution_reservations WHERE callback_id=?1",
                    params![callback_id.to_string(), principal_id.to_string(), idempotency_key.as_str(), request_digest.as_slice(), expected_version],
                    |row| row.get(0),
                )
                .map_err(store)?;
            if !exact {
                return Err(McpCallbackError::IdempotencyConflict);
            }
        }
        transaction.commit().map_err(store)?;
        Ok(None)
    }

    pub fn prepare_response(
        &self,
        callback_id: McpCallbackId,
    ) -> Result<McpCallbackProjection, McpCallbackError> {
        self.transition_prepared(callback_id, McpCallbackState::ResponsePrepared)
    }

    pub fn prepare_automatic_delivery(
        &self,
        callback: McpCallbackProjection,
    ) -> Result<McpCallbackProjection, McpCallbackError> {
        if !matches!(
            (callback.kind, callback.mode),
            (
                crate::domain::mcp_callback::McpCallbackKind::Sampling,
                crate::domain::mcp_callback::McpCallbackMode::SamplingResponse
            ) | (
                crate::domain::mcp_callback::McpCallbackKind::Roots,
                crate::domain::mcp_callback::McpCallbackMode::RootsResponse
            )
        ) {
            return Err(McpCallbackError::Invalid(
                "invalid automatic callback delivery",
            ));
        }
        let current = self.request(callback)?;
        match current.state {
            McpCallbackState::AwaitingResolution => {
                let mut connection = connection(&self.database)?;
                let transaction = connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(store)?;
                let mut current =
                    load(&transaction, current.id)?.ok_or(McpCallbackError::NotFound)?;
                let event = McpCallbackEvent {
                    callback_id: current.id,
                    expected_version: current.version,
                    state: McpCallbackState::Resolved,
                    resolver_actor: Some(current.principal_id),
                    action: Some(McpCallbackAction::Accept),
                    artifact_refs: Vec::new(),
                    terminal_error: None,
                };
                current.apply(&event)?;
                insert_event(
                    &transaction,
                    &McpCallbackCommand::Resolve {
                        callback_id: current.id,
                        expected_version: event.expected_version,
                        challenge_generation: current.challenge_generation,
                        schema_digest: current.schema_digest.clone(),
                        resolver_actor: current.principal_id,
                        action: McpCallbackAction::Accept,
                        artifact_refs: Vec::new(),
                    },
                    &event,
                    &current,
                    current.version,
                )?;
                save(&transaction, &current)?;
                transaction.commit().map_err(store)?;
                self.prepare_response(current.id)
            }
            McpCallbackState::Resolved => self.prepare_response(current.id),
            McpCallbackState::ResponsePrepared
            | McpCallbackState::Delivered
            | McpCallbackState::DeliveryUnknown => Ok(current),
            _ => Err(McpCallbackError::IllegalTransition {
                from: current.state,
                to: McpCallbackState::ResponsePrepared,
            }),
        }
    }

    pub fn deliver(
        &self,
        callback_id: McpCallbackId,
    ) -> Result<McpCallbackProjection, McpCallbackError> {
        self.transition_prepared(callback_id, McpCallbackState::Delivered)
    }

    fn transition_prepared(
        &self,
        callback_id: McpCallbackId,
        state: McpCallbackState,
    ) -> Result<McpCallbackProjection, McpCallbackError> {
        let mut connection = connection(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let mut callback = load(&transaction, callback_id)?.ok_or(McpCallbackError::NotFound)?;
        let event = McpCallbackEvent {
            callback_id,
            expected_version: callback.version,
            state,
            resolver_actor: callback.resolver_actor,
            action: callback.action,
            artifact_refs: callback.artifact_refs.clone(),
            terminal_error: None,
        };
        let command = if state == McpCallbackState::ResponsePrepared {
            McpCallbackCommand::PrepareResponse {
                callback_id,
                expected_version: callback.version,
            }
        } else {
            McpCallbackCommand::Deliver {
                callback_id,
                expected_version: callback.version,
            }
        };
        callback.apply(&event)?;
        insert_event(&transaction, &command, &event, &callback, callback.version)?;
        save(&transaction, &callback)?;
        transaction
            .execute(
                "DELETE FROM mcp_callback_resolution_reservations WHERE callback_id=?1",
                [callback_id.to_string()],
            )
            .map_err(store)?;
        transaction.commit().map_err(store)?;
        Ok(callback)
    }

    pub fn settle(
        &self,
        callback_id: McpCallbackId,
        state: McpCallbackState,
        error: Option<String>,
    ) -> Result<McpCallbackProjection, McpCallbackError> {
        self.settle_inner(callback_id, None, state, error)
    }

    pub fn settle_awaiting(
        &self,
        callback_id: McpCallbackId,
        expected_version: u64,
        state: McpCallbackState,
        error: Option<String>,
    ) -> Result<McpCallbackProjection, McpCallbackError> {
        self.settle_inner(callback_id, Some(expected_version), state, error)
    }

    fn settle_inner(
        &self,
        callback_id: McpCallbackId,
        expected_version: Option<u64>,
        state: McpCallbackState,
        error: Option<String>,
    ) -> Result<McpCallbackProjection, McpCallbackError> {
        if !matches!(
            state,
            McpCallbackState::DeliveryUnknown
                | McpCallbackState::Expired
                | McpCallbackState::Interrupted
        ) {
            return Err(McpCallbackError::Invalid("invalid system settlement"));
        }
        let mut connection = connection(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let mut callback = load(&transaction, callback_id)?.ok_or(McpCallbackError::NotFound)?;
        if let Some(expected_version) = expected_version
            && callback.version != expected_version
        {
            return Err(McpCallbackError::VersionConflict {
                expected: expected_version,
                actual: callback.version,
            });
        }
        if expected_version.is_some() && callback.state != McpCallbackState::AwaitingResolution {
            return Err(if callback.state.is_terminal() {
                McpCallbackError::Terminal(callback.state)
            } else {
                McpCallbackError::IllegalTransition {
                    from: callback.state,
                    to: state,
                }
            });
        }
        if callback.state.is_terminal() {
            return Err(McpCallbackError::Terminal(callback.state));
        }
        let event = McpCallbackEvent {
            callback_id,
            expected_version: callback.version,
            state,
            resolver_actor: callback.resolver_actor,
            action: callback.action,
            artifact_refs: callback.artifact_refs.clone(),
            terminal_error: error.clone(),
        };
        let command = McpCallbackCommand::Settle {
            callback_id,
            expected_version: callback.version,
            state,
            terminal_error: error,
        };
        callback.apply(&event)?;
        insert_event(&transaction, &command, &event, &callback, callback.version)?;
        save(&transaction, &callback)?;
        transaction
            .execute(
                "DELETE FROM mcp_callback_resolution_reservations WHERE callback_id=?1",
                [callback_id.to_string()],
            )
            .map_err(store)?;
        transaction.commit().map_err(store)?;
        Ok(callback)
    }

    pub fn interrupt(
        &self,
        callback_id: McpCallbackId,
        error: String,
    ) -> Result<McpCallbackProjection, McpCallbackError> {
        let mut connection = connection(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let mut callback = load(&transaction, callback_id)?.ok_or(McpCallbackError::NotFound)?;
        if callback.state.is_terminal() {
            return Err(McpCallbackError::Terminal(callback.state));
        }
        let state = if callback.state == McpCallbackState::ResponsePrepared {
            McpCallbackState::DeliveryUnknown
        } else {
            McpCallbackState::Interrupted
        };
        let error = if state == McpCallbackState::DeliveryUnknown {
            "delivery_unknown".to_owned()
        } else {
            error
        };
        let event = McpCallbackEvent {
            callback_id,
            expected_version: callback.version,
            state,
            resolver_actor: callback.resolver_actor,
            action: callback.action,
            artifact_refs: callback.artifact_refs.clone(),
            terminal_error: Some(error.clone()),
        };
        let command = McpCallbackCommand::Settle {
            callback_id,
            expected_version: callback.version,
            state,
            terminal_error: Some(error),
        };
        callback.apply(&event)?;
        insert_event(&transaction, &command, &event, &callback, callback.version)?;
        save(&transaction, &callback)?;
        transaction.commit().map_err(store)?;
        Ok(callback)
    }

    pub fn get(&self, id: McpCallbackId) -> Result<McpCallbackProjection, McpCallbackError> {
        let connection = connection(&self.database)?;
        load(&connection, id)?.ok_or(McpCallbackError::NotFound)
    }

    pub fn pending(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<McpCallbackProjection>, McpCallbackError> {
        let connection = connection(&self.database)?;
        let mut statement = connection.prepare(
            "SELECT projection FROM mcp_callback_projection WHERE project_id=?1 AND state='awaiting_resolution' ORDER BY callback_id"
        ).map_err(store)?;
        let rows = statement
            .query_map([project_id.to_string()], |row| row.get::<_, Vec<u8>>(0))
            .map_err(store)?;
        rows.map(|row| {
            serde_json::from_slice(&row.map_err(store)?)
                .map_err(|error| McpCallbackError::Store(error.to_string()))
        })
        .collect()
    }

    pub fn erase_run(
        &self,
        project_id: ProjectId,
        run_id: RunId,
    ) -> Result<usize, McpCallbackError> {
        self.erase_scope(Some(project_id), Some(run_id))
    }

    fn erase_scope(
        &self,
        project_id: Option<ProjectId>,
        run_id: Option<RunId>,
    ) -> Result<usize, McpCallbackError> {
        let mut connection = connection(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store)?;
        let (sql, value) = match (project_id, run_id) {
            (Some(project_id), Some(run_id)) => {
                let callbacks = transaction
                    .prepare(
                        "SELECT projection.projection
                         FROM mcp_callback_scope_index AS scope
                         JOIN mcp_callback_projection AS projection
                           ON projection.callback_id=scope.callback_id
                         WHERE scope.project_id=?1 AND scope.run_id=?2",
                    )
                    .map_err(store)?
                    .query_map(params![project_id.to_string(), run_id.to_string()], |row| {
                        row.get::<_, Vec<u8>>(0)
                    })
                    .map_err(store)?
                    .map(|row| {
                        serde_json::from_slice::<McpCallbackProjection>(&row.map_err(store)?)
                            .map_err(json_store)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                return self.erase_callbacks(
                    transaction,
                    callbacks,
                    Some(project_id),
                    Some(run_id),
                );
            }
            (Some(project_id), None) => (
                "SELECT projection.projection FROM mcp_callback_scope_index AS scope
                 JOIN mcp_callback_projection AS projection
                   ON projection.callback_id=scope.callback_id
                 WHERE scope.project_id=?1",
                project_id.to_string(),
            ),
            (None, Some(_)) => return Ok(0),
            (None, None) => return Ok(0),
        };
        let callbacks = transaction
            .prepare(sql)
            .map_err(store)?
            .query_map([value], |row| row.get::<_, Vec<u8>>(0))
            .map_err(store)?
            .map(|row| {
                serde_json::from_slice::<McpCallbackProjection>(&row.map_err(store)?)
                    .map_err(json_store)
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.erase_callbacks(transaction, callbacks, project_id, run_id)
    }

    fn erase_callbacks(
        &self,
        transaction: Transaction<'_>,
        callbacks: Vec<McpCallbackProjection>,
        project_id: Option<ProjectId>,
        run_id: Option<RunId>,
    ) -> Result<usize, McpCallbackError> {
        let artifacts = callback_artifacts(&self.database)?;
        let callback_ids = callbacks
            .iter()
            .map(|callback| callback.id.to_string())
            .collect::<Vec<_>>();
        for callback in &callbacks {
            let mut references = callback
                .artifact_refs
                .iter()
                .map(|reference| {
                    ArtifactReference::parse(reference.as_str()).map_err(artifact_store)
                })
                .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
            references.extend(
                artifacts
                    .callback_references(
                        &callback.principal_id.to_string(),
                        &callback.project_id.to_string(),
                        &callback.run_id.to_string(),
                        &callback.id.to_string(),
                    )
                    .map_err(artifact_store)?,
            );
            let journal_references = transaction
                .prepare(
                    "SELECT artifact_reference FROM artifact_publication_journal
                     WHERE purpose='mcp_callback_content' AND subject_id=?1",
                )
                .map_err(store)?
                .query_map([callback.id.to_string()], |row| row.get::<_, String>(0))
                .map_err(store)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store)?;
            for reference in journal_references {
                references.insert(ArtifactReference::parse(&reference).map_err(artifact_store)?);
            }
            for reference in references {
                let lock = publication_lock(reference);
                let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let erased = artifacts
                    .erase_callback_reference(
                        reference,
                        &ArtifactEnvelopeBinding {
                            principal: callback.principal_id.to_string(),
                            project: callback.project_id.to_string(),
                            run: callback.run_id.to_string(),
                            purpose: "mcp_callback_content".to_owned(),
                            invocation_id: None,
                            callback_id: Some(callback.id.to_string()),
                        },
                    )
                    .map_err(artifact_store)?;
                if erased
                    && (artifacts
                        .open_reference_optional(reference)
                        .map_err(artifact_store)?
                        .is_some()
                        || artifacts.staged_publication(reference).is_ok())
                {
                    return Err(McpCallbackError::Store(
                        "callback artifact remained accessible after erasure".to_owned(),
                    ));
                }
            }
        }
        for callback_id in &callback_ids {
            let event_ids = transaction
                .prepare("SELECT event_id FROM events WHERE stream=?1")
                .map_err(store)?
                .query_map([callback_id], |row| row.get::<_, String>(0))
                .map_err(store)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store)?;
            for event_id in event_ids {
                transaction
                    .execute(
                        "DELETE FROM deletion_backup_contents WHERE object_key=?1",
                        [format!("event:{event_id}")],
                    )
                    .map_err(store)?;
            }
        }
        let erased = scrub_callback_scope(
            &transaction,
            project_id.as_ref().map(ToString::to_string).as_deref(),
            run_id.as_ref().map(ToString::to_string).as_deref(),
            now_unix_micros(&transaction)?,
        )?;
        transaction.commit().map_err(store)?;
        Ok(erased)
    }

    pub fn interrupt_inflight(&self) -> Result<usize, McpCallbackError> {
        let ids = {
            let connection = connection(&self.database)?;
            let mut statement = connection.prepare(
                "SELECT callback_id FROM mcp_callback_projection WHERE state IN ('requested','awaiting_resolution','resolved','response_prepared')"
            ).map_err(store)?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(store)?
                .map(|row| {
                    McpCallbackId::from_str(&row.map_err(store)?)
                        .map_err(|error| McpCallbackError::Store(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut interrupted = 0;
        for id in &ids {
            match self.interrupt(*id, "outcome_unknown".to_owned()) {
                Ok(_) => interrupted += 1,
                Err(McpCallbackError::Terminal(_)) | Err(McpCallbackError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(interrupted)
    }

    pub fn reconcile_startup(&self) -> Result<usize, McpCallbackError> {
        self.reconcile_artifact_publications()?;
        self.interrupt_inflight()
    }

    pub fn authority_live(&self, callback: &McpCallbackProjection) -> Result<(), McpCallbackError> {
        let connection = connection(&self.database)?;
        guard_claim(&connection, callback)
    }
}

fn resolution_publication(
    database: &Path,
    principal_id: PrincipalId,
    project_id: ProjectId,
    run_id: RunId,
    callback_id: McpCallbackId,
    action: McpCallbackAction,
    artifact_refs: &[McpCallbackArtifactRef],
) -> Result<Option<ArtifactPublication>, McpCallbackError> {
    if action != McpCallbackAction::Accept {
        return Ok(None);
    }
    if artifact_refs.is_empty()
        && load(&connection(database)?, callback_id)?.is_some_and(|callback| {
            matches!(
                callback.mode,
                crate::domain::mcp_callback::McpCallbackMode::SamplingRequest
                    | crate::domain::mcp_callback::McpCallbackMode::SamplingResponse
                    | crate::domain::mcp_callback::McpCallbackMode::Url
            )
        })
    {
        return Ok(None);
    }
    let reference = artifact_refs.first().ok_or(McpCallbackError::Invalid(
        "accepted callback artifact is missing",
    ))?;
    let reference = ArtifactReference::parse(reference.as_str()).map_err(artifact_store)?;
    let artifacts = callback_artifacts(database)?;
    let publication = artifacts
        .staged_publication(reference)
        .map_err(artifact_store)?;
    let manifest = publication.manifest();
    if manifest.principal != principal_id.to_string()
        || manifest.project != project_id.to_string()
        || manifest.media_type != "application/vnd.kit.artifact-envelope"
        || manifest.retention
            != ArtifactRetention::UntilUnixMicros(authority_artifact_expiry(database, callback_id)?)
    {
        return Err(McpCallbackError::Authority);
    }
    artifacts
        .read_staged_publication(
            &publication,
            &ArtifactEnvelopeBinding {
                principal: principal_id.to_string(),
                project: project_id.to_string(),
                run: run_id.to_string(),
                purpose: "mcp_callback_content".to_owned(),
                invocation_id: None,
                callback_id: Some(callback_id.to_string()),
            },
            8 * 1024 * 1024,
        )
        .map_err(artifact_store)?;
    Ok(Some(publication))
}

fn authority_artifact_expiry(
    database: &Path,
    callback_id: McpCallbackId,
) -> Result<i64, McpCallbackError> {
    Ok(load(&connection(database)?, callback_id)?
        .ok_or(McpCallbackError::NotFound)?
        .artifact_expires_at
        .unix_micros())
}

#[allow(clippy::too_many_arguments)]
fn validate_resolution(
    transaction: &Transaction<'_>,
    principal_id: PrincipalId,
    project_id: ProjectId,
    callback_id: McpCallbackId,
    expected_version: u64,
    challenge_generation: u64,
    schema_digest: &str,
    idempotency_key: &IdempotencyKey,
    request_digest: [u8; 32],
    authority: &McpCallbackProjection,
    authority_recheck: &dyn Fn(&McpCallbackProjection) -> bool,
    authoritative_workspace_revision: &str,
) -> Result<McpCallbackProjection, McpCallbackError> {
    let callback = load(transaction, callback_id)?.ok_or(McpCallbackError::NotFound)?;
    if callback.principal_id != principal_id || callback.project_id != project_id {
        return Err(McpCallbackError::NotFound);
    }
    if callback.version != expected_version {
        return Err(McpCallbackError::VersionConflict {
            expected: expected_version,
            actual: callback.version,
        });
    }
    if callback.state.is_terminal() {
        return Err(McpCallbackError::Terminal(callback.state));
    }
    if callback.state != McpCallbackState::AwaitingResolution {
        return Err(McpCallbackError::IllegalTransition {
            from: callback.state,
            to: McpCallbackState::Resolved,
        });
    }
    if callback.challenge_generation != challenge_generation
        || callback.schema_digest != schema_digest
    {
        return Err(McpCallbackError::Authority);
    }
    if now_unix_micros(transaction)? >= callback.expires_at.unix_micros() {
        return Err(McpCallbackError::Expired);
    }
    let reserved: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM mcp_callback_resolution_reservations
             WHERE callback_id=?1 AND principal_id=?2 AND idempotency_key=?3
               AND request_digest=?4 AND expected_version=?5)",
            params![
                callback_id.to_string(),
                principal_id.to_string(),
                idempotency_key.as_str(),
                request_digest.as_slice(),
                expected_version,
            ],
            |row| row.get(0),
        )
        .map_err(store)?;
    if !reserved || !same_authority(&callback, authority) || !authority_recheck(&callback) {
        return Err(McpCallbackError::Authority);
    }
    guard_workspace_revision(transaction, &callback, authoritative_workspace_revision)?;
    guard_claim(transaction, &callback)?;
    Ok(callback)
}

fn verify_committed_resolution_artifact(
    database: &Path,
    callback: &McpCallbackProjection,
) -> Result<(), McpCallbackError> {
    if callback.mode != crate::domain::mcp_callback::McpCallbackMode::Form
        || callback.action != Some(McpCallbackAction::Accept)
    {
        return Ok(());
    }
    let reference = callback
        .artifact_refs
        .first()
        .ok_or(McpCallbackError::Invalid(
            "committed callback artifact is missing",
        ))?;
    let reference = ArtifactReference::parse(reference.as_str()).map_err(artifact_store)?;
    let artifacts = callback_artifacts(database)?;
    let artifact = artifacts
        .open_reference(reference)
        .map_err(artifact_store)?;
    if artifact.manifest().principal != callback.principal_id.to_string()
        || artifact.manifest().project != callback.project_id.to_string()
        || artifact.manifest().retention
            != ArtifactRetention::UntilUnixMicros(callback.artifact_expires_at.unix_micros())
    {
        return Err(McpCallbackError::Authority);
    }
    artifacts
        .with_reference_reader(reference, |_, reader| {
            let mut envelope = Vec::new();
            reader
                .take(callback.max_content_bytes as u64 + 16 * 1024)
                .read_to_end(&mut envelope)?;
            ArtifactEnvelopeBinding {
                principal: callback.principal_id.to_string(),
                project: callback.project_id.to_string(),
                run: callback.run_id.to_string(),
                purpose: "mcp_callback_content".to_owned(),
                invocation_id: None,
                callback_id: Some(callback.id.to_string()),
            }
            .open(&envelope)?;
            Ok(())
        })
        .map_err(artifact_store)
}

fn set_publication_state(
    database: &Path,
    reference: ArtifactReference,
    state: &str,
) -> Result<(), McpCallbackError> {
    let changed = connection(database)?
        .execute(
            "UPDATE artifact_publication_journal SET state=?2 WHERE artifact_reference=?1",
            params![reference.to_string(), state],
        )
        .map_err(store)?;
    if changed == 1 {
        Ok(())
    } else {
        Err(McpCallbackError::Store(
            "artifact publication intent disappeared".to_owned(),
        ))
    }
}

fn delete_publication_journal(
    database: &Path,
    reference: ArtifactReference,
) -> Result<(), McpCallbackError> {
    connection(database)?
        .execute(
            "DELETE FROM artifact_publication_journal WHERE artifact_reference=?1",
            [reference.to_string()],
        )
        .map(drop)
        .map_err(store)
}

fn callback_artifacts(database: &Path) -> Result<ArtifactStore, McpCallbackError> {
    let root = database
        .parent()
        .ok_or_else(|| McpCallbackError::Store("callback database has no parent".to_owned()))?;
    ArtifactStore::open(root.join("artifacts")).map_err(artifact_store)
}

fn artifact_store(error: crate::store::artifacts::ArtifactError) -> McpCallbackError {
    McpCallbackError::Store(error.to_string())
}

fn erase_callback_artifacts(
    transaction: &Transaction<'_>,
    callback: &McpCallbackProjection,
) -> Result<(), McpCallbackError> {
    let database = transaction
        .query_row(
            "SELECT file FROM pragma_database_list WHERE name='main'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(store)?;
    if database.is_empty() {
        return Err(McpCallbackError::Store(
            "callback database path is unavailable".to_owned(),
        ));
    }
    let artifacts = callback_artifacts(Path::new(&database))?;
    let mut references = callback
        .artifact_refs
        .iter()
        .map(|reference| ArtifactReference::parse(reference.as_str()).map_err(artifact_store))
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    references.extend(
        artifacts
            .callback_references(
                &callback.principal_id.to_string(),
                &callback.project_id.to_string(),
                &callback.run_id.to_string(),
                &callback.id.to_string(),
            )
            .map_err(artifact_store)?,
    );
    let journal = transaction
        .prepare(
            "SELECT artifact_reference FROM artifact_publication_journal
             WHERE purpose='mcp_callback_content' AND subject_id=?1",
        )
        .map_err(store)?
        .query_map([callback.id.to_string()], |row| row.get::<_, String>(0))
        .map_err(store)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store)?;
    for reference in journal {
        references.insert(ArtifactReference::parse(&reference).map_err(artifact_store)?);
    }
    for reference in references {
        let lock = publication_lock(reference);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        artifacts
            .erase_callback_reference(
                reference,
                &ArtifactEnvelopeBinding {
                    principal: callback.principal_id.to_string(),
                    project: callback.project_id.to_string(),
                    run: callback.run_id.to_string(),
                    purpose: "mcp_callback_content".to_owned(),
                    invocation_id: None,
                    callback_id: Some(callback.id.to_string()),
                },
            )
            .map_err(artifact_store)?;
    }
    Ok(())
}

pub(crate) fn scrub_callback_runs(
    transaction: &Transaction<'_>,
    run_ids: &[String],
    now_unix_micros: i64,
) -> Result<usize, McpCallbackError> {
    let mut erased = 0;
    for run_id in run_ids {
        erased += scrub_callback_scope(transaction, None, Some(run_id), now_unix_micros)?;
    }
    Ok(erased)
}

pub(crate) fn scrub_callback_transcript(
    transaction: &Transaction<'_>,
    thread_id: &str,
    now_unix_micros: i64,
) -> Result<usize, McpCallbackError> {
    let run_ids = transaction
        .prepare(
            "SELECT DISTINCT run_id FROM mcp_callback_scope_index
             WHERE thread_id=?1 ORDER BY run_id",
        )
        .map_err(store)?
        .query_map([thread_id], |row| row.get::<_, String>(0))
        .map_err(store)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store)?;
    scrub_callback_runs(transaction, &run_ids, now_unix_micros)
}

pub(crate) fn scrub_callback(
    transaction: &Transaction<'_>,
    callback_id: &str,
    now_unix_micros: i64,
) -> Result<(), McpCallbackError> {
    let callback = transaction
        .query_row(
            "SELECT projection FROM mcp_callback_projection WHERE callback_id=?1",
            [callback_id],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(store)?
        .map(|bytes| serde_json::from_slice::<McpCallbackProjection>(&bytes).map_err(json_store))
        .transpose()?;
    if let Some(callback) = &callback {
        erase_callback_artifacts(transaction, callback)?;
    }
    scrub_callback_id(transaction, callback_id, now_unix_micros)?;
    if let Some(callback) = callback {
        let workspace_id = callback.workspace_id.to_string();
        transaction
            .execute(
                "DELETE FROM mcp_callback_workspace_revisions
                 WHERE workspace_id=?1 AND NOT EXISTS(
                   SELECT 1 FROM mcp_callback_projection WHERE workspace_id=?1
                 )",
                [workspace_id],
            )
            .map_err(store)?;
    }
    Ok(())
}

fn scrub_callback_scope(
    transaction: &Transaction<'_>,
    project_id: Option<&str>,
    run_id: Option<&str>,
    now_unix_micros: i64,
) -> Result<usize, McpCallbackError> {
    if project_id.is_none() && run_id.is_none() {
        return Ok(0);
    }
    let ids = transaction
        .prepare(
            "SELECT projection.callback_id, scope.run_id, projection.workspace_id,
                    projection.projection
             FROM mcp_callback_scope_index AS scope
             JOIN mcp_callback_projection AS projection
               ON projection.callback_id=scope.callback_id
             WHERE (?1 IS NULL OR scope.project_id=?1) AND (?2 IS NULL OR scope.run_id=?2)",
        )
        .map_err(store)?
        .query_map(params![project_id, run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .map_err(store)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store)?;
    for (_, _, _, projection) in &ids {
        let callback: McpCallbackProjection =
            serde_json::from_slice(projection).map_err(json_store)?;
        erase_callback_artifacts(transaction, &callback)?;
    }
    for (callback_id, _, _, _) in &ids {
        scrub_callback_id(transaction, callback_id, now_unix_micros)?;
    }
    let workspace_ids = ids
        .iter()
        .map(|(_, _, workspace_id, _)| workspace_id)
        .collect::<std::collections::BTreeSet<_>>();
    for workspace_id in workspace_ids {
        transaction
            .execute(
                "DELETE FROM mcp_callback_workspace_revisions
                 WHERE workspace_id=?1 AND NOT EXISTS(
                   SELECT 1 FROM mcp_callback_projection WHERE workspace_id=?1
                 )",
                [workspace_id],
            )
            .map_err(store)?;
    }
    Ok(ids.len())
}

fn scrub_callback_id(
    transaction: &Transaction<'_>,
    callback_id: &str,
    now_unix_micros: i64,
) -> Result<(), McpCallbackError> {
    let stream = callback_id.to_owned();
    let positions = transaction
        .prepare("SELECT commit_position FROM events WHERE stream=?1")
        .map_err(store)?
        .query_map([&stream], |row| row.get::<_, i64>(0))
        .map_err(store)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store)?;
    transaction
        .execute(
            "UPDATE events SET event_type='retention.erased',
           payload=X'7B22657261736564223A747275652C22736368656D615F76657273696F6E223A317D',
           artifacts=X'5B5D' WHERE stream=?1",
            [&stream],
        )
        .map_err(store)?;
    for position in &positions {
        transaction
            .execute(
                "UPDATE event_projection_index SET erased=1, thread_id=NULL, run_id=NULL
             WHERE commit_position=?1",
                [position],
            )
            .map_err(store)?;
    }
    transaction
        .execute(
            "DELETE FROM mcp_callback_events WHERE callback_id=?1",
            [callback_id],
        )
        .map_err(store)?;
    transaction
        .execute(
            "DELETE FROM mcp_callback_idempotency WHERE callback_id=?1",
            [callback_id],
        )
        .map_err(store)?;
    transaction
        .execute(
            "DELETE FROM mcp_callback_resolution_reservations WHERE callback_id=?1",
            [callback_id],
        )
        .map_err(store)?;
    transaction
        .execute(
            "DELETE FROM artifact_publication_journal
             WHERE purpose='mcp_callback_content' AND subject_id=?1",
            [callback_id],
        )
        .map_err(store)?;
    transaction
        .execute(
            "DELETE FROM mcp_callback_projection WHERE callback_id=?1",
            [callback_id],
        )
        .map_err(store)?;
    transaction
        .execute(
            "DELETE FROM mcp_callback_scope_index WHERE callback_id=?1",
            [callback_id],
        )
        .map_err(store)?;
    transaction
        .execute(
            "INSERT INTO deletion_tombstones
         (target_sha256, object_kind, completed_at_unix_micros, erased_event_count, outcome)
         VALUES (?1,'mcp_callback',?2,?3,'erased') ON CONFLICT(target_sha256) DO NOTHING",
            params![
                sha256(format!("mcp_callback:{callback_id}").as_bytes()).as_slice(),
                now_unix_micros,
                positions.len(),
            ],
        )
        .map_err(store)?;
    Ok(())
}

fn same_request(left: &McpCallbackProjection, right: &McpCallbackProjection) -> bool {
    left.id == right.id
        && left.server_id == right.server_id
        && left.kind == right.kind
        && left.mode == right.mode
        && left.principal_id == right.principal_id
        && left.project_id == right.project_id
        && left.run_id == right.run_id
        && left.workspace_id == right.workspace_id
        && left.workspace_revision == right.workspace_revision
        && left.request_id == right.request_id
        && left.request == right.request
        && left.request_digest == right.request_digest
        && left.schema_digest == right.schema_digest
        && left.attempt_id == right.attempt_id
        && left.fence == right.fence
        && left.claim_generation == right.claim_generation
        && left.challenge_generation == right.challenge_generation
        && left.operation_sequence == right.operation_sequence
        && left.url_binding == right.url_binding
}

fn same_authority(left: &McpCallbackProjection, right: &McpCallbackProjection) -> bool {
    left.principal_id == right.principal_id
        && left.project_id == right.project_id
        && left.run_id == right.run_id
        && left.attempt_id == right.attempt_id
        && left.fence == right.fence
        && left.claim_generation == right.claim_generation
        && left.workspace_id == right.workspace_id
        && left.workspace_revision == right.workspace_revision
        && left.server_id == right.server_id
        && left.kind == right.kind
        && left.mode == right.mode
        && left.request_id == right.request_id
        && left.request == right.request
        && left.request_digest == right.request_digest
        && left.challenge_generation == right.challenge_generation
        && left.operation_sequence == right.operation_sequence
        && left.schema_digest == right.schema_digest
        && left.url_binding == right.url_binding
}

fn guard_claim(
    connection: &Connection,
    callback: &McpCallbackProjection,
) -> Result<(), McpCallbackError> {
    let live: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM attempt_driver_claims WHERE run_id=?1 AND attempt_id=?2 AND principal_id=?3 AND fence=?4 AND lease_version=?5 AND quiescent=0 AND expires_at_unix_micros>?6)",
        params![callback.run_id.to_string(), callback.attempt_id.to_string(), callback.principal_id.to_string(), callback.fence.get(), callback.claim_generation, now_unix_micros(connection)?],
        |row| row.get(0),
    ).map_err(store)?;
    if live {
        Ok(())
    } else {
        Err(McpCallbackError::Authority)
    }
}

fn guard_workspace_revision(
    transaction: &Transaction<'_>,
    callback: &McpCallbackProjection,
    authoritative_revision: &str,
) -> Result<(), McpCallbackError> {
    if authoritative_revision != callback.workspace_revision {
        return Err(McpCallbackError::Authority);
    }
    let guarded = transaction
        .query_row(
            "UPDATE mcp_callback_workspace_revisions SET lease_version=lease_version
             WHERE workspace_id=?1 AND revision=?2
             RETURNING lease_version",
            params![callback.workspace_id.to_string(), authoritative_revision],
            |row| row.get::<_, u64>(0),
        )
        .optional()
        .map_err(store)?;
    guarded
        .filter(|version| *version > 0)
        .map(drop)
        .ok_or(McpCallbackError::Authority)
}

fn now_unix_micros(connection: &Connection) -> Result<i64, McpCallbackError> {
    connection
        .query_row(
            "SELECT CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)",
            [],
            |row| row.get(0),
        )
        .map_err(store)
}

fn load(
    connection: &Connection,
    id: McpCallbackId,
) -> Result<Option<McpCallbackProjection>, McpCallbackError> {
    connection
        .query_row(
            "SELECT projection FROM mcp_callback_projection WHERE callback_id=?1",
            [id.to_string()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(store)?
        .map(|bytes| {
            let callback: McpCallbackProjection = serde_json::from_slice(&bytes)
                .map_err(|error| McpCallbackError::Store(error.to_string()))?;
            callback.validate()?;
            Ok(callback)
        })
        .transpose()
}

fn save(
    transaction: &Transaction<'_>,
    callback: &McpCallbackProjection,
) -> Result<(), McpCallbackError> {
    let bytes =
        serde_json::to_vec(callback).map_err(|error| McpCallbackError::Store(error.to_string()))?;
    transaction.execute(
        "INSERT INTO mcp_callback_projection (callback_id, project_id, principal_id, run_id, workspace_id, state, version, projection) VALUES (?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(callback_id) DO UPDATE SET state=excluded.state, version=excluded.version, projection=excluded.projection",
        params![callback.id.to_string(), callback.project_id.to_string(), callback.principal_id.to_string(), callback.run_id.to_string(), callback.workspace_id.to_string(), state_name(callback.state), callback.version, bytes],
    ).map_err(store)?;
    transaction
        .execute(
            "INSERT INTO mcp_callback_scope_index
             (callback_id, principal_id, project_id, run_id, attempt_id, thread_id)
             VALUES (?1,?2,?3,?4,?5,NULL)
             ON CONFLICT(callback_id) DO UPDATE SET
               principal_id=excluded.principal_id,
               project_id=excluded.project_id,
               run_id=excluded.run_id,
               attempt_id=excluded.attempt_id",
            params![
                callback.id.to_string(),
                callback.principal_id.to_string(),
                callback.project_id.to_string(),
                callback.run_id.to_string(),
                callback.attempt_id.to_string(),
            ],
        )
        .map_err(store)?;
    let has_event_index: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='event_projection_index')",
            [],
            |row| row.get(0),
        )
        .map_err(store)?;
    if has_event_index {
        transaction
            .execute(
                "UPDATE mcp_callback_scope_index
                 SET thread_id=(SELECT thread_id FROM event_projection_index
                                WHERE run_id=?2 AND thread_id IS NOT NULL
                                ORDER BY commit_position LIMIT 1)
                 WHERE callback_id=?1 AND thread_id IS NULL",
                params![callback.id.to_string(), callback.run_id.to_string()],
            )
            .map_err(store)?;
    }
    Ok(())
}

fn insert_event(
    transaction: &Transaction<'_>,
    command: &McpCallbackCommand,
    event: &McpCallbackEvent,
    callback: &McpCallbackProjection,
    version: u64,
) -> Result<CommitPosition, McpCallbackError> {
    let stored_at_unix_micros = now_unix_micros(transaction)?;
    let mut central_event = event.clone();
    let central_command = match command {
        McpCallbackCommand::Resolve {
            callback_id,
            expected_version,
            challenge_generation,
            schema_digest,
            resolver_actor,
            action,
            ..
        } => McpCallbackCommand::Resolve {
            callback_id: *callback_id,
            expected_version: *expected_version,
            challenge_generation: *challenge_generation,
            schema_digest: schema_digest.clone(),
            resolver_actor: *resolver_actor,
            action: *action,
            artifact_refs: Vec::new(),
        },
        command => command.clone(),
    };
    if !central_event.state.is_terminal() {
        central_event.artifact_refs.clear();
    }
    let payload = serde_json::to_vec(&McpCallbackEventEnvelope {
        principal_id: callback.principal_id,
        project_id: callback.project_id,
        run_id: callback.run_id,
        stored_at_unix_micros,
        command: central_command,
        event: central_event,
    })
    .map_err(json_store)?;
    let artifacts = serde_json::to_vec(if event.state.is_terminal() {
        event.artifact_refs.as_slice()
    } else {
        &[] as &[McpCallbackArtifactRef]
    })
    .map_err(json_store)?;
    let identity = format!("{}:{version}", event.callback_id);
    let central = NewEvent {
        id: EventId::from_stable_bytes(format!("mcp-callback-event:{identity}").as_bytes()),
        stream: EntityId::McpCallback(event.callback_id),
        event_type: EventType::parse("mcp_callback.transitioned")
            .expect("callback event type is valid"),
        schema_version: SchemaVersion::CURRENT,
        occurred_at: UtcDateTime::now()
            .map_err(|error| McpCallbackError::Store(error.to_string()))?,
        causation_id: CommandId::from_stable_bytes(
            format!("mcp-callback-command:{identity}").as_bytes(),
        ),
        correlation_id: EntityId::Run(callback.run_id),
        attempt_id: Some(callback.attempt_id),
        trace_id: TraceId::parse("mcp-callback").expect("callback trace id is valid"),
        payload,
        artifacts,
    };
    let position = append_canonical_event(
        transaction,
        &central,
        ExpectedVersion::new(version.saturating_sub(1)).get(),
    )
    .map_err(|error| McpCallbackError::Store(error.to_string()))?;
    let has_event_index: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='event_projection_index')",
            [],
            |row| row.get(0),
        )
        .map_err(store)?;
    if has_event_index {
        transaction
            .execute(
                "INSERT INTO event_projection_index
                 (commit_position, project_id, thread_id, run_id, event_class, stored_at_unix_micros, erased)
                 VALUES (?1,?2,NULL,?3,?4,?5,0)",
                 params![
                     position.get(),
                     callback.project_id.to_string(),
                     callback.run_id.to_string(),
                     if event.state.is_terminal() { "terminal" } else { "event" },
                     stored_at_unix_micros,
                ],
            )
            .map_err(store)?;
        transaction
            .execute(
                "UPDATE event_projection_index_state SET indexed_through=?1
                 WHERE singleton=1 AND indexed_through=?2",
                params![position.get(), position.get().saturating_sub(1)],
            )
            .map_err(store)?;
    }
    transaction.execute(
        "INSERT INTO mcp_callback_events (callback_id, version, command, event, artifact_refs) VALUES (?1,?2,?3,?4,?5)",
        params![event.callback_id.to_string(), version, serde_json::to_vec(command).map_err(json_store)?, serde_json::to_vec(event).map_err(json_store)?, serde_json::to_vec(&event.artifact_refs).map_err(json_store)?],
    ).map_err(store)?;
    if event.state.is_terminal() {
        transaction
            .execute(
                "DELETE FROM artifact_publication_journal
                 WHERE purpose='mcp_callback_content' AND subject_id=?1",
                [event.callback_id.to_string()],
            )
            .map_err(store)?;
    }
    Ok(position)
}

fn state_name(state: McpCallbackState) -> &'static str {
    match state {
        McpCallbackState::Requested => "requested",
        McpCallbackState::AwaitingResolution => "awaiting_resolution",
        McpCallbackState::Resolved => "resolved",
        McpCallbackState::ResponsePrepared => "response_prepared",
        McpCallbackState::Delivered => "delivered",
        McpCallbackState::DeliveryUnknown => "delivery_unknown",
        McpCallbackState::Expired => "expired",
        McpCallbackState::Interrupted => "interrupted",
    }
}

fn connection(path: &Path) -> Result<Connection, McpCallbackError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(store)?;
    connection.busy_timeout(BUSY_TIMEOUT).map_err(store)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(store)?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(store)?;
    Ok(connection)
}

fn migrate(connection: &mut Connection) -> Result<(), McpCallbackError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(store)?;
    transaction.execute_batch(
         "CREATE TABLE IF NOT EXISTS mcp_callback_projection (
             callback_id TEXT PRIMARY KEY, project_id TEXT NOT NULL, principal_id TEXT NOT NULL,
             run_id TEXT NOT NULL, workspace_id TEXT NOT NULL,
             state TEXT NOT NULL CHECK(state IN ('requested','awaiting_resolution','resolved','response_prepared','delivered','delivery_unknown','expired','interrupted')),
             version INTEGER NOT NULL CHECK(version > 0), projection BLOB NOT NULL
         );
         CREATE INDEX IF NOT EXISTS mcp_callback_pending_project ON mcp_callback_projection(project_id, state);
         CREATE TABLE IF NOT EXISTS mcp_callback_scope_index (
             callback_id TEXT PRIMARY KEY, principal_id TEXT NOT NULL, project_id TEXT NOT NULL,
             run_id TEXT NOT NULL, attempt_id TEXT NOT NULL, thread_id TEXT
         );
         CREATE INDEX IF NOT EXISTS mcp_callback_scope_project
           ON mcp_callback_scope_index(project_id, callback_id);
         CREATE INDEX IF NOT EXISTS mcp_callback_scope_run
           ON mcp_callback_scope_index(run_id, callback_id);
         CREATE INDEX IF NOT EXISTS mcp_callback_scope_thread
           ON mcp_callback_scope_index(thread_id, callback_id);
         CREATE TABLE IF NOT EXISTS mcp_callback_events (
             callback_id TEXT NOT NULL, version INTEGER NOT NULL CHECK(version > 0),
             command BLOB NOT NULL, event BLOB NOT NULL, artifact_refs BLOB NOT NULL,
             PRIMARY KEY(callback_id, version)
         );
         CREATE TABLE IF NOT EXISTS mcp_callback_idempotency (
             principal_id TEXT NOT NULL, callback_id TEXT NOT NULL, idempotency_key TEXT NOT NULL,
             request_digest BLOB NOT NULL CHECK(length(request_digest)=32), response BLOB NOT NULL,
             PRIMARY KEY(principal_id, callback_id, idempotency_key)
         );
         CREATE TABLE IF NOT EXISTS mcp_callback_resolution_reservations (
             callback_id TEXT PRIMARY KEY, principal_id TEXT NOT NULL,
             idempotency_key TEXT NOT NULL,
             request_digest BLOB NOT NULL CHECK(length(request_digest)=32),
             expected_version INTEGER NOT NULL CHECK(expected_version > 0)
         );
         CREATE TABLE IF NOT EXISTS artifact_publication_journal (
             artifact_reference TEXT PRIMARY KEY, artifact_digest TEXT NOT NULL,
             purpose TEXT NOT NULL, subject_id TEXT NOT NULL, principal_id TEXT NOT NULL,
             project_id TEXT NOT NULL, run_id TEXT NOT NULL,
             state TEXT NOT NULL DEFAULT 'intent_recorded'
               CHECK(state IN ('intent_recorded','staged_verified','promoted','resolution_committed')),
             created_at_unix_micros INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS mcp_callback_workspace_revisions (
             workspace_id TEXT PRIMARY KEY, revision TEXT NOT NULL,
             lease_version INTEGER NOT NULL CHECK(lease_version > 0)
         );"
    ).map_err(store)?;
    let has_run_id: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('mcp_callback_projection') WHERE name='run_id')",
            [],
            |row| row.get(0),
        )
        .map_err(store)?;
    if !has_run_id {
        transaction
            .execute_batch(
                "ALTER TABLE mcp_callback_projection ADD COLUMN run_id TEXT NOT NULL DEFAULT '';
                 UPDATE mcp_callback_projection
                 SET run_id=json_extract(CAST(projection AS TEXT), '$.run_id');",
            )
            .map_err(store)?;
    }
    let has_workspace_id: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('mcp_callback_projection') WHERE name='workspace_id')",
            [],
            |row| row.get(0),
        )
        .map_err(store)?;
    if !has_workspace_id {
        transaction
            .execute_batch(
                "ALTER TABLE mcp_callback_projection ADD COLUMN workspace_id TEXT NOT NULL DEFAULT '';
                 UPDATE mcp_callback_projection
                 SET workspace_id=json_extract(CAST(projection AS TEXT), '$.workspace_id');",
            )
            .map_err(store)?;
    }
    transaction
        .execute(
            "INSERT OR IGNORE INTO mcp_callback_scope_index
             (callback_id, principal_id, project_id, run_id, attempt_id, thread_id)
             SELECT callback_id, principal_id, project_id, run_id,
                    json_extract(CAST(projection AS TEXT), '$.attempt_id'), NULL
             FROM mcp_callback_projection",
            [],
        )
        .map_err(store)?;
    let has_event_index: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='event_projection_index')",
            [],
            |row| row.get(0),
        )
        .map_err(store)?;
    if has_event_index {
        transaction
            .execute(
                "UPDATE mcp_callback_scope_index AS scope
                 SET thread_id=(SELECT thread_id FROM event_projection_index
                                WHERE run_id=scope.run_id AND thread_id IS NOT NULL
                                ORDER BY commit_position LIMIT 1)
                 WHERE thread_id IS NULL",
                [],
            )
            .map_err(store)?;
    }
    let has_publication_state: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('artifact_publication_journal') WHERE name='state')",
            [],
            |row| row.get(0),
        )
        .map_err(store)?;
    if !has_publication_state {
        transaction
            .execute_batch(
                "ALTER TABLE artifact_publication_journal
                   ADD COLUMN state TEXT NOT NULL DEFAULT 'intent_recorded';",
            )
            .map_err(store)?;
    }
    let has_publication_created_at: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('artifact_publication_journal') WHERE name='created_at_unix_micros')",
            [],
            |row| row.get(0),
        )
        .map_err(store)?;
    if !has_publication_created_at {
        transaction
            .execute_batch(
                "ALTER TABLE artifact_publication_journal
                   ADD COLUMN created_at_unix_micros INTEGER NOT NULL DEFAULT 0;",
            )
            .map_err(store)?;
    }
    transaction.commit().map_err(store)
}

fn json_store(error: serde_json::Error) -> McpCallbackError {
    McpCallbackError::Store(error.to_string())
}

fn store(error: rusqlite::Error) -> McpCallbackError {
    McpCallbackError::Store(error.to_string())
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ResolutionResponse {
    callback: McpCallbackProjection,
    commit_positions: Vec<CommitPosition>,
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc, thread};

    use super::*;
    use crate::{
        api::service::AttemptDriverClaim,
        domain::{
            events::UtcDateTime,
            ids::{AttemptId, RunId, WorkspaceId},
            lifecycle::FencingToken,
            mcp_callback::{
                McpCallbackKind, McpCallbackMode, McpUndispatchedProof, McpUrlCallbackBinding,
                McpUrlDestination,
            },
        },
        runtime::daemon::ControlPlaneAuthority,
        store::artifacts::{
            ArtifactClass, ArtifactEnvelopeBinding, ArtifactMetadata, ArtifactRetention,
        },
        store::sqlite::{append::SqliteStore, projection::ProjectionStore},
    };

    #[test]
    fn publication_journal_columns_are_repaired_independently() {
        for existing_column in ["state", "created_at_unix_micros"] {
            let root = std::env::temp_dir().join(format!(
                "kit-mcp-callback-migration-{}-{existing_column}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir(&root).unwrap();
            let database = root.join("service.sqlite");
            let connection = Connection::open(&database).unwrap();
            let tail = if existing_column == "state" {
                ", state TEXT NOT NULL DEFAULT 'intent_recorded'"
            } else {
                ", created_at_unix_micros INTEGER NOT NULL DEFAULT 0"
            };
            connection
                .execute_batch(&format!(
                    "CREATE TABLE artifact_publication_journal (
                       artifact_reference TEXT PRIMARY KEY, artifact_digest TEXT NOT NULL,
                       purpose TEXT NOT NULL, subject_id TEXT NOT NULL, principal_id TEXT NOT NULL,
                       project_id TEXT NOT NULL, run_id TEXT NOT NULL{tail}
                     );"
                ))
                .unwrap();
            drop(connection);

            drop(McpCallbackStore::open(&database).unwrap());
            let connection = Connection::open(&database).unwrap();
            for column in ["state", "created_at_unix_micros"] {
                let exists: bool = connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('artifact_publication_journal') WHERE name=?1)",
                        [column],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(exists, "missing repaired column {column}");
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn cas_authority_idempotency_race_and_restart_are_durable() {
        let root = std::env::temp_dir().join(format!(
            "kit-mcp-callback-store-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let database = root.join("service.sqlite");
        let authority = ControlPlaneAuthority::for_test();
        let mut append = SqliteStore::open(&database, &authority).unwrap();
        drop(ProjectionStore::open(&database).unwrap());
        let callback = url_fixture(1);
        append
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id: callback.run_id,
                attempt_id: callback.attempt_id,
                principal_id: callback.principal_id,
                fence: callback.fence,
                lease_version: callback.claim_generation,
                expires_at_unix_micros: i64::MAX,
            })
            .unwrap();
        let store = McpCallbackStore::open(&database).unwrap();
        let mut expired = fixture(9);
        expired.expires_at = UtcDateTime::parse("2000-01-01T00:00:00Z").unwrap();
        assert!(matches!(
            store.request(expired.clone()),
            Err(McpCallbackError::Expired)
        ));
        assert!(matches!(
            store.get(expired.id),
            Err(McpCallbackError::NotFound)
        ));
        let awaiting = store.request(callback.clone()).unwrap();
        assert_eq!(awaiting.state, McpCallbackState::AwaitingResolution);
        assert_eq!(awaiting.version, 2);
        let pending = store.pending(callback.project_id).unwrap();
        assert_eq!(pending.as_slice(), std::slice::from_ref(&awaiting));

        let key = IdempotencyKey::parse("resolve").unwrap();
        for result in [
            store.resolve(
                PrincipalId::generate().unwrap(),
                callback.project_id,
                &key,
                callback.id,
                2,
                7,
                &callback.schema_digest,
                McpCallbackAction::Decline,
                Vec::new(),
                [1; 32],
                &callback,
            ),
            store.resolve(
                callback.principal_id,
                ProjectId::generate().unwrap(),
                &key,
                callback.id,
                2,
                7,
                &callback.schema_digest,
                McpCallbackAction::Decline,
                Vec::new(),
                [1; 32],
                &callback,
            ),
        ] {
            assert!(matches!(result, Err(McpCallbackError::NotFound)));
            assert_eq!(store.get(callback.id).unwrap(), awaiting);
        }
        for (version, generation, schema) in [
            (3, 7, callback.schema_digest.as_str()),
            (2, 8, callback.schema_digest.as_str()),
            (
                2,
                7,
                "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            ),
        ] {
            assert!(
                store
                    .resolve(
                        callback.principal_id,
                        callback.project_id,
                        &key,
                        callback.id,
                        version,
                        generation,
                        schema,
                        McpCallbackAction::Decline,
                        Vec::new(),
                        [1; 32],
                        &callback,
                    )
                    .is_err()
            );
            assert_eq!(store.get(callback.id).unwrap(), awaiting);
        }

        for trigger in [
            format!(
                "CREATE TRIGGER callback_fault BEFORE INSERT ON mcp_callback_events WHEN NEW.callback_id='{}' AND NEW.version=3 BEGIN SELECT RAISE(ABORT, 'fault'); END;",
                callback.id
            ),
            format!(
                "CREATE TRIGGER callback_fault BEFORE UPDATE ON mcp_callback_projection WHEN NEW.callback_id='{}' BEGIN SELECT RAISE(ABORT, 'fault'); END;",
                callback.id
            ),
            "CREATE TRIGGER callback_fault BEFORE INSERT ON mcp_callback_idempotency BEGIN SELECT RAISE(ABORT, 'fault'); END;".to_owned(),
        ] {
            store
                .reserve_resolution(
                    callback.principal_id,
                    callback.project_id,
                    callback.id,
                    &key,
                    [1; 32],
                    2,
                    7,
                    &callback.schema_digest,
                )
                .unwrap();
            let fault = connection(&database).unwrap();
            fault.execute_batch(&trigger).unwrap();
            assert!(store
                .resolve(
                    callback.principal_id,
                    callback.project_id,
                    &key,
                    callback.id,
                    2,
                    7,
                    &callback.schema_digest,
                    McpCallbackAction::Decline,
                    Vec::new(),
                    [1; 32],
                    &callback,
                )
                .is_err());
            assert_eq!(store.get(callback.id).unwrap(), awaiting);
            fault.execute_batch("DROP TRIGGER callback_fault").unwrap();
        }

        let artifacts = ArtifactStore::open(root.join("artifacts")).unwrap();
        let binding = ArtifactEnvelopeBinding {
            principal: callback.principal_id.to_string(),
            project: callback.project_id.to_string(),
            run: callback.run_id.to_string(),
            purpose: "mcp_callback_content".to_owned(),
            invocation_id: None,
            callback_id: Some(callback.id.to_string()),
        };
        let envelope = binding.seal(br#"{"name":"Ada"}"#).unwrap();
        let failed_reference = ArtifactReference::derive(b"callback-cas-fault", b"content");
        artifacts
            .stage_publication(
                &envelope,
                ArtifactMetadata::new(
                    "application/vnd.kit.artifact-envelope",
                    ArtifactClass::File,
                    callback.principal_id.to_string(),
                    callback.project_id.to_string(),
                    ArtifactRetention::UntilUnixMicros(callback.artifact_expires_at.unix_micros()),
                    1,
                )
                .unwrap(),
                failed_reference,
            )
            .unwrap();
        let fault = connection(&database).unwrap();
        fault
            .execute_batch(
                "CREATE TRIGGER callback_fault BEFORE INSERT ON mcp_callback_idempotency
                 BEGIN SELECT RAISE(ABORT, 'fault'); END;",
            )
            .unwrap();
        assert!(
            store
                .resolve(
                    callback.principal_id,
                    callback.project_id,
                    &key,
                    callback.id,
                    2,
                    7,
                    &callback.schema_digest,
                    McpCallbackAction::Accept,
                    vec![McpCallbackArtifactRef::parse(&failed_reference.to_string()).unwrap()],
                    [1; 32],
                    &callback,
                )
                .is_err()
        );
        fault.execute_batch("DROP TRIGGER callback_fault").unwrap();
        assert_eq!(store.get(callback.id).unwrap(), awaiting);
        assert!(artifacts.open_reference(failed_reference).is_err());
        assert!(artifacts.staged_publication(failed_reference).is_err());
        assert_eq!(
            fault
                .query_row(
                    "SELECT count(*) FROM artifact_publication_journal",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            0
        );

        assert!(matches!(
            store.resolve_with_recheck(
                callback.principal_id,
                callback.project_id,
                &key,
                callback.id,
                2,
                7,
                &callback.schema_digest,
                McpCallbackAction::Decline,
                Vec::new(),
                [1; 32],
                &callback,
                &|_| false,
                &callback.workspace_revision,
            ),
            Err(McpCallbackError::Authority)
        ));
        assert_eq!(store.get(callback.id).unwrap(), awaiting);

        connection(&database)
            .unwrap()
            .execute(
                "UPDATE mcp_callback_workspace_revisions
                 SET revision='concurrent-mutation', lease_version=lease_version+1
                 WHERE workspace_id=?1",
                [callback.workspace_id.to_string()],
            )
            .unwrap();
        assert!(matches!(
            store.resolve(
                callback.principal_id,
                callback.project_id,
                &key,
                callback.id,
                2,
                7,
                &callback.schema_digest,
                McpCallbackAction::Decline,
                Vec::new(),
                [1; 32],
                &callback,
            ),
            Err(McpCallbackError::Authority)
        ));
        connection(&database)
            .unwrap()
            .execute(
                "UPDATE mcp_callback_workspace_revisions
                 SET revision=?2, lease_version=lease_version+1 WHERE workspace_id=?1",
                params![
                    callback.workspace_id.to_string(),
                    callback.workspace_revision
                ],
            )
            .unwrap();

        let (declined, replayed, positions) = store
            .resolve(
                callback.principal_id,
                callback.project_id,
                &key,
                callback.id,
                2,
                7,
                &callback.schema_digest,
                McpCallbackAction::Decline,
                Vec::new(),
                [1; 32],
                &callback,
            )
            .unwrap();
        assert!(!replayed);
        assert_eq!(positions.len(), 1);
        assert_eq!(declined.state, McpCallbackState::Resolved);
        assert_eq!(declined.version, 3);
        for state in [McpCallbackState::Expired, McpCallbackState::Interrupted] {
            assert!(matches!(
                store.settle_awaiting(
                    callback.id,
                    awaiting.version,
                    state,
                    Some("stale_awaiting_observer".to_owned()),
                ),
                Err(McpCallbackError::VersionConflict {
                    expected: 2,
                    actual: 3
                })
            ));
            assert_eq!(store.get(callback.id).unwrap(), declined);
        }
        let expiring = store.request(fixture(99)).unwrap();
        let expired = store
            .settle_awaiting(
                expiring.id,
                expiring.version,
                McpCallbackState::Expired,
                Some("callback_expired".to_owned()),
            )
            .unwrap();
        assert_eq!(expired.state, McpCallbackState::Expired);
        assert_eq!(expired.version, expiring.version + 1);
        assert!(store.pending(callback.project_id).unwrap().is_empty());
        let (replay, replayed, replay_positions) = store
            .resolve(
                callback.principal_id,
                callback.project_id,
                &key,
                callback.id,
                2,
                7,
                &callback.schema_digest,
                McpCallbackAction::Decline,
                Vec::new(),
                [1; 32],
                &callback,
            )
            .unwrap();
        assert!(replayed);
        assert_eq!(replay, declined);
        assert_eq!(replay_positions, positions);
        connection(&database)
            .unwrap()
            .execute(
                "UPDATE attempt_driver_claims SET quiescent=1 WHERE run_id=?1",
                [callback.run_id.to_string()],
            )
            .unwrap();
        assert!(
            store
                .resolve(
                    callback.principal_id,
                    callback.project_id,
                    &key,
                    callback.id,
                    2,
                    7,
                    &callback.schema_digest,
                    McpCallbackAction::Decline,
                    Vec::new(),
                    [1; 32],
                    &callback,
                )
                .unwrap()
                .1
        );
        let new_mutation = fixture(10);
        store.request(new_mutation.clone()).unwrap();
        assert!(matches!(
            store.reserve_resolution(
                new_mutation.principal_id,
                new_mutation.project_id,
                new_mutation.id,
                &IdempotencyKey::parse("new-mutation").unwrap(),
                [7; 32],
                2,
                7,
                &new_mutation.schema_digest,
            ),
            Err(McpCallbackError::Authority)
        ));
        connection(&database)
            .unwrap()
            .execute(
                "DELETE FROM attempt_driver_claims WHERE run_id=?1",
                [callback.run_id.to_string()],
            )
            .unwrap();
        assert!(
            store
                .resolve(
                    callback.principal_id,
                    callback.project_id,
                    &key,
                    callback.id,
                    2,
                    7,
                    &callback.schema_digest,
                    McpCallbackAction::Decline,
                    Vec::new(),
                    [1; 32],
                    &callback,
                )
                .unwrap()
                .1
        );
        assert!(matches!(
            store.resolve(
                callback.principal_id,
                callback.project_id,
                &key,
                callback.id,
                2,
                7,
                &callback.schema_digest,
                McpCallbackAction::Decline,
                Vec::new(),
                [2; 32],
                &callback,
            ),
            Err(McpCallbackError::IdempotencyConflict)
        ));
        let replay_connection = connection(&database).unwrap();
        let response = replay_connection
            .query_row(
                "SELECT response FROM mcp_callback_idempotency
                 WHERE principal_id=?1 AND callback_id=?2 AND idempotency_key=?3",
                params![
                    callback.principal_id.to_string(),
                    callback.id.to_string(),
                    key.as_str(),
                ],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .unwrap();
        let mut expired_response: ResolutionResponse = serde_json::from_slice(&response).unwrap();
        expired_response.callback.artifact_expires_at =
            UtcDateTime::parse("2000-01-01T00:00:00Z").unwrap();
        replay_connection
            .execute(
                "UPDATE mcp_callback_idempotency SET response=?4
                 WHERE principal_id=?1 AND callback_id=?2 AND idempotency_key=?3",
                params![
                    callback.principal_id.to_string(),
                    callback.id.to_string(),
                    key.as_str(),
                    serde_json::to_vec(&expired_response).unwrap(),
                ],
            )
            .unwrap();
        assert!(matches!(
            store.resolve(
                callback.principal_id,
                callback.project_id,
                &key,
                callback.id,
                2,
                7,
                &callback.schema_digest,
                McpCallbackAction::Decline,
                Vec::new(),
                [1; 32],
                &callback,
            ),
            Err(McpCallbackError::Expired)
        ));
        let indexed = connection(&database)
            .unwrap()
            .query_row(
                "SELECT count(*), max(commit_position) FROM event_projection_index
                 WHERE project_id=?1 AND run_id=?2",
                params![callback.project_id.to_string(), callback.run_id.to_string()],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
            )
            .unwrap();
        assert_eq!(indexed, (3, positions[0].get()));
        assert!(matches!(
            store.resolve(
                callback.principal_id,
                callback.project_id,
                &key,
                callback.id,
                2,
                7,
                &callback.schema_digest,
                McpCallbackAction::Decline,
                Vec::new(),
                [2; 32],
                &callback,
            ),
            Err(McpCallbackError::IdempotencyConflict)
        ));

        let raced = url_fixture(2);
        append
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id: raced.run_id,
                attempt_id: raced.attempt_id,
                principal_id: raced.principal_id,
                fence: raced.fence,
                lease_version: raced.claim_generation,
                expires_at_unix_micros: i64::MAX,
            })
            .unwrap();
        store.request(raced.clone()).unwrap();
        let store = Arc::new(store);
        let workers = (0..2)
            .map(|index| {
                let store = Arc::clone(&store);
                let raced = raced.clone();
                thread::spawn(move || {
                    let key = IdempotencyKey::parse(&format!("race-{index}")).unwrap();
                    store.reserve_resolution(
                        raced.principal_id,
                        raced.project_id,
                        raced.id,
                        &key,
                        [index as u8; 32],
                        2,
                        7,
                        &raced.schema_digest,
                    )?;
                    store.resolve(
                        raced.principal_id,
                        raced.project_id,
                        &key,
                        raced.id,
                        2,
                        7,
                        &raced.schema_digest,
                        McpCallbackAction::Cancel,
                        Vec::new(),
                        [index as u8; 32],
                        &raced,
                    )
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .filter(Result::is_ok)
                .count(),
            1
        );
        assert_eq!(
            store.get(raced.id).unwrap().state,
            McpCallbackState::Resolved
        );

        let restart = url_fixture(3);
        append
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id: restart.run_id,
                attempt_id: restart.attempt_id,
                principal_id: restart.principal_id,
                fence: restart.fence,
                lease_version: restart.claim_generation,
                expires_at_unix_micros: i64::MAX,
            })
            .unwrap();
        store.request(restart.clone()).unwrap();

        let delivery = url_fixture(4);
        append
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id: delivery.run_id,
                attempt_id: delivery.attempt_id,
                principal_id: delivery.principal_id,
                fence: delivery.fence,
                lease_version: delivery.claim_generation,
                expires_at_unix_micros: i64::MAX,
            })
            .unwrap();
        store.request(delivery.clone()).unwrap();
        let delivery_key = IdempotencyKey::parse("delivery-window").unwrap();
        store
            .reserve_resolution(
                delivery.principal_id,
                delivery.project_id,
                delivery.id,
                &delivery_key,
                [4; 32],
                2,
                7,
                &delivery.schema_digest,
            )
            .unwrap();
        store
            .resolve(
                delivery.principal_id,
                delivery.project_id,
                &delivery_key,
                delivery.id,
                2,
                7,
                &delivery.schema_digest,
                McpCallbackAction::Decline,
                Vec::new(),
                [4; 32],
                &delivery,
            )
            .unwrap();
        store.prepare_response(delivery.id).unwrap();

        let accepted = url_fixture(5);
        append
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id: accepted.run_id,
                attempt_id: accepted.attempt_id,
                principal_id: accepted.principal_id,
                fence: accepted.fence,
                lease_version: accepted.claim_generation,
                expires_at_unix_micros: i64::MAX,
            })
            .unwrap();
        store.request(accepted.clone()).unwrap();
        let accepted_key = IdempotencyKey::parse("accepted-url-proof").unwrap();
        store
            .reserve_resolution(
                accepted.principal_id,
                accepted.project_id,
                accepted.id,
                &accepted_key,
                [5; 32],
                2,
                7,
                &accepted.schema_digest,
            )
            .unwrap();
        let accepted_resolution = store
            .resolve(
                accepted.principal_id,
                accepted.project_id,
                &accepted_key,
                accepted.id,
                2,
                7,
                &accepted.schema_digest,
                McpCallbackAction::Accept,
                Vec::new(),
                [5; 32],
                &accepted,
            )
            .unwrap()
            .0;
        assert_eq!(accepted_resolution.state, McpCallbackState::Resolved);
        assert_eq!(
            store.prepare_response(accepted.id).unwrap().state,
            McpCallbackState::ResponsePrepared
        );
        drop(store);
        let restarted = McpCallbackStore::open(&database).unwrap();
        assert_eq!(restarted.reconcile_startup().unwrap(), 6);
        let interrupted = restarted.get(restart.id).unwrap();
        assert_eq!(interrupted.state, McpCallbackState::Interrupted);
        assert_eq!(interrupted.mode, McpCallbackMode::Url);
        assert_eq!(
            interrupted.terminal_error.as_deref(),
            Some("outcome_unknown")
        );
        let delivery_unknown = restarted.get(delivery.id).unwrap();
        assert_eq!(delivery_unknown.state, McpCallbackState::DeliveryUnknown);
        assert_eq!(delivery_unknown.mode, McpCallbackMode::Url);
        assert_eq!(delivery_unknown.action, Some(McpCallbackAction::Decline));
        assert_eq!(
            delivery_unknown.terminal_error.as_deref(),
            Some("delivery_unknown")
        );
        let accepted_unknown = restarted.get(accepted.id).unwrap();
        assert_eq!(accepted_unknown.state, McpCallbackState::DeliveryUnknown);
        assert_eq!(accepted_unknown.action, Some(McpCallbackAction::Accept));
        assert_eq!(
            connection(&database)
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM mcp_callback_resolution_reservations",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            0
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn response_artifact_is_private_until_terminal_delivery() {
        let root = std::env::temp_dir().join(format!(
            "kit-mcp-terminal-artifact-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let database = root.join("service.sqlite");
        let authority = ControlPlaneAuthority::for_test();
        let mut append = SqliteStore::open(&database, &authority).unwrap();
        drop(ProjectionStore::open(&database).unwrap());
        let callback = fixture(41);
        append
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id: callback.run_id,
                attempt_id: callback.attempt_id,
                principal_id: callback.principal_id,
                fence: callback.fence,
                lease_version: callback.claim_generation,
                expires_at_unix_micros: i64::MAX,
            })
            .unwrap();
        let store = McpCallbackStore::open(&database).unwrap();
        store.request(callback.clone()).unwrap();
        let artifacts = ArtifactStore::open(root.join("artifacts")).unwrap();
        let binding = ArtifactEnvelopeBinding {
            principal: callback.principal_id.to_string(),
            project: callback.project_id.to_string(),
            run: callback.run_id.to_string(),
            purpose: "mcp_callback_content".to_owned(),
            invocation_id: None,
            callback_id: Some(callback.id.to_string()),
        };
        let reference = ArtifactReference::derive(b"terminal-event-test", b"response");
        artifacts
            .stage_publication(
                &binding.seal(br#"{"name":"Ada"}"#).unwrap(),
                ArtifactMetadata::new(
                    "application/vnd.kit.artifact-envelope",
                    ArtifactClass::File,
                    callback.principal_id.to_string(),
                    callback.project_id.to_string(),
                    ArtifactRetention::UntilUnixMicros(callback.artifact_expires_at.unix_micros()),
                    1,
                )
                .unwrap(),
                reference,
            )
            .unwrap();
        let key = IdempotencyKey::parse("terminal-artifact").unwrap();
        store
            .reserve_resolution(
                callback.principal_id,
                callback.project_id,
                callback.id,
                &key,
                [9; 32],
                2,
                7,
                &callback.schema_digest,
            )
            .unwrap();
        store
            .resolve(
                callback.principal_id,
                callback.project_id,
                &key,
                callback.id,
                2,
                7,
                &callback.schema_digest,
                McpCallbackAction::Accept,
                vec![McpCallbackArtifactRef::parse(&reference.to_string()).unwrap()],
                [9; 32],
                &callback,
            )
            .unwrap();
        store.prepare_response(callback.id).unwrap();
        assert_eq!(store.reconcile_artifact_publications().unwrap(), 0);
        let database_connection = connection(&database).unwrap();
        assert_eq!(
            database_connection
                .query_row(
                    "SELECT count(*) FROM artifact_publication_journal WHERE subject_id=?1",
                    [callback.id.to_string()],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            1
        );
        let before_terminal = database_connection
            .prepare("SELECT payload,artifacts FROM events WHERE stream=?1 ORDER BY sequence")
            .unwrap()
            .query_map([callback.id.to_string()], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(before_terminal.len(), 4);
        for (payload, event_artifacts) in before_terminal {
            assert!(
                !String::from_utf8(payload)
                    .unwrap()
                    .contains(&reference.to_string())
            );
            assert_eq!(event_artifacts, b"[]");
        }
        store.deliver(callback.id).unwrap();
        let (terminal_payload, terminal_artifacts) = database_connection
            .query_row(
                "SELECT payload,artifacts FROM events WHERE stream=?1 AND sequence=5",
                [callback.id.to_string()],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .unwrap();
        assert!(
            String::from_utf8(terminal_payload)
                .unwrap()
                .contains(&reference.to_string())
        );
        assert!(
            String::from_utf8(terminal_artifacts)
                .unwrap()
                .contains(&reference.to_string())
        );
        assert_eq!(
            database_connection
                .query_row(
                    "SELECT count(*) FROM artifact_publication_journal WHERE subject_id=?1",
                    [callback.id.to_string()],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            0
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_recovery_covers_uncommitted_committed_and_promoted_boundaries() {
        let root = std::env::temp_dir().join(format!(
            "kit-mcp-publication-recovery-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let database = root.join("service.sqlite");
        let authority = ControlPlaneAuthority::for_test();
        drop(SqliteStore::open(&database, &authority).unwrap());
        drop(ProjectionStore::open(&database).unwrap());
        let store = McpCallbackStore::open(&database).unwrap();
        let artifacts = ArtifactStore::open(root.join("artifacts")).unwrap();
        let callback = fixture(42);
        let binding = ArtifactEnvelopeBinding {
            principal: callback.principal_id.to_string(),
            project: callback.project_id.to_string(),
            run: callback.run_id.to_string(),
            purpose: "mcp_callback_content".to_owned(),
            invocation_id: None,
            callback_id: Some(callback.id.to_string()),
        };
        let bytes = binding.seal(br#"{"name":"Ada"}"#).unwrap();
        let metadata = ArtifactMetadata::new(
            "application/vnd.kit.artifact-envelope",
            ArtifactClass::File,
            callback.principal_id.to_string(),
            callback.project_id.to_string(),
            ArtifactRetention::UntilUnixMicros(callback.artifact_expires_at.unix_micros()),
            1,
        )
        .unwrap();

        let uncommitted_reference = ArtifactReference::derive(b"test-publication", b"uncommitted");
        artifacts
            .stage_publication(&bytes, metadata.clone(), uncommitted_reference)
            .unwrap();
        assert_eq!(store.reconcile_artifact_publications().unwrap(), 0);
        assert!(artifacts.staged_publication(uncommitted_reference).is_err());
        assert!(artifacts.open_reference(uncommitted_reference).is_err());

        let promoted_before_resolution =
            ArtifactReference::derive(b"test-publication", b"promoted-before-resolution");
        let interrupted = artifacts
            .stage_publication(&bytes, metadata.clone(), promoted_before_resolution)
            .unwrap();
        artifacts.promote_publication(&interrupted).unwrap();
        insert_publication_journal(&database, &callback, &interrupted);
        connection(&database)
            .unwrap()
            .execute(
                "UPDATE artifact_publication_journal SET state='promoted'
                 WHERE artifact_reference=?1",
                [promoted_before_resolution.to_string()],
            )
            .unwrap();
        assert_eq!(store.reconcile_artifact_publications().unwrap(), 1);
        assert!(
            artifacts
                .open_reference(promoted_before_resolution)
                .is_err()
        );

        let committed_reference = ArtifactReference::derive(b"test-publication", b"committed");
        let committed = artifacts
            .stage_publication(&bytes, metadata.clone(), committed_reference)
            .unwrap();
        install_resolved_projection(&database, &callback, committed_reference);
        insert_publication_journal(&database, &callback, &committed);
        assert_eq!(store.reconcile_artifact_publications().unwrap(), 0);
        assert_eq!(
            artifacts
                .open_reference(committed_reference)
                .unwrap()
                .digest(),
            committed.digest()
        );

        let promoted_reference = ArtifactReference::derive(b"test-publication", b"promoted");
        let promoted = artifacts
            .stage_publication(&bytes, metadata, promoted_reference)
            .unwrap();
        artifacts.promote_publication(&promoted).unwrap();
        install_resolved_projection(&database, &callback, promoted_reference);
        insert_publication_journal(&database, &callback, &promoted);
        assert_eq!(store.reconcile_artifact_publications().unwrap(), 1);
        assert_eq!(
            artifacts
                .open_reference(promoted_reference)
                .unwrap()
                .digest(),
            promoted.digest()
        );
        let missing_reference = ArtifactReference::derive(b"test-publication", b"missing");
        install_resolved_projection(&database, &callback, missing_reference);
        connection(&database)
            .unwrap()
            .execute(
                "INSERT INTO artifact_publication_journal
                 (artifact_reference,artifact_digest,purpose,subject_id,principal_id,project_id,run_id,
                  state,created_at_unix_micros)
                 VALUES (?1,?2,'mcp_callback_content',?3,?4,?5,?6,'resolution_committed',1)",
                params![
                    missing_reference.to_string(),
                    promoted.digest().to_string(),
                    callback.id.to_string(),
                    callback.principal_id.to_string(),
                    callback.project_id.to_string(),
                    callback.run_id.to_string(),
                ],
            )
            .unwrap();
        assert_eq!(store.reconcile_artifact_publications().unwrap(), 1);
        assert_eq!(
            store.get(callback.id).unwrap().state,
            McpCallbackState::Interrupted
        );
        assert_eq!(
            connection(&database)
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM artifact_publication_journal",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            0
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn run_erasure_scrubs_only_matching_callback_data_and_keeps_backup_tenants() {
        let root = std::env::temp_dir().join(format!(
            "kit-mcp-callback-erasure-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let database = root.join("service.sqlite");
        let authority = ControlPlaneAuthority::for_test();
        let mut append = SqliteStore::open(&database, &authority).unwrap();
        drop(ProjectionStore::open(&database).unwrap());
        let mut callback = fixture(43);
        callback.request = serde_json::json!({"message":"privacy-canary"});
        callback.schema = serde_json::json!({"type":"object","title":"privacy-schema-canary"});
        append
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id: callback.run_id,
                attempt_id: callback.attempt_id,
                principal_id: callback.principal_id,
                fence: callback.fence,
                lease_version: callback.claim_generation,
                expires_at_unix_micros: i64::MAX,
            })
            .unwrap();
        let store = McpCallbackStore::open(&database).unwrap();
        store.request(callback.clone()).unwrap();
        let artifacts = ArtifactStore::open(root.join("artifacts")).unwrap();
        let binding = ArtifactEnvelopeBinding {
            principal: callback.principal_id.to_string(),
            project: callback.project_id.to_string(),
            run: callback.run_id.to_string(),
            purpose: "mcp_callback_content".to_owned(),
            invocation_id: None,
            callback_id: Some(callback.id.to_string()),
        };
        let artifact_bytes = binding.seal(b"privacy-artifact-canary").unwrap();
        let metadata = ArtifactMetadata::new(
            "application/vnd.kit.artifact-envelope",
            ArtifactClass::File,
            callback.principal_id.to_string(),
            callback.project_id.to_string(),
            ArtifactRetention::UntilUnixMicros(callback.artifact_expires_at.unix_micros()),
            1,
        )
        .unwrap();
        let staged_reference = ArtifactReference::derive(b"erasure-test", b"staged");
        artifacts
            .stage_publication(&artifact_bytes, metadata.clone(), staged_reference)
            .unwrap();
        let unrelated_reference = ArtifactReference::derive(b"erasure-test", b"unrelated");
        let unrelated = ArtifactEnvelopeBinding {
            principal: callback.principal_id.to_string(),
            project: callback.project_id.to_string(),
            run: callback.run_id.to_string(),
            purpose: "mcp_invocation_result".to_owned(),
            invocation_id: Some("tool_call_unrelated".to_owned()),
            callback_id: None,
        }
        .seal(b"unrelated-tenant-artifact")
        .unwrap();
        artifacts
            .stage_publication(&unrelated, metadata.clone(), unrelated_reference)
            .unwrap();
        connection(&database)
            .unwrap()
            .execute(
                "INSERT INTO artifact_publication_journal
                 (artifact_reference,artifact_digest,purpose,subject_id,principal_id,project_id,run_id)
                 VALUES (?1,?2,'mcp_callback_content',?3,?4,?5,?6)",
                params![
                    unrelated_reference.to_string(),
                    ArtifactDigest::digest(&unrelated).to_string(),
                    callback.id.to_string(),
                    callback.principal_id.to_string(),
                    callback.project_id.to_string(),
                    callback.run_id.to_string(),
                ],
            )
            .unwrap();
        let published_reference = ArtifactReference::derive(b"erasure-test", b"published");
        let published = artifacts
            .stage_publication(&artifact_bytes, metadata, published_reference)
            .unwrap();
        let digest = artifacts.promote_publication(&published).unwrap().digest();
        let backup_root = root.parent().unwrap().join(format!(
            "{}.backups",
            root.file_name().unwrap().to_string_lossy()
        ));
        let _ = fs::remove_dir_all(&backup_root);
        fs::create_dir(&backup_root).unwrap();
        fs::write(
            backup_root.join("privacy-backup-canary"),
            b"privacy-artifact-canary",
        )
        .unwrap();
        store
            .settle(
                callback.id,
                McpCallbackState::Interrupted,
                Some("privacy-terminal-canary".to_owned()),
            )
            .unwrap();
        assert_eq!(
            store
                .erase_run(
                    ProjectId::from_stable_bytes(b"other-project"),
                    callback.run_id
                )
                .unwrap(),
            0
        );
        assert!(store.get(callback.id).is_ok());
        connection(&database)
            .unwrap()
            .execute(
                "UPDATE event_projection_index SET erased=1, thread_id=NULL, run_id=NULL
                 WHERE run_id=?1",
                [callback.run_id.to_string()],
            )
            .unwrap();
        assert_eq!(
            store
                .erase_run(callback.project_id, callback.run_id)
                .unwrap(),
            1
        );
        assert!(matches!(
            store.get(callback.id),
            Err(McpCallbackError::NotFound)
        ));
        assert_eq!(
            fs::read(backup_root.join("privacy-backup-canary")).unwrap(),
            b"privacy-artifact-canary"
        );
        assert!(artifacts.staged_publication(staged_reference).is_err());
        assert!(artifacts.staged_publication(unrelated_reference).is_ok());
        assert!(artifacts.open_reference(published_reference).is_err());
        assert!(artifacts.open_bytes(digest).is_err());

        let connection = connection(&database).unwrap();
        for table in [
            "mcp_callback_projection",
            "mcp_callback_scope_index",
            "mcp_callback_events",
            "mcp_callback_idempotency",
            "mcp_callback_resolution_reservations",
            "artifact_publication_journal",
            "mcp_callback_workspace_revisions",
        ] {
            let count: usize = connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} retained callback data");
        }
        let event_bytes = connection
            .prepare("SELECT payload, artifacts FROM events WHERE stream=?1")
            .unwrap()
            .query_map([callback.id.to_string()], |row| {
                let mut bytes = row.get::<_, Vec<u8>>(0)?;
                bytes.extend(row.get::<_, Vec<u8>>(1)?);
                Ok(bytes)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .concat();
        let event_text = String::from_utf8(event_bytes).unwrap();
        assert!(!event_text.contains("privacy-canary"));
        assert!(!event_text.contains("privacy-schema-canary"));
        assert!(!event_text.contains("privacy-terminal-canary"));
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM deletion_tombstones WHERE object_kind='mcp_callback'",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transcript_erasure_uses_callback_scope_after_event_index_compaction() {
        let root = std::env::temp_dir().join(format!(
            "kit-mcp-callback-transcript-erasure-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let database = root.join("service.sqlite");
        let authority = ControlPlaneAuthority::for_test();
        let mut append = SqliteStore::open(&database, &authority).unwrap();
        drop(ProjectionStore::open(&database).unwrap());
        let callback = fixture(44);
        append
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id: callback.run_id,
                attempt_id: callback.attempt_id,
                principal_id: callback.principal_id,
                fence: callback.fence,
                lease_version: callback.claim_generation,
                expires_at_unix_micros: i64::MAX,
            })
            .unwrap();
        let store = McpCallbackStore::open(&database).unwrap();
        store.request(callback.clone()).unwrap();
        let thread_id = "thread_compacted";
        let mut connection = connection(&database).unwrap();
        connection
            .execute(
                "UPDATE event_projection_index SET thread_id=?2 WHERE run_id=?1",
                params![callback.run_id.to_string(), thread_id],
            )
            .unwrap();
        store
            .settle(
                callback.id,
                McpCallbackState::Interrupted,
                Some("test".to_owned()),
            )
            .unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT thread_id FROM mcp_callback_scope_index WHERE callback_id=?1",
                    [callback.id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            thread_id
        );
        connection
            .execute(
                "UPDATE event_projection_index SET erased=1, thread_id=NULL, run_id=NULL
                 WHERE run_id=?1",
                [callback.run_id.to_string()],
            )
            .unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert_eq!(
            scrub_callback_transcript(&transaction, thread_id, 1).unwrap(),
            1
        );
        transaction.commit().unwrap();
        assert!(matches!(
            store.get(callback.id),
            Err(McpCallbackError::NotFound)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    fn insert_publication_journal(
        database: &Path,
        callback: &McpCallbackProjection,
        publication: &ArtifactPublication,
    ) {
        connection(database)
            .unwrap()
            .execute(
                "INSERT INTO artifact_publication_journal
                 (artifact_reference,artifact_digest,purpose,subject_id,principal_id,project_id,run_id)
                 VALUES (?1,?2,'mcp_callback_content',?3,?4,?5,?6)",
                params![
                    publication.reference().to_string(),
                    publication.digest().to_string(),
                    callback.id.to_string(),
                    callback.principal_id.to_string(),
                    callback.project_id.to_string(),
                    callback.run_id.to_string(),
                ],
            )
            .unwrap();
    }

    fn install_resolved_projection(
        database: &Path,
        callback: &McpCallbackProjection,
        reference: ArtifactReference,
    ) {
        let mut resolved = callback.clone();
        resolved.state = McpCallbackState::Resolved;
        resolved.version = 3;
        resolved.resolver_actor = Some(callback.principal_id);
        resolved.action = Some(McpCallbackAction::Accept);
        resolved.artifact_refs =
            vec![McpCallbackArtifactRef::parse(&reference.to_string()).unwrap()];
        let connection = connection(database).unwrap();
        connection
            .execute(
                "INSERT INTO mcp_callback_projection
                 (callback_id,project_id,principal_id,run_id,workspace_id,state,version,projection)
                 VALUES (?1,?2,?3,?4,?5,'resolved',3,?6)
                 ON CONFLICT(callback_id) DO UPDATE SET state='resolved',version=3,projection=excluded.projection",
                params![
                    resolved.id.to_string(),
                    resolved.project_id.to_string(),
                    resolved.principal_id.to_string(),
                    resolved.run_id.to_string(),
                    resolved.workspace_id.to_string(),
                    serde_json::to_vec(&resolved).unwrap(),
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO stream_heads (stream,version) VALUES (?1,3)
                 ON CONFLICT(stream) DO UPDATE SET version=3",
                [resolved.id.to_string()],
            )
            .unwrap();
    }

    fn fixture(sequence: u8) -> McpCallbackProjection {
        let stable = [sequence; 16];
        let principal_id = PrincipalId::from_stable_bytes(&[b'p', sequence]);
        let project_id = ProjectId::from_stable_bytes(&[b'j', sequence]);
        let run_id = RunId::from_stable_bytes(&[b'r', sequence]);
        let attempt_id = AttemptId::from_stable_bytes(&[b'a', sequence]);
        McpCallbackProjection {
            id: McpCallbackId::from_stable_bytes(&stable),
            server_id: "server".to_owned(),
            kind: McpCallbackKind::Elicitation,
            mode: McpCallbackMode::Form,
            principal_id,
            project_id,
            run_id,
            attempt_id,
            fence: FencingToken::new(3),
            claim_generation: 5,
            workspace_id: WorkspaceId::from_stable_bytes(&[b'w', sequence]),
            workspace_revision: "revision".to_owned(),
            request_id: sequence.to_string(),
            request: serde_json::json!({"message":"name"}),
            schema: serde_json::json!({"type":"object","properties":{"name":{"type":"string"}}}),
            request_digest: format!("sha256:{}", "1".repeat(64)),
            schema_digest: format!("sha256:{}", "2".repeat(64)),
            challenge_generation: 7,
            operation_sequence: u64::from(sequence),
            expires_at: UtcDateTime::parse("2099-01-01T00:00:00Z").unwrap(),
            artifact_expires_at: UtcDateTime::parse("2100-01-01T00:00:00Z").unwrap(),
            max_response_bytes: 1024,
            max_content_bytes: 900,
            secret_policy_id: "authorized-secrets-v1".to_owned(),
            url_binding: None,
            state: McpCallbackState::Requested,
            version: 1,
            resolver_actor: None,
            action: None,
            artifact_refs: Vec::new(),
            terminal_error: None,
        }
    }

    fn url_fixture(sequence: u8) -> McpCallbackProjection {
        let mut callback = fixture(sequence);
        let response_digest = format!("sha256:{}", format!("{:x}", sequence).repeat(64));
        let url = format!("https://auth.example/complete/{sequence}");
        callback.mode = McpCallbackMode::Url;
        callback.request_id = format!("invocation-{sequence}");
        callback.request = serde_json::json!({
            "error_code":-32042,
            "error_response_digest":response_digest,
            "message":"authenticate",
            "url":url,
            "elicitation_id":format!("challenge-{sequence}")
        });
        callback.schema = serde_json::json!({});
        callback.request_digest = crate::capabilities::kernel::identity::Digest::of(
            crate::capabilities::kernel::identity::DigestAlgorithm::Sha256,
            &serde_json::to_vec(&callback.request).unwrap(),
        )
        .to_string();
        callback.operation_sequence = callback.challenge_generation;
        callback.url_binding = Some(McpUrlCallbackBinding {
            invocation_id: callback.request_id.clone(),
            idempotency_digest: format!("sha256:{}", "a".repeat(64)),
            server_id: callback.server_id.clone(),
            generation: callback.challenge_generation,
            operation: "tools/call".to_owned(),
            invocation_request_digest: format!("sha256:{}", "b".repeat(64)),
            error_response_digest: response_digest.clone(),
            url_digest: crate::capabilities::kernel::identity::Digest::of(
                crate::capabilities::kernel::identity::DigestAlgorithm::Sha256,
                url.as_bytes(),
            )
            .to_string(),
            original_effect: crate::capabilities::kernel::grant::EffectClass::WorkspaceRead,
            grant_digest: format!("sha256:{}", "c".repeat(64)),
            accept_destination: McpUrlDestination::from_url(&url).unwrap(),
            retry_safety: "idempotent".to_owned(),
            undispatched_proof: McpUndispatchedProof::from_terminal_url_elicitation(
                response_digest,
            ),
        });
        callback
    }
}
