use std::{
    collections::HashMap,
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

use crate::{
    acp_child::{ChildConfig, ChildError, ChildOutput, ChildSession},
    session,
};

#[derive(Clone)]
pub struct Subagents {
    config: ChildConfig,
    max_depth: usize,
    sessions: Arc<Mutex<HashMap<String, Arc<AsyncMutex<State>>>>>,
    capacity: Arc<Semaphore>,
}

struct State {
    generation: u64,
    output: Value,
    updates: Option<SubagentUpdates>,
    retired: bool,
    harness: String,
    kit: bool,
    child: ChildSession,
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentValue {
    pub id: String,
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
        Self {
            config,
            max_depth,
            sessions: Arc::default(),
            capacity: Arc::new(Semaphore::new(MAX_LIVE_SUBAGENTS)),
        }
    }

    pub(crate) fn child_config(&self) -> ChildConfig {
        self.config.clone()
    }

    pub(crate) fn fresh(&self) -> Self {
        Self::new(self.config.clone(), self.max_depth)
    }

    fn harness_references(&self) -> Vec<String> {
        self.config.harnesses.references()
    }

    async fn create(
        &self,
        prompt: String,
        harness: Option<String>,
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
        let kit = self.config.harnesses.is_kit(&harness);
        let persisted = kit.then(|| (id.clone(), false));
        let child = ChildSession::start(
            self.config.clone(),
            harness.clone(),
            persisted,
            depth + 1,
            cancellation.clone(),
        )
        .await?;
        let output = child
            .prompt(structured_prompt(prompt, contract), cancellation)
            .await?;
        let (output, updates) = turn_output(output, contract);
        self.insert(
            id.clone(),
            State {
                generation: 1,
                output: output.clone(),
                updates: updates.clone(),
                retired: false,
                harness,
                kit,
                child,
                _permit: permit,
            },
        )?;
        Ok(SubagentValue {
            id,
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
        self.check_active(&locked)?;
        self.check_generation(&prior, locked.generation)?;
        let generation = locked
            .generation
            .checked_add(1)
            .ok_or_else(|| ChildError::Failed("subagent generation overflow".into()))?;
        match locked
            .child
            .prompt(structured_prompt(prompt, contract), cancellation)
            .await
        {
            Ok(output) => {
                let (output, updates) = turn_output(output, contract);
                locked.generation = generation;
                locked.output.clone_from(&output);
                locked.updates.clone_from(&updates);
                Ok(SubagentValue {
                    id: prior.id,
                    output,
                    generation,
                    updates,
                })
            }
            Err(error) => {
                // Any dispatched unsuccessful turn may have changed the durable
                // transcript. Retire it rather than accepting the old generation.
                locked.retired = true;
                drop(locked);
                self.remove_if_same(&prior.id, &state);
                Err(error)
            }
        }
    }

    async fn fork(
        &self,
        prior: SubagentValue,
        prompt: String,
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
        self.check_active(&source)?;
        self.check_generation(&prior, source.generation)?;
        let id = session::new_id();
        let harness = source.harness.clone();
        let kit = source.kit;
        let child = if source.child.supports_native_fork() {
            let child = source.child.fork(&cancellation).await?;
            drop(source);
            child
        } else if kit {
            session::clone_completed(&self.config.root, &prior.id, &id)
                .map_err(ChildError::Failed)?;
            // The clone is the stable snapshot boundary. Do not keep the source
            // registry lock across child startup or the branch prompt.
            drop(source);
            ChildSession::start(
                self.config.clone(),
                harness.clone(),
                Some((id.clone(), true)),
                depth + 1,
                cancellation.clone(),
            )
            .await?
        } else {
            return Err(ChildError::Failed(format!(
                "ACP harness {harness:?} does not advertise session/fork; transcript fallback is only available for Kit"
            )));
        };
        let output = match child
            .prompt(structured_prompt(prompt, contract), cancellation)
            .await
        {
            Ok(output) => output,
            Err(error) => {
                let _ = child.close().await;
                return Err(error);
            }
        };
        let (output, updates) = turn_output(output, contract);
        let generation = prior
            .generation
            .checked_add(1)
            .ok_or_else(|| ChildError::Failed("subagent generation overflow".into()))?;
        self.insert(
            id.clone(),
            State {
                generation,
                output: output.clone(),
                updates: updates.clone(),
                retired: false,
                harness,
                kit,
                child,
                _permit: permit,
            },
        )?;
        Ok(SubagentValue {
            id,
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
        self.check_active(&source)?;
        self.check_generation(prior, source.generation)?;
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
    ) -> Result<Vec<SubagentValue>, ChildError> {
        let states = self
            .sessions
            .lock()
            .map_err(|_| ChildError::Failed("subagent registry lock was poisoned".into()))?
            .iter()
            .map(|(id, state)| (id.clone(), Arc::clone(state)))
            .collect::<Vec<_>>();
        let mut values = Vec::with_capacity(states.len());
        for (id, state) in states {
            let state = tokio::select! {
                state = state.lock() => state,
                () = cancellation.cancelled() => return Err(ChildError::Cancelled),
            };
            if !state.retired && !state.child.is_closed() {
                values.push(SubagentValue {
                    id,
                    output: state.output.clone(),
                    generation: state.generation,
                    updates: state.updates.clone(),
                });
            }
        }
        values.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(values)
    }

    async fn close(&self, id: &str, cancellation: &TurnCancellation) -> Result<(), ChildError> {
        let state = self
            .sessions
            .lock()
            .map_err(|_| ChildError::Failed("subagent registry lock was poisoned".into()))?
            .get(id)
            .cloned()
            .ok_or_else(|| ChildError::Failed(format!("unknown subagent session {id:?}")))?;
        let mut locked = tokio::select! {
            locked = state.lock() => locked,
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        };
        self.check_active(&locked)?;
        locked.child.close().await?;
        locked.retired = true;
        drop(locked);
        self.remove_if_same(id, &state);
        Ok(())
    }

    fn lookup(&self, prior: &SubagentValue) -> Result<Arc<AsyncMutex<State>>, ChildError> {
        self.sessions
            .lock()
            .map_err(|_| ChildError::Failed("subagent registry lock was poisoned".into()))?
            .get(&prior.id)
            .cloned()
            .ok_or_else(|| ChildError::Failed(format!("unknown subagent session {:?}", prior.id)))
    }

    fn insert(&self, id: String, state: State) -> Result<(), ChildError> {
        self.sessions
            .lock()
            .map_err(|_| ChildError::Failed("subagent registry lock was poisoned".into()))?
            .insert(id, Arc::new(AsyncMutex::new(state)));
        Ok(())
    }

    fn reserve(&self) -> Result<OwnedSemaphorePermit, ChildError> {
        self.sessions
            .lock()
            .map_err(|_| ChildError::Failed("subagent registry lock was poisoned".into()))?
            .retain(|_, state| {
                state
                    .try_lock()
                    .map_or(true, |state| !state.child.is_closed())
            });
        Arc::clone(&self.capacity).try_acquire_owned().map_err(|_| {
            ChildError::Failed(format!(
                "live subagent session limit ({MAX_LIVE_SUBAGENTS}) reached"
            ))
        })
    }

    fn remove_if_same(&self, id: &str, expected: &Arc<AsyncMutex<State>>) {
        if let Ok(mut sessions) = self.sessions.lock()
            && sessions
                .get(id)
                .is_some_and(|state| Arc::ptr_eq(state, expected))
        {
            sessions.remove(id);
        }
    }
    fn check_active(&self, state: &State) -> Result<(), ChildError> {
        if state.retired {
            Err(ChildError::Failed("subagent session is retired".into()))
        } else {
            Ok(())
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
#[derive(Clone)]
pub struct CloseTool {
    manager: Subagents,
    spec: ToolSpec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentId {
    id: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CloseInput {
    Handle(SubagentValue),
    Id(SubagentId),
}

impl CloseInput {
    fn id(self) -> String {
        match self {
            Self::Handle(value) => value.id,
            Self::Id(value) => value.id,
        }
    }
}

fn value_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"id":{"type":"string"},"output":{},"generation":{"type":"integer","minimum":1},"updates":{"type":"object","properties":{"items":{"type":"array","items":{"type":"object"}},"truncated":{"type":"boolean"}},"required":["items","truncated"],"additionalProperties":false}},"required":["id","output","generation"],"additionalProperties":false})
}
fn continuation_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"subagent":value_schema(),"prompt":{"type":"string"},"output_schema":{"oneOf":[{"type":"object"},{"type":"boolean"}]}},"required":["subagent","prompt"],"additionalProperties":false})
}
fn id_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false})
}

impl SubagentTool {
    pub fn new(manager: Subagents, depth: usize) -> Self {
        let harnesses = manager.harness_references();
        Self { manager, depth, spec: ToolSpec::new(ToolName::new("subagent"), "Start a parent-owned configured ACP harness, prompt it, and return its reusable session value. `harness` is an override that selects a value other than the user's configured preference.", json!({"type":"object","properties":{"prompt":{"type":"string"},"harness":{"type":"string","enum":harnesses,"description":"Override the user's configured harness preference with this value. Default to omitting it."},"output_schema":{"oneOf":[{"type":"object"},{"type":"boolean"}]}},"required":["prompt"],"additionalProperties":false})).with_output_schema(value_schema()).with_annotations(ToolAnnotations::new()) }
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
        Self { manager, depth, spec: ToolSpec::new(ToolName::new("fork"), "Fork a completed ACP subagent session using native capability support or the isolated Kit fallback, prompt it, and return the new session value.", continuation_schema()).with_output_schema(value_schema()).with_annotations(ToolAnnotations::new()) }
    }
}
impl SubagentsTool {
    pub fn new(manager: Subagents) -> Self {
        Self {
            manager,
            spec: ToolSpec::new(
                ToolName::new("subagents"),
                "List the active reusable subagent session handles owned by this parent session.",
                json!({"type":"object","properties":{},"additionalProperties":false}),
            )
            .with_output_schema(json!({"type":"array","items":value_schema()}))
            .with_annotations(ToolAnnotations::new()),
        }
    }
}
impl CloseTool {
    pub fn new(manager: Subagents) -> Self {
        Self { manager, spec: ToolSpec::new(ToolName::new("close"), "Close an active subagent by its complete handle or by an object containing its id. The handle becomes unusable and its capacity is released.", json!({"oneOf":[value_schema(), id_schema()]})).with_output_schema(id_schema()).with_annotations(ToolAnnotations::new()) }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    prompt: String,
    harness: Option<String>,
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
        ChildError::Cancelled => ToolError::Cancelled,
        ChildError::Failed(error) => ToolError::ExecutionFailed(error),
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
        let id = serde_json::from_value::<CloseInput>(request.input.clone())
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?
            .id();
        self.manager
            .close(&id, &cancellation(context))
            .await
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
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
        let input: Continuation = serde_json::from_value(request.input.clone())
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let contract = input.output_schema.map(OutputContract::new).transpose()?;
        result(
            request,
            self.manager
                .fork(
                    input.subagent,
                    input.prompt,
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
                mcp_config: None,
                credential_storage: Default::default(),
                harnesses: Default::default(),
                default_harness: crate::acp_child::BUILTIN_HARNESS.into(),
            },
            2,
        );
        let state = Arc::new(AsyncMutex::new(State {
            generation: 1,
            output: Value::String("done".into()),
            updates: None,
            retired: false,
            harness: crate::acp_child::BUILTIN_HARNESS.into(),
            kit: true,
            child: ChildSession::disconnected_for_test(),
            _permit: Arc::clone(&manager.capacity).try_acquire_owned().unwrap(),
        }));
        manager
            .sessions
            .lock()
            .unwrap()
            .insert("source".into(), Arc::clone(&state));
        let prior = SubagentValue {
            id: "source".into(),
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
                mcp_config: None,
                credential_storage: Default::default(),
                harnesses: Default::default(),
                default_harness: crate::acp_child::BUILTIN_HARNESS.into(),
            },
            2,
        );
        let state = Arc::new(AsyncMutex::new(State {
            generation: 1,
            output: Value::String("done".into()),
            updates: None,
            retired: false,
            harness: crate::acp_child::BUILTIN_HARNESS.into(),
            kit: true,
            child: ChildSession::disconnected_for_test(),
            _permit: Arc::clone(&manager.capacity).try_acquire_owned().unwrap(),
        }));
        manager
            .sessions
            .lock()
            .unwrap()
            .insert("source".into(), Arc::clone(&state));
        let prior = SubagentValue {
            id: "source".into(),
            output: Value::String("done".into()),
            generation: 1,
            updates: None,
        };
        (manager, state, prior)
    }

    #[tokio::test]
    async fn active_subagents_can_be_listed_and_closed() {
        let directory = tempfile::tempdir().unwrap();
        let (manager, state, prior) = manager_with_disconnected_session(directory.path());
        let (child, closed) = ChildSession::closure_probe_for_test();
        state.lock().await.child = child;
        drop(state);

        let cancellation = TurnCancellation::default();
        assert_eq!(manager.list(&cancellation).await.unwrap().len(), 1);
        assert_eq!(manager.list(&cancellation).await.unwrap()[0].id, prior.id);
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
        state.lock().await.child = child;
        drop(state);

        drop(manager);

        tokio::time::timeout(std::time::Duration::from_secs(1), closed)
            .await
            .expect("manager drop did not terminate the child actor")
            .unwrap();
    }

    #[test]
    fn close_input_accepts_a_handle_or_an_id() {
        let handle = json!({"id": "child", "output": "done", "generation": 1});
        let id = json!({"id": "child"});
        assert_eq!(
            serde_json::from_value::<CloseInput>(handle).unwrap().id(),
            "child"
        );
        assert_eq!(
            serde_json::from_value::<CloseInput>(id).unwrap().id(),
            "child"
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

    #[tokio::test]
    async fn child_prompt_failure_retires_the_session() {
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
                mcp_config: None,
                credential_storage: Default::default(),
                harnesses,
                default_harness: "acp.generic".into(),
            },
            2,
        );
        let prior = manager
            .create("base".into(), None, 0, TurnCancellation::default(), None)
            .await
            .unwrap();

        let error = manager
            .fork(prior, "branch".into(), 0, TurnCancellation::default(), None)
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "ACP harness \"acp.generic\" does not advertise session/fork; transcript fallback is only available for Kit"
        );
    }
}
