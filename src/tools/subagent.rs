use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use agentkit_core::{ToolOutput, ToolResultPart, TurnCancellation};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};

const MAX_LIVE_SUBAGENTS: usize = 120;

pub const DEFAULT_SUBAGENT_NAMES: &[&str] = &[
    "Scout", "Pip", "Juniper", "Miso", "Clover", "Pixel", "Pebble", "Nova",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentNames {
    names: Arc<[String]>,
}

impl SubagentNames {
    pub fn resolve(configured: Option<Vec<String>>) -> Result<Self, String> {
        let values = configured.unwrap_or_else(|| {
            DEFAULT_SUBAGENT_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect()
        });
        let mut names = Vec::with_capacity(values.len());
        let mut normalized = HashSet::with_capacity(values.len());

        for source in values {
            let name = source.trim();
            if name.is_empty() {
                return Err(format!("subagent name {source:?} must not be empty"));
            }
            if name.chars().any(char::is_control) {
                return Err(format!(
                    "subagent name {source:?} must not contain control characters"
                ));
            }
            if name.chars().count() > 32 {
                return Err(format!(
                    "subagent name {source:?} must be at most 32 characters"
                ));
            }
            let key = name.to_lowercase();
            if !normalized.insert(key) {
                return Err(format!("duplicate subagent name {name:?}"));
            }
            names.push(name.to_string());
        }

        Ok(Self {
            names: names.into(),
        })
    }

    pub fn as_slice(&self) -> &[String] {
        &self.names
    }
}

impl Default for SubagentNames {
    fn default() -> Self {
        Self::resolve(None).expect("built-in subagent names are valid")
    }
}

fn allocate_name(
    pool: &[String],
    used: &mut HashSet<String>,
    fallback_name: Option<&str>,
) -> String {
    for name in pool {
        if reserve_name(used, name) {
            return name.clone();
        }
    }

    if let Some(base) = fallback_name.and_then(normalize_fallback_name) {
        for suffix in 1usize.. {
            let name = suffixed_name(&base, suffix);
            if reserve_name(used, &name) {
                return name;
            }
        }
    }

    for number in 1usize.. {
        let name = format!("Agent {number}");
        if reserve_name(used, &name) {
            return name;
        }
    }
    unreachable!("generated subagent name space is inexhaustible")
}

fn reserve_name(used: &mut HashSet<String>, name: &str) -> bool {
    used.insert(name.to_lowercase())
}

fn normalize_fallback_name(value: &str) -> Option<String> {
    if value.chars().any(char::is_control) {
        return None;
    }
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty() && normalized.chars().count() <= 32).then_some(normalized)
}

fn suffixed_name(base: &str, suffix: usize) -> String {
    if suffix == 1 {
        return base.to_string();
    }
    let suffix = format!(" {suffix}");
    let keep = 32usize.saturating_sub(suffix.chars().count());
    format!("{}{}", base.chars().take(keep).collect::<String>(), suffix)
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
    names: SubagentNames,
    sessions: Arc<Mutex<HashMap<String, SessionEntry>>>,
    capacity: Arc<Semaphore>,
    event_sink: EventSink,
    #[cfg(test)]
    failed_removals: Arc<Mutex<Vec<FailedRemoval>>>,
    #[cfg(test)]
    runtime_events: Arc<Mutex<Vec<events::RuntimeEvent>>>,
}

struct SessionEntry {
    name: String,
    state: Arc<AsyncMutex<State>>,
}

#[cfg(test)]
#[derive(Clone, Debug)]
struct FailedRemoval {
    status: SubagentStatus,
    outcome: Option<GenerationOutcome>,
    finished_at_unix_ms: Option<u64>,
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
    child: Option<ChildSession>,
    _permit: OwnedSemaphorePermit,
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

impl Subagents {
    pub(crate) fn new(config: ChildConfig, max_depth: usize) -> Self {
        Self::with_names(config, max_depth, SubagentNames::default())
    }

    pub(crate) fn with_names(config: ChildConfig, max_depth: usize, names: SubagentNames) -> Self {
        Self {
            config,
            max_depth,
            names,
            sessions: Arc::default(),
            capacity: Arc::new(Semaphore::new(MAX_LIVE_SUBAGENTS)),
            event_sink: Arc::new(|event| {
                events::emit(event);
                Ok(())
            }),
            #[cfg(test)]
            failed_removals: Arc::default(),
            #[cfg(test)]
            runtime_events: Arc::default(),
        }
    }

    pub(crate) fn child_config(&self) -> ChildConfig {
        self.config.clone()
    }

    pub(crate) fn fresh(&self) -> Self {
        Self::with_names(self.config.clone(), self.max_depth, self.names.clone())
    }

    pub(crate) fn names_policy(&self) -> SubagentNames {
        self.names.clone()
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
        #[cfg(test)]
        if let Ok(mut emitted) = self.runtime_events.lock() {
            emitted.push(event.clone());
        }
        let _ = (self.event_sink)(&event);
    }

    #[cfg(test)]
    fn runtime_events_for_test(&self) -> Vec<events::RuntimeEvent> {
        self.runtime_events.lock().unwrap().clone()
    }

    #[cfg(test)]
    fn with_event_sink_for_test(mut self, event_sink: EventSink) -> Self {
        self.event_sink = event_sink;
        self
    }

    #[cfg(test)]
    pub(crate) fn subagent_names(&self) -> &[String] {
        self.names.as_slice()
    }

    #[cfg(test)]
    async fn insert_starting_for_test(&self, fallback_name: Option<&str>) -> String {
        let now = events::now_millis();
        let state = self
            .insert_starting(
                session::new_id(),
                fallback_name,
                State {
                    name: String::new(),
                    status: SubagentStatus::Starting,
                    task: "test".into(),
                    generation: 1,
                    handle_generation: 1,
                    outcome: None,
                    created_at_unix_ms: now,
                    generation_started_at_unix_ms: now,
                    generation_finished_at_unix_ms: None,
                    output: Value::Null,
                    updates: None,
                    harness: crate::acp_child::BUILTIN_HARNESS.into(),
                    model: None,
                    kit: true,
                    child: None,
                    _permit: self.reserve().unwrap(),
                },
            )
            .unwrap();
        state.lock().await.name.clone()
    }

    fn harness_references(&self) -> Vec<String> {
        self.config.harnesses.references()
    }

    async fn create(
        &self,
        prompt: String,
        harness: Option<String>,
        model: Option<String>,
        fallback_name: Option<String>,
        depth: usize,
        cancellation: TurnCancellation,
        contract: Option<&OutputContract>,
    ) -> Result<SubagentValue, ChildError> {
        self.check_depth(depth)?;
        let permit = self.reserve()?;
        let id = session::new_id();
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
            fallback_name.as_deref(),
            State {
                name: String::new(),
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
                child: None,
                _permit: permit,
            },
        )?;
        let persisted = kit.then(|| (id.clone(), false));
        let child_config = self
            .config
            .clone()
            .with_parent_context(id.clone(), state.lock().await.name.clone());
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
                Err(error)
            }
        }
    }

    async fn fork(
        &self,
        prior: SubagentValue,
        prompt: String,
        fallback_name: Option<String>,
        depth: usize,
        cancellation: TurnCancellation,
        contract: Option<&OutputContract>,
    ) -> Result<SubagentValue, ChildError> {
        self.check_depth(depth)?;
        let permit = self.reserve()?;
        let source_state = self.lookup(&prior)?;
        let source = tokio::select! {
            source = source_state.lock() => source,
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        };
        self.check_ready(&source)?;
        self.check_generation(&prior, source.handle_generation)?;
        let id = session::new_id();
        let harness = source.harness.clone();
        let model = source.model.clone();
        let kit = source.kit;
        let source_child = source
            .child
            .clone()
            .ok_or_else(|| ChildError::Failed("subagent session is still starting".into()))?;
        let native_fork = source_child.supports_native_fork();
        if !native_fork && kit {
            session::clone_completed(&self.config.root, &prior.id, &id)
                .map_err(ChildError::Failed)?;
        } else if !native_fork {
            return Err(ChildError::Failed(format!(
                "ACP harness {harness:?} does not advertise session/fork; transcript fallback is only available for Kit"
            )));
        }
        let generation = source
            .generation
            .checked_add(1)
            .ok_or_else(|| ChildError::Failed("subagent generation overflow".into()))?;
        drop(source);
        let now = events::now_millis();
        let state = self.insert_starting(
            id.clone(),
            fallback_name.as_deref(),
            State {
                name: String::new(),
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
                child: None,
                _permit: permit,
            },
        )?;
        let child_config = self
            .config
            .clone()
            .with_parent_context(id.clone(), state.lock().await.name.clone());
        let child_result = if native_fork {
            source_child.fork(model.as_deref(), &cancellation).await
        } else {
            ChildSession::start(
                child_config,
                harness,
                Some((id.clone(), true)),
                model,
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
        {
            let mut locked = state.lock().await;
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

    #[cfg(test)]
    async fn clone_for_fork(
        &self,
        prior: &SubagentValue,
        cancellation: &TurnCancellation,
        directory: &std::path::Path,
    ) -> Result<String, ChildError> {
        let source = self.lookup(prior)?;
        let source = tokio::select! {
            source = source.lock() => source,
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        };
        self.check_ready(&source)?;
        self.check_generation(prior, source.handle_generation)?;
        let id = session::new_id();
        session::clone_completed_in(&self.config.root, directory, &prior.id, &id)
            .map_err(ChildError::Failed)?;
        // This is the stable snapshot boundary. Returning drops the source
        // guard before child startup or the branch prompt can begin.
        drop(source);
        Ok(id)
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
        let previous_status = locked.status;
        locked.status = SubagentStatus::Removed;
        let child = locked.child.take();
        let event = locked.runtime_event(id.to_string());
        drop(locked);

        let result = match &child {
            Some(child) => child.close().await,
            None => Ok(()),
        };
        match result {
            Ok(()) => {
                self.remove_if_same(id, &state);
                self.emit_event(event);
                Ok(())
            }
            Err(error) => {
                let terminal = child
                    .as_ref()
                    .is_none_or(|child| child_error_is_terminal(&error, child));
                if terminal || previous_status != SubagentStatus::Idle {
                    self.remove_if_same(id, &state);
                    self.emit_event(event);
                } else {
                    let mut locked = state.lock().await;
                    if locked.status == SubagentStatus::Removed {
                        locked.status = previous_status;
                        locked.child = child;
                    }
                }
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
        fallback_name: Option<&str>,
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
        let name = allocate_name(self.names.as_slice(), &mut used, fallback_name);
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

    fn monitor_child_exit(&self, id: String, state: &Arc<AsyncMutex<State>>, child: &ChildSession) {
        let manager = self.clone();
        let state = Arc::downgrade(state);
        let mut closed = child.closed_signal();
        tokio::spawn(async move {
            if !*closed.borrow() {
                let _ = closed.changed().await;
            }
            if let Some(state) = state.upgrade() {
                manager.retire_closed_child(id, state).await;
            }
        });
    }

    async fn retire_closed_child(&self, id: String, state: Arc<AsyncMutex<State>>) {
        let mut locked = state.lock().await;
        if locked.status == SubagentStatus::Removed {
            return;
        }
        if locked.status != SubagentStatus::Idle {
            locked.outcome = Some(GenerationOutcome::Failed);
        }
        locked.status = SubagentStatus::Removed;
        locked
            .generation_finished_at_unix_ms
            .get_or_insert_with(events::now_millis);
        let event = locked.runtime_event(id.clone());
        drop(locked);
        self.remove_if_same(&id, &state);
        drop(state);
        self.emit_event(event);
    }

    async fn fail_removed_and_remove(&self, id: &str, state: &Arc<AsyncMutex<State>>) {
        let mut locked = state.lock().await;
        if locked.status == SubagentStatus::Removed {
            return;
        }
        locked.status = SubagentStatus::Removed;
        locked.outcome = Some(GenerationOutcome::Failed);
        locked.generation_finished_at_unix_ms = Some(events::now_millis());
        #[cfg(test)]
        if let Ok(mut transitions) = self.failed_removals.lock() {
            transitions.push(FailedRemoval {
                status: locked.status,
                outcome: locked.outcome,
                finished_at_unix_ms: locked.generation_finished_at_unix_ms,
            });
        }
        let event = locked.runtime_event(id.to_string());
        drop(locked);
        self.remove_if_same(id, state);
        self.emit_event(event);
    }

    #[cfg(test)]
    fn failed_removals_for_test(&self) -> Vec<FailedRemoval> {
        self.failed_removals.lock().unwrap().clone()
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

fn listing_id(value: &SubagentListing) -> &str {
    &value.id
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
fn fork_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"subagent":value_schema(),"prompt":{"type":"string"},"fallback_name":{"type":"string"},"output_schema":{"oneOf":[{"type":"object"},{"type":"boolean"}]}},"required":["subagent","prompt"],"additionalProperties":false})
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
        Self { manager, depth, spec: ToolSpec::new(ToolName::new("subagent"), "Start a parent-owned configured ACP harness, prompt it, and return its reusable session value. Provide a short whimsical `fallback_name`; Kit uses it only after the configured name pool is exhausted. Omit `harness` and `model` unless the user or active workflow explicitly supplies the exact override or a configured alias. Never choose an override based on your own model, provider, publisher, familiarity, cost, or perceived quality; advertised choices indicate availability, not preference.", json!({"type":"object","properties":{"prompt":{"type":"string"},"harness":{"type":"string","enum":harnesses,"description":"Override the user's configured harness preference with this value. Default to omitting it."},"model":{"type":"string","minLength":1,"description":"Exact ACP model selection ID or configured alias explicitly requested by the user or active workflow. Applies only to this new session; default to omitting it."},"fallback_name":{"type":"string","description":"A short whimsical fallback name used only after the configured name pool is exhausted."},"output_schema":{"oneOf":[{"type":"object"},{"type":"boolean"}]}},"required":["prompt"],"additionalProperties":false})).with_output_schema(value_schema()).with_annotations(ToolAnnotations::new()) }
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
        Self { manager, depth, spec: ToolSpec::new(ToolName::new("fork"), "Fork a completed ACP subagent session using native capability support or the isolated Kit fallback, prompt it, and return the new session value. Provide a short whimsical `fallback_name`; Kit uses it only after the configured name pool is exhausted.", fork_schema()).with_output_schema(value_schema()).with_annotations(ToolAnnotations::new()) }
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
    harness: Option<String>,
    model: Option<String>,
    fallback_name: Option<String>,
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
    fallback_name: Option<String>,
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
                    input.harness,
                    input.model,
                    input.fallback_name,
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
                    input.fallback_name,
                    self.depth,
                    cancellation(context),
                    contract.as_ref(),
                )
                .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use agentkit_core::{Item, ItemKind};

    use super::*;

    #[test]
    fn configured_subagent_names_use_defaults_or_explicit_replacement() {
        let defaults = SubagentNames::resolve(None).expect("default names");
        assert_eq!(
            defaults.as_slice(),
            DEFAULT_SUBAGENT_NAMES
                .iter()
                .copied()
                .map(String::from)
                .collect::<Vec<_>>()
        );

        let names = SubagentNames::resolve(Some(vec!["  Acorn  ".into(), "Moss".into()]))
            .expect("valid names");
        assert_eq!(names.as_slice(), &["Acorn", "Moss"]);
        assert!(
            SubagentNames::resolve(Some(Vec::new()))
                .expect("empty pool")
                .as_slice()
                .is_empty()
        );
    }

    #[test]
    fn configured_subagent_names_reject_invalid_values() {
        let too_long = "x".repeat(33);
        for invalid in ["   ", "line\nbreak", "control\u{7}", too_long.as_str()] {
            let error = SubagentNames::resolve(Some(vec![invalid.into()]))
                .expect_err("invalid configured name must fail");
            let identified = format!("{invalid:?}");
            assert!(
                error.contains(&identified),
                "{error:?} did not identify {invalid:?}"
            );
        }
    }

    #[test]
    fn configured_subagent_names_reject_case_insensitive_duplicates() {
        let error = SubagentNames::resolve(Some(vec!["Scout".into(), "scout".into()]))
            .expect_err("case-insensitive duplicate must fail");
        assert!(error.contains("scout"));
    }

    #[test]
    fn name_allocation_uses_pool_order_then_reuses_released_names() {
        let pool = vec!["Scout".into(), "Pip".into()];
        let mut used = HashSet::new();

        assert_eq!(allocate_name(&pool, &mut used, Some("Waffles")), "Scout");
        assert_eq!(allocate_name(&pool, &mut used, Some("Waffles")), "Pip");
        assert_eq!(allocate_name(&pool, &mut used, Some("Waffles")), "Waffles");
        used.remove("scout");
        assert_eq!(allocate_name(&pool, &mut used, None), "Scout");
    }

    #[test]
    fn name_allocation_normalizes_fallbacks_and_generates_collision_safe_names() {
        let mut used = HashSet::from(["waffles".into(), "agent 1".into()]);
        assert_eq!(
            allocate_name(&[], &mut used, Some("  Waffles   McGee  ")),
            "Waffles McGee"
        );
        assert_eq!(allocate_name(&[], &mut used, Some("Waffles")), "Waffles 2");
        assert_eq!(
            allocate_name(&[], &mut used, Some("line\nbreak")),
            "Agent 2"
        );
        assert_eq!(allocate_name(&[], &mut used, Some("   ")), "Agent 3");
    }

    #[test]
    fn name_allocation_truncates_suffixed_fallbacks_to_32_chars() {
        let base = "🦀".repeat(32);
        let mut used = HashSet::from([base.to_lowercase()]);
        let allocated = allocate_name(&[], &mut used, Some(&base));

        assert_eq!(allocated.chars().count(), 32);
        assert!(allocated.ends_with(" 2"));
    }

    #[test]
    fn name_allocation_is_unique_when_siblings_allocate_concurrently() {
        let used = Arc::new(Mutex::new(HashSet::new()));
        let names = Arc::new(vec!["Scout".into(), "Pip".into()]);
        let threads = (0..8)
            .map(|_| {
                let used = Arc::clone(&used);
                let names = Arc::clone(&names);
                std::thread::spawn(move || {
                    allocate_name(&names, &mut used.lock().unwrap(), Some("Waffles"))
                })
            })
            .collect::<Vec<_>>();
        let allocated = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<HashSet<_>>();

        assert_eq!(allocated.len(), 8);
    }

    #[tokio::test]
    async fn manager_name_allocation_is_atomic_across_concurrent_insertions() {
        let root = tempfile::tempdir().unwrap();
        let base = manager_with_generic_harness(root.path(), vec!["--no-fork".into()]);
        let manager = Subagents::with_names(
            base.child_config(),
            2,
            SubagentNames::resolve(Some(vec!["Scout".into()])).unwrap(),
        );
        let starts = (0..8)
            .map(|_| {
                let manager = manager.clone();
                tokio::spawn(async move { manager.insert_starting_for_test(Some("Waffles")).await })
            })
            .collect::<Vec<_>>();
        let mut allocated = HashSet::new();
        for start in starts {
            allocated.insert(start.await.unwrap());
        }

        assert_eq!(allocated.len(), 8);
        assert!(allocated.contains("Scout"));
        assert!(allocated.contains("Waffles"));
        assert!(allocated.contains("Waffles 2"));
        assert_eq!(manager.sessions.lock().unwrap().len(), 8);
    }

    #[test]
    fn name_allocation_uses_generated_names_for_an_explicit_empty_pool() {
        assert_eq!(allocate_name(&[], &mut HashSet::new(), None), "Agent 1");
    }

    #[test]
    fn name_allocation_summarizes_tasks_as_single_bounded_lines() {
        assert_eq!(task_summary("  trace\n\t the   flow  "), "trace the flow");
        assert_eq!(task_summary(" \n\t "), "Untitled task");
        let summary = task_summary(&"é".repeat(97));
        assert_eq!(summary.chars().count(), 96);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn subagent_value_reads_legacy_and_current_handles_and_rejects_malformed_shapes() {
        let legacy = serde_json::from_value::<SubagentValue>(json!({
            "id": "s-old", "output": null, "generation": 1
        }))
        .expect("legacy handle remains readable");
        assert_eq!(legacy.name, None);

        let current = serde_json::from_value::<SubagentValue>(json!({
            "id": "s-current", "name": "Scout", "output": "done", "generation": 2
        }))
        .expect("current handle remains readable");
        assert_eq!(current.name.as_deref(), Some("Scout"));
        assert!(
            serde_json::from_value::<SubagentValue>(json!({
                "id": "s-bad", "name": 42, "output": null, "generation": 1
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<SubagentValue>(json!({
                "id": "s-bad", "output": null, "generation": 1, "extra": true
            }))
            .is_err()
        );
    }

    #[test]
    fn subagent_value_schema_accepts_legacy_and_current_handles() {
        let validator = jsonschema::validator_for(&value_schema()).unwrap();
        assert!(validator.is_valid(&json!({
            "id": "s-old", "output": null, "generation": 1
        })));
        assert!(validator.is_valid(&json!({
            "id": "s-new", "name": "Scout", "output": null, "generation": 1
        })));
        assert!(!validator.is_valid(&json!({
            "id": "s-bad", "name": 7, "output": null, "generation": 1
        })));
    }

    #[test]
    fn fallback_name_is_accepted_only_for_subagent_and_fork_inputs() {
        let input = serde_json::from_value::<Input>(json!({
            "prompt": "work", "fallback_name": "Waffles"
        }))
        .expect("subagent accepts fallback_name");
        assert_eq!(input.fallback_name.as_deref(), Some("Waffles"));

        let fork = serde_json::from_value::<ForkInput>(json!({
            "subagent": {"id": "s", "output": null, "generation": 1},
            "prompt": "fork work",
            "fallback_name": "Mochi"
        }))
        .expect("fork accepts fallback_name");
        assert_eq!(fork.fallback_name.as_deref(), Some("Mochi"));

        assert!(
            serde_json::from_value::<Continuation>(json!({
                "subagent": {"id": "s", "output": null, "generation": 1},
                "prompt": "continue",
                "fallback_name": "not allowed"
            }))
            .is_err()
        );
    }

    #[test]
    fn structured_output_is_parsed_and_validated() {
        let contract = OutputContract::new(json!({
            "type": "object",
            "properties": {"approved": {"type": "boolean"}},
            "required": ["approved"],
            "additionalProperties": false
        }))
        .unwrap();

        assert_eq!(
            contract.parse(r#"{"approved":true}"#).unwrap(),
            json!({"approved": true})
        );
        assert!(contract.parse(r#"{"approved":"yes"}"#).is_none());
        assert!(contract.parse("```json\n{}\n```").is_none());
    }

    #[test]
    fn invalid_output_schema_is_rejected() {
        assert!(OutputContract::new(json!({"type": 42})).is_err());
    }

    #[test]
    fn explicit_null_output_schema_is_rejected() {
        assert!(
            serde_json::from_value::<Input>(json!({"prompt": "test", "output_schema": null}))
                .is_err()
        );
    }

    #[test]
    fn boolean_output_schema_is_supported() {
        let contract = OutputContract::new(Value::Bool(true)).unwrap();
        assert_eq!(contract.parse("[1, 2]").unwrap(), json!([1, 2]));
    }

    #[test]
    fn unstructured_output_remains_text() {
        assert_eq!(
            structured_output("plain text".into(), None),
            Value::String("plain text".into())
        );
    }

    #[test]
    fn text_only_values_keep_the_existing_json_shape() {
        let value = SubagentValue {
            id: "child".into(),
            name: None,
            output: Value::String("done".into()),
            generation: 1,
            updates: None,
        };

        assert_eq!(
            serde_json::to_value(value).unwrap(),
            json!({"id": "child", "output": "done", "generation": 1})
        );
    }

    #[tokio::test]
    async fn fork_snapshot_releases_source_before_child_work() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = vec![Item::text(ItemKind::System, "system")];
        let sessions = directory.path().join("sessions");
        let opened = session::open_in(
            directory.path(),
            &sessions,
            "source",
            false,
            false,
            transcript,
        )
        .unwrap();
        let manager = Subagents::new(
            ChildConfig {
                root: directory.path().to_path_buf(),
                model: "test".into(),
                provider: Default::default(),
                reasoning_effort: None,
                openrouter_api_key: None,
                mcp_config: None,
                credential_storage: Default::default(),
                telemetry: Default::default(),
                harnesses: Default::default(),
                default_harness: crate::acp_child::BUILTIN_HARNESS.into(),
                parent_id: None,
                parent_name: None,
            },
            2,
        );
        let state = Arc::new(AsyncMutex::new(State {
            name: "Scout".into(),
            status: SubagentStatus::Idle,
            task: "done".into(),
            generation: 1,
            handle_generation: 1,
            outcome: Some(GenerationOutcome::Success),
            created_at_unix_ms: 1,
            generation_started_at_unix_ms: 1,
            generation_finished_at_unix_ms: Some(2),
            output: Value::String("done".into()),
            updates: None,
            harness: crate::acp_child::BUILTIN_HARNESS.into(),
            model: None,
            kit: true,
            child: Some(ChildSession::disconnected_for_test()),
            _permit: Arc::clone(&manager.capacity).try_acquire_owned().unwrap(),
        }));
        manager.sessions.lock().unwrap().insert(
            "source".into(),
            SessionEntry {
                name: "Scout".into(),
                state: Arc::clone(&state),
            },
        );
        let prior = SubagentValue {
            id: "source".into(),
            name: Some("Scout".into()),
            output: Value::String("done".into()),
            generation: 1,
            updates: None,
        };

        let branch = manager
            .clone_for_fork(&prior, &TurnCancellation::default(), &sessions)
            .await
            .unwrap();

        assert!(
            state.try_lock().is_ok(),
            "source remained locked after snapshot"
        );
        assert!(session::load_in(directory.path(), &sessions, &branch).is_ok());
        drop(opened);
    }
    fn manager_with_disconnected_session(
        root: &Path,
    ) -> (Subagents, Arc<AsyncMutex<State>>, SubagentValue) {
        let manager = Subagents::new(
            ChildConfig {
                root: root.to_path_buf(),
                model: "test".into(),
                provider: Default::default(),
                reasoning_effort: None,
                openrouter_api_key: None,
                mcp_config: None,
                credential_storage: Default::default(),
                telemetry: Default::default(),
                harnesses: Default::default(),
                default_harness: crate::acp_child::BUILTIN_HARNESS.into(),
                parent_id: None,
                parent_name: None,
            },
            2,
        );
        let state = Arc::new(AsyncMutex::new(State {
            name: "Scout".into(),
            status: SubagentStatus::Idle,
            task: "done".into(),
            generation: 1,
            handle_generation: 1,
            outcome: Some(GenerationOutcome::Success),
            created_at_unix_ms: 1,
            generation_started_at_unix_ms: 1,
            generation_finished_at_unix_ms: Some(2),
            output: Value::String("done".into()),
            updates: None,
            harness: crate::acp_child::BUILTIN_HARNESS.into(),
            model: None,
            kit: true,
            child: Some(ChildSession::disconnected_for_test()),
            _permit: Arc::clone(&manager.capacity).try_acquire_owned().unwrap(),
        }));
        manager.sessions.lock().unwrap().insert(
            "source".into(),
            SessionEntry {
                name: "Scout".into(),
                state: Arc::clone(&state),
            },
        );
        let prior = SubagentValue {
            id: "source".into(),
            name: Some("Scout".into()),
            output: Value::String("done".into()),
            generation: 1,
            updates: None,
        };
        (manager, state, prior)
    }

    #[tokio::test]
    async fn close_does_not_block_listings_or_allow_stale_reuse() {
        let root = tempfile::tempdir().unwrap();
        let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
        let harnesses = crate::acp_child::AcpHarnesses::new(std::collections::BTreeMap::from([(
            "generic".into(),
            crate::acp_child::AcpHarnessProfile {
                command: "python3".into(),
                args: vec![fixture, "--slow-close".into()],
                permissions: Default::default(),
            },
        )]))
        .unwrap();
        let manager = Subagents::new(
            ChildConfig {
                root: root.path().to_path_buf(),
                model: "unused".into(),
                provider: Default::default(),
                reasoning_effort: None,
                openrouter_api_key: None,
                mcp_config: None,
                credential_storage: Default::default(),
                telemetry: Default::default(),
                harnesses,
                default_harness: "acp.generic".into(),
                parent_id: None,
                parent_name: None,
            },
            2,
        );
        let handle = manager
            .create(
                "base".into(),
                None,
                None,
                None,
                0,
                TurnCancellation::default(),
                None,
            )
            .await
            .unwrap();
        let close_manager = manager.clone();
        let close_id = handle.id.clone();
        let close = tokio::spawn(async move {
            close_manager
                .close(&close_id, &TurnCancellation::default())
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let listing = tokio::time::timeout(
            std::time::Duration::from_millis(150),
            manager.list(&TurnCancellation::default()),
        )
        .await
        .expect("listing blocked behind child close")
        .unwrap();
        assert!(listing.is_empty());
        let reuse = tokio::time::timeout(
            std::time::Duration::from_millis(150),
            manager.prompt(
                handle,
                "stale reuse".into(),
                TurnCancellation::default(),
                None,
            ),
        )
        .await
        .expect("reuse blocked behind child close");
        assert!(reuse.is_err());
        close.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn listing_omits_closed_subagents() {
        let directory = tempfile::tempdir().unwrap();
        let (manager, state, prior) = manager_with_disconnected_session(directory.path());
        let (child, closed) = ChildSession::closure_probe_for_test();
        state.lock().await.child = Some(child);
        drop(state);

        let cancellation = TurnCancellation::default();
        assert_eq!(manager.list(&cancellation).await.unwrap().len(), 1);
        assert_eq!(
            listing_id(&manager.list(&cancellation).await.unwrap()[0]),
            prior.id
        );
        manager.close(&prior.id, &cancellation).await.unwrap();
        assert!(manager.list(&cancellation).await.unwrap().is_empty());
        assert!(manager.lookup(&prior).is_err());
        tokio::time::timeout(std::time::Duration::from_secs(1), closed)
            .await
            .expect("closing did not terminate the child actor")
            .unwrap();
    }

    #[tokio::test]
    async fn dropping_a_session_manager_terminates_its_children() {
        let directory = tempfile::tempdir().unwrap();
        let (manager, state, _) = manager_with_disconnected_session(directory.path());
        let (child, closed) = ChildSession::closure_probe_for_test();
        state.lock().await.child = Some(child);
        drop(state);

        drop(manager);

        tokio::time::timeout(std::time::Duration::from_secs(1), closed)
            .await
            .expect("manager drop did not terminate the child actor")
            .unwrap();
    }

    #[test]
    fn close_input_accepts_a_handle_subagent_id_or_background_call_id() {
        let handle = json!({"id": "child", "output": "done", "generation": 1});
        let id = json!({"id": "child"});
        let call_id = json!({"call_id": "call_123"});
        assert_eq!(
            serde_json::from_value::<CloseInput>(handle)
                .unwrap()
                .target()
                .0,
            "child"
        );
        assert_eq!(
            serde_json::from_value::<CloseInput>(id).unwrap().target().0,
            "child"
        );
        assert_eq!(
            serde_json::from_value::<CloseInput>(call_id)
                .unwrap()
                .target(),
            ("call_123".into(), true)
        );
        assert!(
            serde_json::from_value::<CloseInput>(json!({"id": "child", "extra": true})).is_err()
        );
    }

    #[test]
    fn live_session_limit_is_120_per_manager() {
        let directory = tempfile::tempdir().unwrap();
        let (manager, _, prior) = manager_with_disconnected_session(directory.path());
        manager.sessions.lock().unwrap().remove(&prior.id);
        let permits = (0..MAX_LIVE_SUBAGENTS)
            .map(|_| manager.reserve().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(permits.len(), 120);
        assert_eq!(
            manager.reserve().unwrap_err().to_string(),
            "live subagent session limit (120) reached"
        );
    }

    #[test]
    fn invalid_structured_output_remains_recoverable_as_text() {
        let contract = OutputContract::new(json!({"type": "object"})).unwrap();

        assert_eq!(
            structured_output("not JSON".into(), Some(&contract)),
            Value::String("not JSON".into())
        );
    }

    fn manager_with_generic_harness(root: &Path, args: Vec<String>) -> Subagents {
        let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
        let harnesses = crate::acp_child::AcpHarnesses::new(std::collections::BTreeMap::from([(
            "generic".into(),
            crate::acp_child::AcpHarnessProfile {
                command: "python3".into(),
                args: std::iter::once(fixture).chain(args).collect(),
                permissions: Default::default(),
            },
        )]))
        .unwrap();
        Subagents::new(
            ChildConfig {
                root: root.to_path_buf(),
                model: "unused".into(),
                provider: Default::default(),
                reasoning_effort: None,
                openrouter_api_key: None,
                mcp_config: None,
                credential_storage: Default::default(),
                telemetry: Default::default(),
                harnesses,
                default_harness: "acp.generic".into(),
                parent_id: None,
                parent_name: None,
            },
            2,
        )
    }

    mod lifecycle_events {
        use super::*;

        fn transitions(manager: &Subagents) -> Vec<(SubagentStatus, Option<GenerationOutcome>)> {
            manager
                .runtime_events_for_test()
                .into_iter()
                .filter_map(|event| match event {
                    events::RuntimeEvent::SubagentStateChanged {
                        status, outcome, ..
                    } => Some((status, outcome)),
                    _ => None,
                })
                .collect()
        }

        #[tokio::test]
        async fn nested_runtime_events_include_only_the_immediate_parent() {
            let root = tempfile::tempdir().unwrap();
            let base = manager_with_generic_harness(root.path(), Vec::new());
            let mut config = base.child_config();
            config.parent_id = Some("s-parent".into());
            config.parent_name = Some("偵察 🦀".into());
            let manager = Subagents::new(config, 2);

            manager.insert_starting_for_test(None).await;

            assert!(matches!(
                manager.runtime_events_for_test().as_slice(),
                [events::RuntimeEvent::SubagentStateChanged {
                    parent_id: Some(parent_id),
                    parent_name: Some(parent_name),
                    ..
                }] if parent_id == "s-parent" && parent_name == "偵察 🦀"
            ));
        }

        #[tokio::test]
        async fn emits_committed_create_prompt_failure_and_close_transitions() {
            let root = tempfile::tempdir().unwrap();
            let manager = manager_with_generic_harness(root.path(), vec!["--no-fork".into()]);
            let handle = manager
                .create(
                    "base task".into(),
                    None,
                    None,
                    None,
                    0,
                    TurnCancellation::default(),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(
                transitions(&manager),
                vec![
                    (SubagentStatus::Starting, None),
                    (SubagentStatus::Working, None),
                    (SubagentStatus::Idle, Some(GenerationOutcome::Success)),
                ]
            );

            let handle = manager
                .prompt(
                    handle,
                    "successful continuation".into(),
                    TurnCancellation::default(),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(
                &transitions(&manager)[3..],
                &[
                    (SubagentStatus::Working, None),
                    (SubagentStatus::Idle, Some(GenerationOutcome::Success)),
                ]
            );

            let error = manager
                .prompt(
                    handle.clone(),
                    "MOCK_REFUSAL".into(),
                    TurnCancellation::default(),
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "nested agent refused the prompt");
            manager
                .close(&handle.id, &TurnCancellation::default())
                .await
                .unwrap();

            let emitted = manager.runtime_events_for_test();
            assert_eq!(
                &transitions(&manager)[5..],
                &[
                    (SubagentStatus::Working, None),
                    (SubagentStatus::Idle, Some(GenerationOutcome::Failed)),
                    (SubagentStatus::Removed, Some(GenerationOutcome::Failed)),
                ]
            );
            assert!(emitted.iter().all(|event| event.parent_call().is_none()));
            let generations = emitted
                .iter()
                .filter_map(|event| match event {
                    events::RuntimeEvent::SubagentStateChanged { generation, .. } => {
                        Some(*generation)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(generations, vec![1, 1, 1, 2, 2, 3, 3, 3]);
            assert!(
                matches!(emitted.last(), Some(events::RuntimeEvent::SubagentStateChanged { id, name, status: SubagentStatus::Removed, generation_finished_at_unix_ms: Some(_), .. }) if id.as_str() == handle.id.as_str() && name == "Scout")
            );
        }

        #[tokio::test]
        async fn failing_event_transport_does_not_change_subagent_operations() {
            use std::sync::atomic::{AtomicUsize, Ordering};

            let root = tempfile::tempdir().unwrap();
            let attempts = Arc::new(AtomicUsize::new(0));
            let sink_attempts = Arc::clone(&attempts);
            let manager = manager_with_generic_harness(root.path(), vec!["--no-fork".into()])
                .with_event_sink_for_test(Arc::new(move |_| {
                    sink_attempts.fetch_add(1, Ordering::Relaxed);
                    Err(())
                }));

            let handle = manager
                .create(
                    "create".into(),
                    None,
                    None,
                    None,
                    0,
                    TurnCancellation::default(),
                    None,
                )
                .await
                .expect("event failure must not fail create");
            let handle = manager
                .prompt(handle, "continue".into(), TurnCancellation::default(), None)
                .await
                .expect("event failure must not fail prompt");
            assert_eq!(
                manager.lookup(&handle).unwrap().lock().await.status,
                SubagentStatus::Idle
            );
            manager
                .close(&handle.id, &TurnCancellation::default())
                .await
                .expect("event failure must not fail close");
            assert!(manager.lookup(&handle).is_err());
            assert!(attempts.load(Ordering::Relaxed) >= 6);
        }

        #[tokio::test]
        async fn closed_sessions_emit_one_removed_transition_before_name_reuse() {
            for (working, expected_outcome) in [
                (false, Some(GenerationOutcome::Success)),
                (true, Some(GenerationOutcome::Failed)),
            ] {
                let root = tempfile::tempdir().unwrap();
                let (manager, state, prior) = manager_with_disconnected_session(root.path());
                if working {
                    let mut state = state.lock().await;
                    state.status = SubagentStatus::Working;
                    state.outcome = None;
                    state.generation_finished_at_unix_ms = None;
                }

                assert_eq!(manager.insert_starting_for_test(None).await, "Scout");
                let removed = manager
                    .runtime_events_for_test()
                    .into_iter()
                    .filter(|event| {
                        matches!(event, events::RuntimeEvent::SubagentStateChanged { id, status: SubagentStatus::Removed, .. } if id == &prior.id)
                    })
                    .collect::<Vec<_>>();
                assert_eq!(removed.len(), 1);
                assert!(matches!(
                    &removed[0],
                    events::RuntimeEvent::SubagentStateChanged { outcome, generation_finished_at_unix_ms: Some(_), .. } if *outcome == expected_outcome
                ));
                assert!(manager.lookup(&prior).is_err());
            }
        }

        #[tokio::test]
        async fn idle_child_exit_promptly_retires_the_direct_handle_once() {
            let root = tempfile::tempdir().unwrap();
            let manager =
                manager_with_generic_harness(root.path(), vec!["--exit-after-prompt".into()]);
            let handle = manager
                .create(
                    "finish before exiting".into(),
                    None,
                    None,
                    None,
                    0,
                    TurnCancellation::default(),
                    None,
                )
                .await
                .unwrap();

            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if transitions(&manager).last()
                        == Some(&(SubagentStatus::Removed, Some(GenerationOutcome::Success)))
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("idle child exit did not emit direct removal promptly");

            assert_eq!(
                transitions(&manager),
                vec![
                    (SubagentStatus::Starting, None),
                    (SubagentStatus::Working, None),
                    (SubagentStatus::Idle, Some(GenerationOutcome::Success)),
                    (SubagentStatus::Removed, Some(GenerationOutcome::Success)),
                ]
            );
            assert!(manager.lookup(&handle).is_err());
            assert_eq!(manager.capacity.available_permits(), MAX_LIVE_SUBAGENTS);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert_eq!(
                transitions(&manager)
                    .into_iter()
                    .filter(|(status, _)| *status == SubagentStatus::Removed)
                    .count(),
                1
            );
            assert_eq!(manager.insert_starting_for_test(None).await, "Scout");
        }

        #[tokio::test]
        async fn emits_removed_after_failed_creation_and_terminal_retirement() {
            let root = tempfile::tempdir().unwrap();
            let failed = manager_with_generic_harness(root.path(), vec!["--fail-start".into()]);
            assert!(
                failed
                    .create(
                        "create".into(),
                        None,
                        None,
                        None,
                        0,
                        TurnCancellation::default(),
                        None
                    )
                    .await
                    .is_err()
            );
            assert_eq!(
                transitions(&failed),
                vec![
                    (SubagentStatus::Starting, None),
                    (SubagentStatus::Removed, Some(GenerationOutcome::Failed)),
                ]
            );

            let (terminal, _, prior) = manager_with_disconnected_session(root.path());
            assert!(
                terminal
                    .prompt(prior, "continue".into(), TurnCancellation::default(), None)
                    .await
                    .is_err()
            );
            assert_eq!(
                transitions(&terminal),
                vec![
                    (SubagentStatus::Working, None),
                    (SubagentStatus::Removed, Some(GenerationOutcome::Failed)),
                ]
            );
        }
    }

    #[tokio::test]
    async fn failed_create_and_fork_startup_record_failed_removed_transitions() {
        let root = tempfile::tempdir().unwrap();
        let failed_create = manager_with_generic_harness(root.path(), vec!["--fail-start".into()]);
        assert!(
            failed_create
                .create(
                    "create".into(),
                    None,
                    None,
                    None,
                    0,
                    TurnCancellation::default(),
                    None,
                )
                .await
                .is_err()
        );
        let create_transitions = failed_create.failed_removals_for_test();
        assert_eq!(create_transitions.len(), 1);
        assert_eq!(create_transitions[0].status, SubagentStatus::Removed);
        assert_eq!(
            create_transitions[0].outcome,
            Some(GenerationOutcome::Failed)
        );
        assert!(create_transitions[0].finished_at_unix_ms.is_some());

        let failed_fork = manager_with_generic_harness(root.path(), vec!["--fail-fork".into()]);
        let source = failed_fork
            .create(
                "source".into(),
                None,
                None,
                None,
                0,
                TurnCancellation::default(),
                None,
            )
            .await
            .unwrap();
        assert!(
            failed_fork
                .fork(
                    source,
                    "fork".into(),
                    None,
                    0,
                    TurnCancellation::default(),
                    None,
                )
                .await
                .is_err()
        );
        let fork_transitions = failed_fork.failed_removals_for_test();
        assert_eq!(fork_transitions.len(), 1);
        assert_eq!(fork_transitions[0].status, SubagentStatus::Removed);
        assert_eq!(fork_transitions[0].outcome, Some(GenerationOutcome::Failed));
        assert!(fork_transitions[0].finished_at_unix_ms.is_some());
    }

    #[tokio::test]
    async fn terminal_child_errors_do_not_depend_on_channel_close_timing() {
        let (child, _) = ChildSession::closure_probe_for_test();
        assert!(child_error_is_terminal(
            &ChildError::TerminalCancelled,
            &child
        ));
        assert!(child_error_is_terminal(
            &ChildError::TerminalFailed("transport ended".into()),
            &child
        ));
    }

    #[tokio::test]
    async fn reusable_prompt_failure_remains_failed_idle_and_can_be_retried() {
        let root = tempfile::tempdir().unwrap();
        let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
        let harnesses = crate::acp_child::AcpHarnesses::new(std::collections::BTreeMap::from([(
            "generic".into(),
            crate::acp_child::AcpHarnessProfile {
                command: "python3".into(),
                args: vec![fixture, "--no-fork".into()],
                permissions: Default::default(),
            },
        )]))
        .unwrap();
        let manager = Subagents::new(
            ChildConfig {
                root: root.path().to_path_buf(),
                model: "unused".into(),
                provider: Default::default(),
                reasoning_effort: None,
                openrouter_api_key: None,
                mcp_config: None,
                credential_storage: Default::default(),
                telemetry: Default::default(),
                harnesses,
                default_harness: "acp.generic".into(),
                parent_id: None,
                parent_name: None,
            },
            2,
        );
        let handle = manager
            .create(
                "base".into(),
                None,
                None,
                None,
                0,
                TurnCancellation::default(),
                None,
            )
            .await
            .unwrap();

        let error = manager
            .prompt(
                handle.clone(),
                "MOCK_REFUSAL".into(),
                TurnCancellation::default(),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "nested agent refused the prompt");
        let state = manager
            .lookup(&handle)
            .expect("live child remains reusable");
        let state = state.lock().await;
        assert_eq!(state.status, SubagentStatus::Idle);
        assert_eq!(state.outcome, Some(GenerationOutcome::Failed));
        assert!(state.generation_finished_at_unix_ms.is_some());
        drop(state);

        let retried = manager
            .prompt(handle, "retry".into(), TurnCancellation::default(), None)
            .await
            .expect("failed reusable generation does not stale the last handle");
        assert_eq!(retried.generation, 3);
        assert_eq!(retried.name.as_deref(), Some("Scout"));
    }

    #[tokio::test]
    async fn listing_omits_terminally_retired_subagents() {
        let directory = tempfile::tempdir().unwrap();
        let (manager, _, prior) = manager_with_disconnected_session(directory.path());

        assert!(
            manager
                .prompt(
                    prior.clone(),
                    "continue".into(),
                    TurnCancellation::default(),
                    None,
                )
                .await
                .is_err()
        );
        assert!(manager.lookup(&prior).is_err());
        assert!(
            manager
                .list(&TurnCancellation::default())
                .await
                .unwrap()
                .is_empty()
        );
    }
    #[tokio::test]
    async fn listing_includes_named_starting_and_idle_subagents() {
        let root = tempfile::tempdir().unwrap();
        let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
        let harnesses = crate::acp_child::AcpHarnesses::new(std::collections::BTreeMap::from([(
            "generic".into(),
            crate::acp_child::AcpHarnessProfile {
                command: "python3".into(),
                args: vec![fixture, "--no-fork".into()],
                permissions: Default::default(),
            },
        )]))
        .unwrap();
        let manager = Subagents::new(
            ChildConfig {
                root: root.path().to_path_buf(),
                model: "unused".into(),
                provider: Default::default(),
                reasoning_effort: None,
                openrouter_api_key: None,
                mcp_config: None,
                credential_storage: Default::default(),
                telemetry: Default::default(),
                harnesses,
                default_harness: "acp.generic".into(),
                parent_id: None,
                parent_name: None,
            },
            2,
        );
        let create_manager = manager.clone();
        let create = tokio::spawn(async move {
            create_manager
                .create(
                    "first prompt".into(),
                    None,
                    None,
                    None,
                    0,
                    TurnCancellation::default(),
                    None,
                )
                .await
        });

        let listing = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(listing) = manager
                    .list(&TurnCancellation::default())
                    .await
                    .unwrap()
                    .into_iter()
                    .next()
                {
                    break listing;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("starting subagent was not registered");
        assert_eq!(listing.name, "Scout");
        assert_eq!(listing.status, SubagentStatus::Starting);
        assert_eq!(listing.generation, 1);
        assert_eq!(listing.task, "first prompt");

        let completed = create.await.unwrap().unwrap();
        assert_eq!(completed.name.as_deref(), Some("Scout"));
        let listings = manager.list(&TurnCancellation::default()).await.unwrap();
        assert_eq!(listings[0].id, completed.id);
        assert_eq!(listings[0].name, "Scout");
        assert_eq!(listings[0].status, SubagentStatus::Idle);
        assert_eq!(listings[0].generation, 1);
        assert_eq!(listings[0].task, "first prompt");
        assert_eq!(
            serde_json::to_value(&listings[0]).unwrap(),
            json!({
                "id": completed.id,
                "name": "Scout",
                "status": "idle",
                "generation": 1,
                "task": "first prompt"
            })
        );

        let mut informational_name = completed.clone();
        informational_name.name = Some("Imposter".into());
        let prompt_manager = manager.clone();
        let continued = tokio::spawn(async move {
            prompt_manager
                .prompt(
                    informational_name,
                    "second prompt".into(),
                    TurnCancellation::default(),
                    None,
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let listing = manager.list(&TurnCancellation::default()).await.unwrap();
                if listing[0].status == SubagentStatus::Working {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("working continuation was not listable");
        let continued = continued.await.unwrap().unwrap();
        assert_eq!(continued.name.as_deref(), Some("Scout"));
        let listing = manager.list(&TurnCancellation::default()).await.unwrap();
        assert_eq!(listing[0].generation, 2);
        assert_eq!(listing[0].task, "second prompt");
    }

    #[tokio::test]
    async fn generic_harness_without_native_fork_returns_unsupported() {
        let root = tempfile::tempdir().unwrap();
        let fixture = format!("{}/fixtures/mock-acp.py", env!("CARGO_MANIFEST_DIR"));
        let harnesses = crate::acp_child::AcpHarnesses::new(std::collections::BTreeMap::from([(
            "generic".into(),
            crate::acp_child::AcpHarnessProfile {
                command: "python3".into(),
                args: vec![fixture, "--no-fork".into()],
                permissions: Default::default(),
            },
        )]))
        .unwrap();
        let manager = Subagents::new(
            ChildConfig {
                root: root.path().to_path_buf(),
                model: "unused".into(),
                provider: Default::default(),
                reasoning_effort: None,
                openrouter_api_key: None,
                mcp_config: None,
                credential_storage: Default::default(),
                telemetry: Default::default(),
                harnesses,
                default_harness: "acp.generic".into(),
                parent_id: None,
                parent_name: None,
            },
            2,
        );
        let prior = manager
            .create(
                "base".into(),
                None,
                None,
                None,
                0,
                TurnCancellation::default(),
                None,
            )
            .await
            .unwrap();

        let error = manager
            .fork(
                prior,
                "branch".into(),
                None,
                0,
                TurnCancellation::default(),
                None,
            )
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "ACP harness \"acp.generic\" does not advertise session/fork; transcript fallback is only available for Kit"
        );
    }
}
