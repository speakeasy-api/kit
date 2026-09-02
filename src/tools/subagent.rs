use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use agentkit_core::{ToolOutput, ToolResultPart, TurnCancellation};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore, oneshot};

const MAX_LIVE_SUBAGENTS: usize = 120;
const MAX_DISPLAY_NAME_LEN: usize = 32;

fn normalize_display_name(candidate: &str) -> Option<String> {
    let name = candidate.trim();
    if (1..=MAX_DISPLAY_NAME_LEN).contains(&name.len())
        && name.is_ascii()
        && name.bytes().all(|byte| !byte.is_ascii_control())
    {
        Some(name.to_string())
    } else {
        None
    }
}

fn allocate_display_name(
    used: &mut HashSet<String>,
    preferred: Option<&str>,
) -> Result<String, ChildError> {
    if let Some(name) = preferred.and_then(normalize_display_name) {
        if reserve_name(used, &name) {
            return Ok(name);
        }
        for number in 2..=MAX_LIVE_SUBAGENTS + 1 {
            let suffix = format!(" {number}");
            let Some(max_base_len) = MAX_DISPLAY_NAME_LEN.checked_sub(suffix.len()) else {
                break;
            };
            let base = name[..name.len().min(max_base_len)].trim_end();
            let candidate = format!("{base}{suffix}");
            if reserve_name(used, &candidate) {
                return Ok(candidate);
            }
        }
    }

    // At most MAX_LIVE_SUBAGENTS names can be reserved, so one of these
    // MAX_LIVE_SUBAGENTS + 1 generated names must be available. Keep the
    // search bounded and report invariant violations instead of looping.
    for number in 1..=MAX_LIVE_SUBAGENTS + 1 {
        let name = format!("Agent {number}");
        if reserve_name(used, &name) {
            return Ok(name);
        }
    }
    Err(ChildError::Failed(
        "could not allocate a unique subagent display name".into(),
    ))
}

fn reserve_name(used: &mut HashSet<String>, name: &str) -> bool {
    used.insert(name.to_ascii_lowercase())
}

fn child_error_is_terminal(error: &ChildError, child: &ChildSession) -> bool {
    match error {
        ChildError::TerminalCancelled | ChildError::TerminalFailed(_) => true,
        ChildError::Cancelled | ChildError::Failed(_) => child.is_closed(),
    }
}

fn task_summary(prompt: &str) -> String {
    let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return "Untitled task".into();
    }
    if normalized.chars().count() <= 96 {
        normalized
    } else {
        format!("{}…", normalized.chars().take(95).collect::<String>())
    }
}

use crate::{
    acp_child::{ChildConfig, ChildError, ChildOutput, ChildSession},
    events::{self, GenerationOutcome, SubagentStatus},
    session,
};

type EventSink = Arc<dyn Fn(&events::RuntimeEvent) -> Result<(), ()> + Send + Sync>;

#[derive(Clone)]
pub struct Subagents {
    config: ChildConfig,
    max_depth: usize,
    sessions: Arc<Mutex<HashMap<String, SessionEntry>>>,
    capacity: Arc<Semaphore>,
    event_sink: EventSink,
}

struct SessionEntry {
    name: String,
    state: Arc<AsyncMutex<State>>,
}

struct State {
    name: String,
    status: SubagentStatus,
    task: String,
    generation: u64,
    handle_generation: u64,
    outcome: Option<GenerationOutcome>,
    created_at_unix_ms: u64,
    generation_started_at_unix_ms: u64,
    generation_finished_at_unix_ms: Option<u64>,
    output: Value,
    updates: Option<SubagentUpdates>,
    harness: String,
    model: Option<String>,
    kit: bool,
    root: PathBuf,
    child: Option<ChildSession>,
    forking: Option<String>,
    permit: Option<OwnedSemaphorePermit>,
}

impl State {
    fn runtime_event(&self, id: String) -> events::RuntimeEvent {
        events::RuntimeEvent::SubagentStateChanged {
            id,
            name: self.name.clone(),
            status: self.status,
            outcome: self.outcome,
            generation: self.generation,
            task: self.task.clone(),
            parent_id: None,
            parent_name: None,
            harness: self.harness.clone(),
            model: self.model.clone(),
            created_at_unix_ms: self.created_at_unix_ms,
            generation_started_at_unix_ms: self.generation_started_at_unix_ms,
            generation_finished_at_unix_ms: self.generation_finished_at_unix_ms,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentValue {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub output: Value,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates: Option<SubagentUpdates>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SubagentUpdates {
    pub items: Vec<Value>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
struct SubagentListing {
    id: String,
    name: String,
    status: SubagentStatus,
    generation: u64,
    task: String,
}

struct OutputContract {
    schema: Value,
    validator: jsonschema::Validator,
}

impl OutputContract {
    fn new(schema: Value) -> Result<Self, ToolError> {
        let validator = jsonschema::validator_for(&schema)
            .map_err(|error| ToolError::InvalidInput(format!("invalid output_schema: {error}")))?;
        Ok(Self { schema, validator })
    }

    fn prompt(&self, prompt: String) -> String {
        format!(
            "{prompt}\n\nReturn only a JSON value matching this JSON Schema. Do not wrap it in Markdown or add commentary:\n{}",
            serde_json::to_string(&self.schema).expect("JSON Schema serializes")
        )
    }

    fn parse(&self, output: &str) -> Option<Value> {
        let value: Value = serde_json::from_str(output.trim()).ok()?;
        self.validator.validate(&value).ok()?;
        Some(value)
    }
}

fn structured_prompt(prompt: String, contract: Option<&OutputContract>) -> String {
    match contract {
        Some(contract) => contract.prompt(prompt),
        None => prompt,
    }
}

fn structured_output(output: String, contract: Option<&OutputContract>) -> Value {
    contract
        .and_then(|contract| contract.parse(&output))
        .unwrap_or_else(|| Value::String(output))
}

fn turn_output(
    output: ChildOutput,
    contract: Option<&OutputContract>,
) -> (Value, Option<SubagentUpdates>) {
    let value = structured_output(output.text, contract);
    let updates =
        (!output.updates.is_empty() || output.updates_truncated).then_some(SubagentUpdates {
            items: output.updates,
            truncated: output.updates_truncated,
        });
    (value, updates)
}

#[derive(Default)]
struct CreateOptions {
    name: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    cwd: Option<PathBuf>,
}

struct ForkSuccess {
    value: SubagentValue,
    acknowledge: oneshot::Sender<()>,
}

struct ForkOperation {
    source_id: String,
    source_state: Arc<AsyncMutex<State>>,
    source_child: ChildSession,
    id: String,
    prompt: String,
    name: Option<String>,
    harness: String,
    model: Option<String>,
    kit: bool,
    root: PathBuf,
    generation: u64,
    depth: usize,
    cancellation: TurnCancellation,
    contract: Option<Arc<OutputContract>>,
    permit: OwnedSemaphorePermit,
    native_fork: bool,
}

impl Subagents {
    pub(crate) fn new(config: ChildConfig, max_depth: usize) -> Self {
        Self {
            config,
            max_depth,
            sessions: Arc::default(),
            capacity: Arc::new(Semaphore::new(MAX_LIVE_SUBAGENTS)),
            event_sink: Arc::new(|event| {
                events::emit(event);
                Ok(())
            }),
        }
    }

    pub(crate) fn child_config(&self) -> ChildConfig {
        self.config.clone()
    }

    pub(crate) fn fresh(&self) -> Self {
        Self::new(self.config.clone(), self.max_depth)
    }

    pub(crate) fn fresh_with_parent(&self, id: String, name: String) -> Self {
        Self::new(
            self.config.clone().with_parent_context(id, name),
            self.max_depth,
        )
    }

    fn emit_event(&self, mut event: events::RuntimeEvent) {
        if let events::RuntimeEvent::SubagentStateChanged {
            parent_id,
            parent_name,
            ..
        } = &mut event
        {
            *parent_id = self.config.parent_id.clone();
            *parent_name = self.config.parent_name.clone();
        }
        let _ = (self.event_sink)(&event);
    }

    fn harness_references(&self) -> Vec<String> {
        self.config.harnesses.references()
    }

    fn resolve_root(&self, cwd: Option<PathBuf>) -> Result<PathBuf, ChildError> {
        let Some(cwd) = cwd else {
            return Ok(self.config.root.clone());
        };
        let path = if cwd.is_absolute() {
            cwd
        } else {
            self.config.root.join(cwd)
        };
        let root = path.canonicalize().map_err(|error| {
            ChildError::Failed(format!(
                "could not open subagent working directory {}: {error}",
                path.display()
            ))
        })?;
        if !root.is_dir() {
            return Err(ChildError::Failed(format!(
                "subagent working directory is not a directory: {}",
                root.display()
            )));
        }
        Ok(root)
    }

    async fn create(
        &self,
        prompt: String,
        options: CreateOptions,
        depth: usize,
        cancellation: TurnCancellation,
        contract: Option<&OutputContract>,
    ) -> Result<SubagentValue, ChildError> {
        self.check_depth(depth)?;
        let permit = self.reserve()?;
        let id = session::new_id();
        let CreateOptions {
            name,
            harness,
            model,
            cwd,
        } = options;
        let root = self.resolve_root(cwd)?;
        let harness = harness.unwrap_or_else(|| self.config.default_harness.clone());
        if !self.config.harnesses.contains(&harness) {
            return Err(ChildError::Failed(format!(
                "unknown ACP harness {harness:?}"
            )));
        }
        let model = model
            .as_deref()
            .map(|model| self.config.harnesses.resolve_model(&harness, model))
            .transpose()
            .map_err(ChildError::Failed)?;
        let kit = self.config.harnesses.is_kit(&harness);
        let now = events::now_millis();
        let state = self.insert_starting(
            id.clone(),
            State {
                name: name.unwrap_or_default(),
                status: SubagentStatus::Starting,
                task: task_summary(&prompt),
                generation: 1,
                handle_generation: 1,
                outcome: None,
                created_at_unix_ms: now,
                generation_started_at_unix_ms: now,
                generation_finished_at_unix_ms: None,
                output: Value::Null,
                updates: None,
                harness: harness.clone(),
                model: model.clone(),
                kit,
                root: root.clone(),
                child: None,
                forking: None,
                permit: Some(permit),
            },
        )?;
        let persisted = kit.then(|| (id.clone(), false));
        let child_config = self
            .config
            .clone()
            .with_root(root)
            .with_parent_context(id.clone(), state.lock().await.name.clone());
        {
            let locked = state.lock().await;
            self.check_active(&locked)?;
        }
        let child = match ChildSession::start(
            child_config,
            harness,
            persisted,
            model,
            depth + 1,
            cancellation.clone(),
        )
        .await
        {
            Ok(child) => child,
            Err(error) => {
                self.fail_removed_and_remove(&id, &state).await;
                return Err(error);
            }
        };
        {
            let mut locked = state.lock().await;
            if let Err(error) = self.check_active(&locked) {
                drop(locked);
                let error = self
                    .reject_uninstalled_child_owned(
                        id.clone(),
                        Arc::clone(&state),
                        child.clone(),
                        error,
                    )
                    .await;
                return Err(error);
            }
            locked.status = SubagentStatus::Working;
            locked.child = Some(child.clone());
            self.emit_event(locked.runtime_event(id.clone()));
        }
        self.monitor_child_exit(id.clone(), &state, &child);
        let output = match child
            .prompt(structured_prompt(prompt, contract), cancellation)
            .await
        {
            Ok(output) => output,
            Err(error) => {
                self.fail_removed_and_remove(&id, &state).await;
                let _ = child.close().await;
                return Err(error);
            }
        };
        let (output, updates) = turn_output(output, contract);
        let mut locked = state.lock().await;
        self.check_active(&locked)?;
        locked.status = SubagentStatus::Idle;
        locked.outcome = Some(GenerationOutcome::Success);
        locked.generation_finished_at_unix_ms = Some(events::now_millis());
        locked.output.clone_from(&output);
        locked.updates.clone_from(&updates);
        let name = locked.name.clone();
        let event = locked.runtime_event(id.clone());
        drop(locked);
        self.emit_event(event);
        Ok(SubagentValue {
            id,
            name: Some(name),
            output,
            generation: 1,
            updates,
        })
    }

    async fn prompt(
        &self,
        prior: SubagentValue,
        prompt: String,
        cancellation: TurnCancellation,
        contract: Option<&OutputContract>,
    ) -> Result<SubagentValue, ChildError> {
        let state = self.lookup(&prior)?;
        let mut locked = tokio::select! {
            locked = state.lock() => locked,
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        };
        self.check_ready(&locked)?;
        if locked.forking.is_some() {
            return Err(ChildError::Failed(
                "subagent session is being forked".into(),
            ));
        }
        self.check_generation(&prior, locked.handle_generation)?;
        let generation = locked
            .generation
            .checked_add(1)
            .ok_or_else(|| ChildError::Failed("subagent generation overflow".into()))?;
        locked.status = SubagentStatus::Working;
        locked.task = task_summary(&prompt);
        locked.generation = generation;
        locked.outcome = None;
        locked.generation_started_at_unix_ms = events::now_millis();
        locked.generation_finished_at_unix_ms = None;
        let child = locked
            .child
            .clone()
            .ok_or_else(|| ChildError::Failed("subagent session is still starting".into()))?;
        let name = locked.name.clone();
        let event = locked.runtime_event(prior.id.clone());
        drop(locked);
        self.emit_event(event);
        match child
            .prompt(structured_prompt(prompt, contract), cancellation)
            .await
        {
            Ok(output) => {
                let (output, updates) = turn_output(output, contract);
                let mut locked = state.lock().await;
                self.check_active(&locked)?;
                locked.status = SubagentStatus::Idle;
                locked.handle_generation = generation;
                locked.outcome = Some(GenerationOutcome::Success);
                locked.generation_finished_at_unix_ms = Some(events::now_millis());
                locked.output.clone_from(&output);
                locked.updates.clone_from(&updates);
                let event = locked.runtime_event(prior.id.clone());
                drop(locked);
                self.emit_event(event);
                Ok(SubagentValue {
                    id: prior.id,
                    name: Some(name),
                    output,
                    generation,
                    updates,
                })
            }
            Err(error) => {
                if child_error_is_terminal(&error, &child) {
                    self.fail_removed_and_remove(&prior.id, &state).await;
                } else {
                    let mut locked = state.lock().await;
                    if locked.status != SubagentStatus::Removed {
                        locked.status = SubagentStatus::Idle;
                        locked.outcome = Some(GenerationOutcome::Failed);
                        locked.generation_finished_at_unix_ms = Some(events::now_millis());
                        // A failed call returns no replacement handle, so preserve the
                        // accepted handle generation for a retry while keeping lifecycle
                        // generations monotonic.
                        let event = locked.runtime_event(prior.id.clone());
                        drop(locked);
                        self.emit_event(event);
                    }
                }
                Err(error)
            }
        }
    }

    async fn fork(
        &self,
        prior: SubagentValue,
        prompt: String,
        name: Option<String>,
        depth: usize,
        cancellation: TurnCancellation,
        contract: Option<Arc<OutputContract>>,
    ) -> Result<SubagentValue, ChildError> {
        self.check_depth(depth)?;
        let permit = self.reserve()?;
        let source_state = self.lookup(&prior)?;
        let mut source = tokio::select! {
            source = source_state.lock() => source,
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        };
        self.check_ready(&source)?;
        if source.forking.is_some() {
            return Err(ChildError::Failed(
                "subagent session is being forked".into(),
            ));
        }
        self.check_generation(&prior, source.handle_generation)?;

        let id = session::new_id();
        let source_child = source
            .child
            .clone()
            .ok_or_else(|| ChildError::Failed("subagent session is still starting".into()))?;
        let native_fork = source_child.supports_native_fork();
        if !native_fork && !source.kit {
            return Err(ChildError::Failed(format!(
                "ACP harness {:?} does not advertise session/fork; transcript fallback is only available for Kit",
                source.harness
            )));
        }
        let generation = source
            .generation
            .checked_add(1)
            .ok_or_else(|| ChildError::Failed("subagent generation overflow".into()))?;
        source.forking = Some(id.clone());
        let operation = ForkOperation {
            source_id: prior.id,
            source_state: Arc::clone(&source_state),
            source_child,
            id: id.clone(),
            prompt,
            name,
            harness: source.harness.clone(),
            model: source.model.clone(),
            kit: source.kit,
            root: source.root.clone(),
            generation,
            depth,
            cancellation,
            contract,
            permit,
            native_fork,
        };
        drop(source);

        let (reply, response) = oneshot::channel();
        let manager = self.clone();
        tokio::spawn(async move {
            let source_state = Arc::clone(&operation.source_state);
            let reservation = operation.id.clone();
            let result = manager.run_fork(operation, &reply).await;
            manager.finish_forking(&source_state, &reservation).await;
            match result {
                Ok(value) => manager.handoff_fork_success(reply, value).await,
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            }
        });
        match response.await.map_err(|_| {
            ChildError::Failed("subagent fork task stopped before returning a result".into())
        })? {
            Ok(success) => {
                success.acknowledge.send(()).map_err(|_| {
                    ChildError::Failed(
                        "subagent fork task stopped before transferring ownership".into(),
                    )
                })?;
                Ok(success.value)
            }
            Err(error) => Err(error),
        }
    }

    async fn run_fork(
        &self,
        operation: ForkOperation,
        reply: &oneshot::Sender<Result<ForkSuccess, ChildError>>,
    ) -> Result<SubagentValue, ChildError> {
        let ForkOperation {
            source_id,
            source_state,
            source_child,
            id,
            prompt,
            name,
            harness,
            model,
            kit,
            root,
            generation,
            depth,
            cancellation,
            contract,
            permit,
            native_fork,
        } = operation;

        if reply.is_closed() {
            return Err(ChildError::Cancelled);
        }

        let now = events::now_millis();
        let state = self.insert_starting(
            id.clone(),
            State {
                name: name.unwrap_or_default(),
                status: SubagentStatus::Starting,
                task: task_summary(&prompt),
                generation,
                handle_generation: generation,
                outcome: None,
                created_at_unix_ms: now,
                generation_started_at_unix_ms: now,
                generation_finished_at_unix_ms: None,
                output: Value::Null,
                updates: None,
                harness: harness.clone(),
                model: model.clone(),
                kit,
                root: root.clone(),
                child: None,
                forking: None,
                permit: None,
            },
        )?;
        let branch_name = state.lock().await.name.clone();
        if reply.is_closed() {
            self.fail_removed_and_remove(&id, &state).await;
            return Err(ChildError::Cancelled);
        }

        if !native_fork {
            let transcript_root = root.clone();
            let source_id = source_id.clone();
            let branch_id = id.clone();
            let cloned = tokio::task::spawn_blocking(move || {
                session::clone_completed(&transcript_root, &source_id, &branch_id)
            })
            .await
            .map_err(|error| ChildError::Failed(format!("transcript clone task failed: {error}")))
            .and_then(|result| result.map_err(ChildError::Failed));
            if let Err(error) = cloned {
                self.fail_removed_and_remove(&id, &state).await;
                return Err(error);
            }
        }
        if reply.is_closed() {
            self.fail_removed_and_remove(&id, &state).await;
            return Err(ChildError::Cancelled);
        }

        let child_config = self
            .config
            .clone()
            .with_root(root)
            .with_parent_context(id.clone(), branch_name.clone());
        let child_result = if native_fork {
            let parent = kit.then(|| (id.clone(), branch_name));
            source_child
                .fork(model.as_deref(), parent, &cancellation)
                .await
        } else {
            ChildSession::start(
                child_config,
                harness.clone(),
                Some((id.clone(), true)),
                model.clone(),
                depth + 1,
                cancellation.clone(),
            )
            .await
        };
        let child = match child_result {
            Ok(child) => child,
            Err(error) => {
                self.fail_removed_and_remove(&id, &state).await;
                return Err(error);
            }
        };

        if let Err(error) = self
            .revalidate_fork_source(&source_id, &source_state, &id)
            .await
        {
            self.fail_removed_and_remove(&id, &state).await;
            return Err(self
                .cleanup_uninstalled_child(child, Some(permit), error)
                .await);
        }
        self.finish_forking(&source_state, &id).await;
        if reply.is_closed() {
            self.fail_removed_and_remove(&id, &state).await;
            return Err(self
                .cleanup_uninstalled_child(child, Some(permit), ChildError::Cancelled)
                .await);
        }
        {
            let mut locked = state.lock().await;
            if let Err(error) = self.check_active(&locked) {
                drop(locked);
                return Err(self
                    .cleanup_uninstalled_child(child, Some(permit), error)
                    .await);
            }
            locked.child = Some(child.clone());
            locked.permit = Some(permit);
            locked.status = SubagentStatus::Working;
            self.emit_event(locked.runtime_event(id.clone()));
        }
        self.monitor_child_exit(id.clone(), &state, &child);

        if reply.is_closed() {
            return Err(self
                .cleanup_installed_child(&id, &state, &child, ChildError::Cancelled)
                .await);
        }
        let output = match child
            .prompt(structured_prompt(prompt, contract.as_deref()), cancellation)
            .await
        {
            Ok(output) => output,
            Err(error) => {
                return Err(self
                    .cleanup_installed_child(&id, &state, &child, error)
                    .await);
            }
        };
        let (output, updates) = turn_output(output, contract.as_deref());
        let mut locked = state.lock().await;
        if reply.is_closed() {
            drop(locked);
            return Err(self
                .cleanup_installed_child(&id, &state, &child, ChildError::Cancelled)
                .await);
        }
        if let Err(error) = self.check_active(&locked) {
            drop(locked);
            return Err(self
                .cleanup_installed_child(&id, &state, &child, error)
                .await);
        }
        locked.status = SubagentStatus::Idle;
        locked.outcome = Some(GenerationOutcome::Success);
        locked.generation_finished_at_unix_ms = Some(events::now_millis());
        locked.output.clone_from(&output);
        locked.updates.clone_from(&updates);
        let name = locked.name.clone();
        let event = locked.runtime_event(id.clone());
        drop(locked);
        self.emit_event(event);
        Ok(SubagentValue {
            id,
            name: Some(name),
            output,
            generation,
            updates,
        })
    }

    async fn list(
        &self,
        cancellation: &TurnCancellation,
    ) -> Result<Vec<SubagentListing>, ChildError> {
        let states = self
            .sessions
            .lock()
            .map_err(|_| ChildError::Failed("subagent registry lock was poisoned".into()))?
            .iter()
            .map(|(id, entry)| (id.clone(), Arc::clone(&entry.state)))
            .collect::<Vec<_>>();
        let mut values = Vec::with_capacity(states.len());
        for (id, state) in states {
            let state = tokio::select! {
                state = state.lock() => state,
                () = cancellation.cancelled() => return Err(ChildError::Cancelled),
            };
            let child_closed = state.child.as_ref().is_some_and(ChildSession::is_closed);
            if state.status != SubagentStatus::Removed && !child_closed {
                values.push((
                    state.created_at_unix_ms,
                    SubagentListing {
                        id,
                        name: state.name.clone(),
                        status: state.status,
                        generation: state.generation,
                        task: state.task.clone(),
                    },
                ));
            }
        }
        values.sort_by(|(left_created, left), (right_created, right)| {
            left_created
                .cmp(right_created)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(values.into_iter().map(|(_, value)| value).collect())
    }

    async fn close(&self, id: &str, cancellation: &TurnCancellation) -> Result<(), ChildError> {
        let state = self
            .sessions
            .lock()
            .map_err(|_| ChildError::Failed("subagent registry lock was poisoned".into()))?
            .get(id)
            .map(|entry| Arc::clone(&entry.state))
            .ok_or_else(|| ChildError::Failed(format!("unknown subagent session {id:?}")))?;
        let mut locked = tokio::select! {
            locked = state.lock() => locked,
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        };
        self.check_active(&locked)?;
        locked.status = SubagentStatus::Removed;
        locked.forking = None;
        let child = locked.child.take();
        let event = locked.runtime_event(id.to_string());
        drop(locked);
        self.remove_if_same(id, &state);
        self.emit_event(event);

        let Some(child) = child else {
            return Ok(());
        };
        match child.close().await {
            Ok(()) => Ok(()),
            Err(error) if child_error_is_terminal(&error, &child) => Err(error),
            Err(error) => {
                self.retain_permit_until_process_exit(&state, &child).await;
                Err(error)
            }
        }
    }

    fn lookup(&self, prior: &SubagentValue) -> Result<Arc<AsyncMutex<State>>, ChildError> {
        self.sessions
            .lock()
            .map_err(|_| ChildError::Failed("subagent registry lock was poisoned".into()))?
            .get(&prior.id)
            .map(|entry| Arc::clone(&entry.state))
            .ok_or_else(|| ChildError::Failed(format!("unknown subagent session {:?}", prior.id)))
    }

    fn insert_starting(
        &self,
        id: String,
        mut state: State,
    ) -> Result<Arc<AsyncMutex<State>>, ChildError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| ChildError::Failed("subagent registry lock was poisoned".into()))?;
        let mut used = sessions
            .values()
            .map(|entry| entry.name.to_lowercase())
            .collect::<HashSet<_>>();
        let preferred = std::mem::take(&mut state.name);
        let name = allocate_display_name(&mut used, Some(&preferred))?;
        state.name.clone_from(&name);
        let event = state.runtime_event(id.clone());
        let state = Arc::new(AsyncMutex::new(state));
        sessions.insert(
            id,
            SessionEntry {
                name,
                state: Arc::clone(&state),
            },
        );
        drop(sessions);
        self.emit_event(event);
        Ok(state)
    }

    fn reserve(&self) -> Result<OwnedSemaphorePermit, ChildError> {
        let mut removed_events = Vec::new();
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| ChildError::Failed("subagent registry lock was poisoned".into()))?;
        sessions.retain(|id, entry| {
            entry.state.try_lock().map_or(true, |mut state| {
                if !state.child.as_ref().is_some_and(ChildSession::is_closed) {
                    return true;
                }
                if state.status != SubagentStatus::Removed {
                    if state.status != SubagentStatus::Idle {
                        state.outcome = Some(GenerationOutcome::Failed);
                    }
                    state.status = SubagentStatus::Removed;
                    state.forking = None;
                    state
                        .generation_finished_at_unix_ms
                        .get_or_insert_with(events::now_millis);
                    removed_events.push(state.runtime_event(id.clone()));
                }
                false
            })
        });
        drop(sessions);
        for event in removed_events {
            self.emit_event(event);
        }
        Arc::clone(&self.capacity).try_acquire_owned().map_err(|_| {
            ChildError::Failed(format!(
                "live subagent session limit ({MAX_LIVE_SUBAGENTS}) reached"
            ))
        })
    }

    async fn handoff_fork_success(
        &self,
        reply: oneshot::Sender<Result<ForkSuccess, ChildError>>,
        value: SubagentValue,
    ) {
        let cleanup = value.clone();
        let (acknowledge, acknowledged) = oneshot::channel();
        let sent = reply.send(Ok(ForkSuccess { value, acknowledge })).is_ok();
        if !sent || acknowledged.await.is_err() {
            self.cleanup_abandoned_fork(&cleanup).await;
        }
    }

    async fn cleanup_abandoned_fork(&self, value: &SubagentValue) {
        let Ok(state) = self.lookup(value) else {
            return;
        };
        let child = state.lock().await.child.clone();
        if let Some(child) = child {
            let _ = self
                .cleanup_installed_child(&value.id, &state, &child, ChildError::Cancelled)
                .await;
        } else {
            self.fail_removed_and_remove(&value.id, &state).await;
        }
    }

    async fn revalidate_fork_source(
        &self,
        id: &str,
        state: &Arc<AsyncMutex<State>>,
        reservation: &str,
    ) -> Result<(), ChildError> {
        let registered = self
            .sessions
            .lock()
            .map_err(|_| ChildError::Failed("subagent registry lock was poisoned".into()))?
            .get(id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.state, state));
        if !registered {
            return Err(ChildError::Failed("subagent session is retired".into()));
        }
        let locked = state.lock().await;
        self.check_active(&locked)?;
        if locked.forking.as_deref() != Some(reservation) {
            return Err(ChildError::Failed(
                "subagent fork reservation is no longer active".into(),
            ));
        }
        if locked.child.as_ref().is_none_or(ChildSession::is_closed) {
            return Err(ChildError::TerminalFailed(
                "nested agent process is no longer running".into(),
            ));
        }
        Ok(())
    }

    async fn cleanup_uninstalled_child(
        &self,
        child: ChildSession,
        permit: Option<OwnedSemaphorePermit>,
        error: ChildError,
    ) -> ChildError {
        match child.close().await {
            Ok(()) => error,
            Err(cleanup) if child_error_is_terminal(&cleanup, &child) => error,
            Err(cleanup) => {
                Self::watch_permit_until_process_exit(permit, &child);
                ChildError::Failed(format!(
                    "{error}; failed to clean up retired subagent session: {cleanup}"
                ))
            }
        }
    }

    async fn cleanup_installed_child(
        &self,
        id: &str,
        state: &Arc<AsyncMutex<State>>,
        child: &ChildSession,
        error: ChildError,
    ) -> ChildError {
        let mut locked = state.lock().await;
        let event = if locked.status == SubagentStatus::Removed {
            None
        } else {
            locked.status = SubagentStatus::Removed;
            locked.forking = None;
            locked.outcome = Some(GenerationOutcome::Failed);
            locked.generation_finished_at_unix_ms = Some(events::now_millis());
            Some(locked.runtime_event(id.to_string()))
        };
        drop(locked);
        self.remove_if_same(id, state);
        if let Some(event) = event {
            self.emit_event(event);
        }

        match child.close().await {
            Ok(()) => error,
            Err(cleanup) if child_error_is_terminal(&cleanup, child) => error,
            Err(cleanup) => {
                self.retain_permit_until_process_exit(state, child).await;
                ChildError::Failed(format!(
                    "{error}; failed to clean up retired subagent session: {cleanup}"
                ))
            }
        }
    }

    async fn reject_uninstalled_child_owned(
        &self,
        id: String,
        state: Arc<AsyncMutex<State>>,
        child: ChildSession,
        error: ChildError,
    ) -> ChildError {
        let manager = self.clone();
        match tokio::spawn(async move {
            manager
                .cleanup_installed_child(&id, &state, &child, error)
                .await
        })
        .await
        {
            Ok(error) => error,
            Err(error) => {
                ChildError::Failed(format!("retired subagent cleanup task failed: {error}"))
            }
        }
    }

    async fn retain_permit_until_process_exit(
        &self,
        state: &Arc<AsyncMutex<State>>,
        child: &ChildSession,
    ) {
        let permit = state.lock().await.permit.take();
        Self::watch_permit_until_process_exit(permit, child);
    }

    fn watch_permit_until_process_exit(permit: Option<OwnedSemaphorePermit>, child: &ChildSession) {
        let Some(permit) = permit else {
            return;
        };
        let mut closed = child.closed_signal();
        tokio::spawn(async move {
            if !*closed.borrow() {
                let _ = closed.changed().await;
            }
            drop(permit);
        });
    }

    fn monitor_child_exit(&self, id: String, state: &Arc<AsyncMutex<State>>, child: &ChildSession) {
        // Do not keep the manager alive while waiting for the child. Dropping a
        // session-scoped manager must drop its child handles so their ACP
        // processes (including native-fork siblings) can terminate.
        let sessions = Arc::downgrade(&self.sessions);
        let state = Arc::downgrade(state);
        let event_sink = Arc::clone(&self.event_sink);
        let parent_id = self.config.parent_id.clone();
        let parent_name = self.config.parent_name.clone();
        let mut closed = child.closed_signal();
        tokio::spawn(async move {
            if !*closed.borrow() {
                let _ = closed.changed().await;
            }
            let Some(state) = state.upgrade() else {
                return;
            };
            let mut locked = state.lock().await;
            if locked.status == SubagentStatus::Removed {
                return;
            }
            if locked.status != SubagentStatus::Idle {
                locked.outcome = Some(GenerationOutcome::Failed);
            }
            locked.status = SubagentStatus::Removed;
            locked.forking = None;
            locked
                .generation_finished_at_unix_ms
                .get_or_insert_with(events::now_millis);
            let mut event = locked.runtime_event(id.clone());
            drop(locked);
            if let Some(sessions) = sessions.upgrade()
                && let Ok(mut sessions) = sessions.lock()
                && sessions
                    .get(&id)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.state, &state))
            {
                sessions.remove(&id);
            }
            if let events::RuntimeEvent::SubagentStateChanged {
                parent_id: event_parent_id,
                parent_name: event_parent_name,
                ..
            } = &mut event
            {
                *event_parent_id = parent_id;
                *event_parent_name = parent_name;
            }
            let _ = event_sink(&event);
        });
    }

    async fn fail_removed_and_remove(&self, id: &str, state: &Arc<AsyncMutex<State>>) {
        let mut locked = state.lock().await;
        let event = if locked.status == SubagentStatus::Removed {
            None
        } else {
            locked.status = SubagentStatus::Removed;
            locked.forking = None;
            locked.outcome = Some(GenerationOutcome::Failed);
            locked.generation_finished_at_unix_ms = Some(events::now_millis());
            Some(locked.runtime_event(id.to_string()))
        };
        drop(locked);
        self.remove_if_same(id, state);
        if let Some(event) = event {
            self.emit_event(event);
        }
    }

    async fn finish_forking(&self, state: &Arc<AsyncMutex<State>>, reservation: &str) {
        let mut locked = state.lock().await;
        if locked.forking.as_deref() == Some(reservation) {
            locked.forking = None;
        }
    }

    fn remove_if_same(&self, id: &str, expected: &Arc<AsyncMutex<State>>) {
        if let Ok(mut sessions) = self.sessions.lock()
            && sessions
                .get(id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.state, expected))
        {
            sessions.remove(id);
        }
    }
    fn check_active(&self, state: &State) -> Result<(), ChildError> {
        if state.status == SubagentStatus::Removed {
            Err(ChildError::Failed("subagent session is retired".into()))
        } else {
            Ok(())
        }
    }
    fn check_ready(&self, state: &State) -> Result<(), ChildError> {
        self.check_active(state)?;
        match state.status {
            SubagentStatus::Idle => Ok(()),
            SubagentStatus::Starting => Err(ChildError::Failed(
                "subagent session is still starting".into(),
            )),
            SubagentStatus::Working => Err(ChildError::Failed(
                "subagent session is already working".into(),
            )),
            SubagentStatus::Removed => unreachable!("check_active rejects removed sessions"),
        }
    }
    fn check_generation(&self, prior: &SubagentValue, actual: u64) -> Result<(), ChildError> {
        if prior.generation == actual {
            Ok(())
        } else {
            Err(ChildError::Failed(format!(
                "stale subagent generation {}; current generation is {actual}",
                prior.generation
            )))
        }
    }
    fn check_depth(&self, depth: usize) -> Result<(), ChildError> {
        if depth < self.max_depth {
            Ok(())
        } else {
            Err(ChildError::Failed(format!(
                "subagent depth limit ({}) reached",
                self.max_depth
            )))
        }
    }
}

#[derive(Clone)]
pub struct SubagentTool {
    manager: Subagents,
    depth: usize,
    spec: ToolSpec,
}
#[derive(Clone)]
pub struct PromptTool {
    manager: Subagents,
    spec: ToolSpec,
}
#[derive(Clone)]
pub struct ForkTool {
    manager: Subagents,
    depth: usize,
    spec: ToolSpec,
}
#[derive(Clone)]
pub struct SubagentsTool {
    manager: Subagents,
    spec: ToolSpec,
}
type CancelBackground = Arc<dyn Fn(&str, bool) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct CloseTool {
    manager: Subagents,
    cancel_background: CancelBackground,
    spec: ToolSpec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentId {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackgroundCallId {
    call_id: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CloseInput {
    Handle(SubagentValue),
    Id(SubagentId),
    BackgroundCall(BackgroundCallId),
}

impl CloseInput {
    fn target(self) -> (String, bool) {
        match self {
            Self::Handle(value) => (value.id, false),
            Self::Id(value) => (value.id, false),
            Self::BackgroundCall(value) => (value.call_id, true),
        }
    }
}

fn value_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"},"output":{},"generation":{"type":"integer","minimum":1},"updates":{"type":"object","properties":{"items":{"type":"array","items":{"type":"object"}},"truncated":{"type":"boolean"}},"required":["items","truncated"],"additionalProperties":false}},"required":["id","output","generation"],"additionalProperties":false})
}
fn listing_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"},"status":{"type":"string","enum":["starting","working","idle"]},"generation":{"type":"integer","minimum":1},"task":{"type":"string"}},"required":["id","name","status","generation","task"],"additionalProperties":false})
}
fn continuation_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"subagent":value_schema(),"prompt":{"type":"string"},"output_schema":{"oneOf":[{"type":"object"},{"type":"boolean"}]}},"required":["subagent","prompt"],"additionalProperties":false})
}
fn display_name_schema() -> serde_json::Value {
    json!({"type":"string","description":"Provide a concise role-oriented display name that is unique among live sibling subagents. Valid names are 1-32 bytes of printable ASCII. Kit allocates a unique fallback when the name is omitted or invalid."})
}
fn id_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false})
}
fn call_id_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"call_id":{"type":"string"}},"required":["call_id"],"additionalProperties":false})
}

impl SubagentTool {
    pub fn new(manager: Subagents, depth: usize) -> Self {
        let harnesses = manager.harness_references();
        let usage = if depth == 1 {
            concat!(
                "Use this only if you uncover independent workstreams whose parallel execution would yield quicker or better results. ",
                "Give each subagent a focused assignment based on what you discovered, and synthesize its findings into your response. "
            )
        } else {
            concat!(
                "Use a fresh subagent for work that changes phase or objective instead of carrying unrelated history. ",
                "Keep outputs focused, pass only necessary context, reuse sessions only when continuity helps, and close subagents when no longer needed. "
            )
        };
        let description = format!(
            "Start a parent-owned configured ACP harness, preferably assign a concise role-oriented display name, prompt it, and return its reusable session value. {usage}Omit `harness` and `model` unless the user or active workflow explicitly supplies the exact override or a configured alias. Never choose an override based on your own model, provider, publisher, familiarity, cost, or perceived quality; advertised choices indicate availability, not preference."
        );
        Self { manager, depth, spec: ToolSpec::new(ToolName::new("subagent"), description, json!({"type":"object","properties":{"prompt":{"type":"string"},"name":display_name_schema(),"harness":{"type":"string","enum":harnesses,"description":"Override the user's configured harness preference with this value. Default to omitting it."},"model":{"type":"string","minLength":1,"description":"Exact ACP model selection ID or configured alias explicitly requested by the user or active workflow. Applies only to this new session; default to omitting it."},"cwd":{"type":"string","minLength":1,"description":"Working directory for the new subagent. Relative paths resolve from Kit's working directory."},"output_schema":{"oneOf":[{"type":"object"},{"type":"boolean"}]}},"required":["prompt"],"additionalProperties":false})).with_output_schema(value_schema()).with_annotations(ToolAnnotations::new()) }
    }
}
impl PromptTool {
    pub fn new(manager: Subagents) -> Self {
        Self {
            manager,
            spec: ToolSpec::new(
                ToolName::new("prompt"),
                "Re-prompt the same completed ACP subagent session using a prior subagent value.",
                continuation_schema(),
            )
            .with_output_schema(value_schema())
            .with_annotations(ToolAnnotations::new()),
        }
    }
}
impl ForkTool {
    pub fn new(manager: Subagents, depth: usize) -> Self {
        let usage = if depth == 1 {
            " Use this only for an independent workstream whose parallel execution would yield quicker or better results, and synthesize its findings into your response."
        } else {
            ""
        };
        let description = format!(
            "Fork a completed ACP subagent session using native capability support or the isolated Kit fallback, preferably assign the fork a concise role-oriented display name, prompt it, and return the new session value.{usage}"
        );
        Self { manager, depth, spec: ToolSpec::new(ToolName::new("fork"), description, json!({"type":"object","properties":{"subagent":value_schema(),"prompt":{"type":"string"},"name":display_name_schema(),"output_schema":{"oneOf":[{"type":"object"},{"type":"boolean"}]}},"required":["subagent","prompt"],"additionalProperties":false})).with_output_schema(value_schema()).with_annotations(ToolAnnotations::new()) }
    }
}
impl SubagentsTool {
    pub fn new(manager: Subagents) -> Self {
        Self {
            manager,
            spec: ToolSpec::new(
                ToolName::new("subagents"),
                "List active subagent sessions owned by this parent, including sessions whose first prompt is still starting.",
                json!({"type":"object","properties":{},"additionalProperties":false}),
            )
            .with_output_schema(json!({"type":"array","items":listing_schema()}))
            .with_annotations(ToolAnnotations::new()),
        }
    }
}
impl CloseTool {
    pub fn new(
        manager: Subagents,
        cancel_background: impl Fn(&str, bool) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            manager,
            cancel_background: Arc::new(cancel_background),
            spec: ToolSpec::new(
                ToolName::new("close"),
                "Close an active subagent by its complete handle or `{ id }`, or cancel a background tool call with `{ call_id }`. Closed subagent handles become unusable and their capacity is released.",
                json!({"oneOf":[value_schema(), id_schema(), call_id_schema()]}),
            )
            .with_output_schema(id_schema())
            .with_annotations(ToolAnnotations::new()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    prompt: String,
    name: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    cwd: Option<PathBuf>,
    #[serde(default, deserialize_with = "deserialize_output_schema")]
    output_schema: Option<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Continuation {
    subagent: SubagentValue,
    prompt: String,
    #[serde(default, deserialize_with = "deserialize_output_schema")]
    output_schema: Option<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkInput {
    subagent: SubagentValue,
    prompt: String,
    name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_output_schema")]
    output_schema: Option<Value>,
}

fn deserialize_output_schema<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Err(serde::de::Error::custom(
            "output_schema must be a JSON Schema object or boolean",
        ));
    }
    Ok(Some(value))
}
fn cancellation(context: &ToolContext<'_>) -> TurnCancellation {
    context
        .cancellation
        .as_ref()
        .map(|value| value.handle().checkpoint())
        .unwrap_or_default()
}
fn result(
    request: ToolRequest,
    value: Result<SubagentValue, ChildError>,
) -> Result<ToolResult, ToolError> {
    let value = value.map_err(|error| match error {
        ChildError::Cancelled | ChildError::TerminalCancelled => ToolError::Cancelled,
        ChildError::Failed(error) | ChildError::TerminalFailed(error) => {
            ToolError::ExecutionFailed(error)
        }
    })?;
    Ok(ToolResult::new(ToolResultPart::success(
        request.call_id,
        ToolOutput::structured(serde_json::to_value(value).expect("subagent value serializes")),
    )))
}

#[async_trait]
impl Tool for SubagentsTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        if request.input != json!({}) {
            return Err(ToolError::InvalidInput(
                "subagents input must be an empty object".into(),
            ));
        }
        let values = self
            .manager
            .list(&cancellation(context))
            .await
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(
                serde_json::to_value(values).expect("subagent values serialize"),
            ),
        )))
    }
}

#[async_trait]
impl Tool for CloseTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let (id, background_call) = serde_json::from_value::<CloseInput>(request.input.clone())
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?
            .target();
        if !(self.cancel_background)(&id, background_call) {
            self.manager
                .close(&id, &cancellation(context))
                .await
                .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        }
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(json!({ "id": id })),
        )))
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: Input = serde_json::from_value(request.input.clone())
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let contract = input.output_schema.map(OutputContract::new).transpose()?;
        result(
            request,
            self.manager
                .create(
                    input.prompt,
                    CreateOptions {
                        name: input.name,
                        harness: input.harness,
                        model: input.model,
                        cwd: input.cwd,
                    },
                    self.depth,
                    cancellation(context),
                    contract.as_ref(),
                )
                .await,
        )
    }
}

#[async_trait]
impl Tool for PromptTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: Continuation = serde_json::from_value(request.input.clone())
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let contract = input.output_schema.map(OutputContract::new).transpose()?;
        result(
            request,
            self.manager
                .prompt(
                    input.subagent,
                    input.prompt,
                    cancellation(context),
                    contract.as_ref(),
                )
                .await,
        )
    }
}

#[async_trait]
impl Tool for ForkTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: ForkInput = serde_json::from_value(request.input.clone())
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let contract = input.output_schema.map(OutputContract::new).transpose()?;
        result(
            request,
            self.manager
                .fork(
                    input.subagent,
                    input.prompt,
                    input.name,
                    self.depth,
                    cancellation(context),
                    contract.map(Arc::new),
                )
                .await,
        )
    }
}

#[cfg(test)]
#[path = "subagent/tests.rs"]
mod tests;
