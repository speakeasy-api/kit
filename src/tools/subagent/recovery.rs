//! Parent-owned durable identity, deliberately separate from immutable forks.
use super::*;
use session::{ChildLifecycle, DurableChild, SessionObserver};

impl Subagents {
    pub(crate) fn with_observer(
        mut self,
        observer: SessionObserver,
        restored: Vec<DurableChild>,
    ) -> Result<Self, String> {
        if self.observer.is_some()
            || Arc::strong_count(&self.sessions) != 1
            || !self
                .sessions
                .lock()
                .map_err(|_| "subagent registry lock was poisoned")?
                .is_empty()
        {
            return Err("durable recovery requires a fresh unshared subagent manager".into());
        }
        // Build privately: malformed entries cannot expose a partial registry.
        let mut sessions = HashMap::new();
        for record in restored {
            if record.lifecycle != ChildLifecycle::Idle {
                continue;
            }
            let updates = record
                .updates
                .clone()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|error| format!("invalid durable child updates: {error}"))?;
            let state = State {
                name: record.name.clone(),
                status: SubagentStatus::Idle,
                task: record.task.clone(),
                generation: record.generation,
                handle_generation: record.handle_generation,
                outcome: Some(GenerationOutcome::Success),
                created_at_unix_ms: record.created_at_unix_ms,
                generation_started_at_unix_ms: record.created_at_unix_ms,
                generation_finished_at_unix_ms: None,
                output: record.output.clone(),
                updates,
                harness: record.harness.clone(),
                vendor: self.config.harnesses.vendor(&record.harness),
                model: record.model.clone(),
                kit: self.config.harnesses.is_kit(&record.harness),
                root: record.root.clone(),
                child: None,
                recovery: Some(record.clone()),
                forking: None,
                permit: None,
            };
            if sessions
                .insert(
                    record.id,
                    SessionEntry {
                        name: record.name,
                        state: Arc::new(AsyncMutex::new(state)),
                    },
                )
                .is_some()
            {
                return Err("duplicate durable child handle".into());
            }
        }
        self.sessions = Arc::new(Mutex::new(sessions));
        self.observer = Some(observer);
        Ok(self)
    }

    pub(super) fn remember_child(
        &self,
        id: &str,
        state: &mut State,
        child: &ChildSession,
        depth: usize,
    ) {
        if self.observer.is_none() {
            return;
        }
        state.recovery = Some(DurableChild {
            id: id.into(),
            acp_session_id: child.session_id(),
            name: state.name.clone(),
            task: state.task.clone(),
            generation: state.generation,
            handle_generation: state.handle_generation,
            output: state.output.clone(),
            updates: None,
            harness: state.harness.clone(),
            model: state.model.clone(),
            root: state.root.clone(),
            depth,
            lifecycle: ChildLifecycle::Interrupted,
            created_at_unix_ms: state.created_at_unix_ms,
        });
    }

    // Lock order: per-child state -> transcript writer. No writer calls back into
    // the registry, and no guard spans process startup, an RPC, or an await.
    pub(super) fn persist_state(
        &self,
        state: &State,
        lifecycle: ChildLifecycle,
    ) -> Result<(), ChildError> {
        let (Some(observer), Some(record)) = (&self.observer, &state.recovery) else {
            return Ok(());
        };
        let mut record = record.clone();
        record.lifecycle = lifecycle;
        record.task.clone_from(&state.task);
        record.generation = state.generation;
        record.handle_generation = state.handle_generation;
        record.output.clone_from(&state.output);
        record.updates = state
            .updates
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| ChildError::Failed(error.to_string()))?;
        observer.persist_child(&record).map_err(ChildError::Failed)
    }

    pub(super) async fn reconnect(
        &self,
        prior: &SubagentValue,
        state: &Arc<AsyncMutex<State>>,
        cancellation: &TurnCancellation,
    ) -> Result<(), ChildError> {
        let mut locked =
            match select(Box::pin(state.lock()), Box::pin(cancellation.cancelled())).await {
                Either::Left((locked, _)) => locked,
                Either::Right(((), pending_lock)) => {
                    drop(pending_lock);
                    return Err(ChildError::Cancelled);
                }
            };
        let id = prior.id.as_str();
        self.check_generation(prior, locked.handle_generation)?;
        self.check_ready(&locked)?;
        if locked.child.is_some() {
            return Ok(());
        }
        let record = locked
            .recovery
            .clone()
            .ok_or_else(|| ChildError::Failed("subagent session has no durable identity".into()))?;
        if !self.config.harnesses.contains(&record.harness) {
            return Err(ChildError::Failed(format!(
                "durable child harness {:?} is no longer configured",
                record.harness
            )));
        }
        self.check_depth(record.depth.saturating_sub(1))?;
        let model = record
            .model
            .as_deref()
            .map(|model| self.config.harnesses.resolve_model(&record.harness, model))
            .transpose()
            .map_err(ChildError::Failed)?;
        // Capacity is private to this attempt until the child is installed.
        let permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| ChildError::Failed("maximum live subagent count reached".into()))?;
        locked.status = SubagentStatus::Starting;
        let config = self
            .config
            .clone()
            .with_root(record.root.clone())
            .with_parent_context(id.into(), locked.name.clone());
        drop(locked);
        let manager = self.clone();
        let state = Arc::clone(state);
        let cancellation = cancellation.clone();
        let id = id.to_owned();
        // The task owns completion/rollback even if the caller drops its future.
        // Replay feeds inspection only; it never replaces public last-turn output.
        tokio::spawn(async move {
            let result = ChildSession::start_with_output(
                config,
                record.harness,
                Some((record.acp_session_id, true)),
                model,
                record.depth,
                cancellation,
            )
            .await;
            let mut locked = state.lock().await;
            match result {
                Ok((child, replay)) => {
                    if let Err(error) = manager.check_active(&locked) {
                        drop(locked);
                        return Err(manager
                            .cleanup_uninstalled_child(child, Some(permit), error)
                            .await);
                    }
                    locked.child = Some(child.clone());
                    locked.permit = Some(permit);
                    locked.status = SubagentStatus::Idle;
                    let generation = locked.generation;
                    drop(locked);
                    manager.transcripts.start(&id, generation);
                    if let Ok(transcript) = manager.transcripts.get(&id, generation) {
                        transcript.replay(&id, generation, &replay);
                    }
                    manager.monitor_child_exit(id, &state, &child);
                    Ok(())
                }
                Err(error) => {
                    if locked.status != SubagentStatus::Removed {
                        locked.status = SubagentStatus::Idle;
                    }
                    Err(error)
                }
            }
        })
        .await
        .map_err(|error| ChildError::Failed(format!("child reconnect task failed: {error}")))?
    }
}
