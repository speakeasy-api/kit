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

const MAX_LIVE_SUBAGENTS: usize = 16;

use crate::{
    acp_child::{ChildConfig, ChildError, ChildSession},
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
    retired: bool,
    harness: String,
    kit: bool,
    child: ChildSession,
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SubagentValue {
    pub id: String,
    pub output: Value,
    pub generation: u64,
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

    fn parse(&self, output: String) -> Result<Value, ChildError> {
        let value: Value = serde_json::from_str(output.trim()).map_err(|error| {
            ChildError::Failed(format!(
                "subagent returned invalid JSON for output_schema: {error}"
            ))
        })?;
        if let Err(error) = self.validator.validate(&value) {
            return Err(ChildError::Failed(format!(
                "subagent output did not match output_schema at {}: {error}",
                error.instance_path()
            )));
        }
        Ok(value)
    }
}

fn structured_prompt(prompt: String, contract: Option<&OutputContract>) -> String {
    match contract {
        Some(contract) => contract.prompt(prompt),
        None => prompt,
    }
}

fn structured_output(
    output: String,
    contract: Option<&OutputContract>,
) -> Result<Value, ChildError> {
    match contract {
        Some(contract) => contract.parse(output),
        None => Ok(Value::String(output)),
    }
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
        let output = structured_output(output, contract)?;
        self.insert(
            id.clone(),
            State {
                generation: 1,
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
            Ok(output) => match structured_output(output, contract) {
                Ok(output) => {
                    locked.generation = generation;
                    Ok(SubagentValue {
                        id: prior.id,
                        output,
                        generation,
                    })
                }
                Err(error) => {
                    locked.retired = true;
                    drop(locked);
                    self.remove_if_same(&prior.id, &state);
                    Err(error)
                }
            },
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
        let output = child
            .prompt(structured_prompt(prompt, contract), cancellation)
            .await?;
        let output = structured_output(output, contract)?;
        let generation = prior
            .generation
            .checked_add(1)
            .ok_or_else(|| ChildError::Failed("subagent generation overflow".into()))?;
        self.insert(
            id.clone(),
            State {
                generation,
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
        })
    }

    #[cfg(test)]
    async fn clone_for_fork(
        &self,
        prior: &SubagentValue,
        cancellation: &TurnCancellation,
    ) -> Result<String, ChildError> {
        let source = self.lookup(prior)?;
        let source = tokio::select! {
            source = source.lock() => source,
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        };
        self.check_active(&source)?;
        self.check_generation(prior, source.generation)?;
        let id = session::new_id();
        session::clone_completed(&self.config.root, &prior.id, &id).map_err(ChildError::Failed)?;
        // This is the stable snapshot boundary. Returning drops the source
        // guard before child startup or the branch prompt can begin.
        drop(source);
        Ok(id)
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

fn value_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"id":{"type":"string"},"output":{},"generation":{"type":"integer","minimum":1}},"required":["id","output","generation"],"additionalProperties":false})
}
fn continuation_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"subagent":value_schema(),"prompt":{"type":"string"},"output_schema":{"oneOf":[{"type":"object"},{"type":"boolean"}]}},"required":["subagent","prompt"],"additionalProperties":false})
}

impl SubagentTool {
    pub fn new(manager: Subagents, depth: usize) -> Self {
        let harnesses = manager.harness_references();
        Self { manager, depth, spec: ToolSpec::new(ToolName::new("subagent"), "Start a parent-owned configured ACP harness, prompt it, and return its reusable session value.", json!({"type":"object","properties":{"prompt":{"type":"string"},"harness":{"type":"string","enum":harnesses},"output_schema":{"oneOf":[{"type":"object"},{"type":"boolean"}]}},"required":["prompt"],"additionalProperties":false})).with_output_schema(value_schema()).with_annotations(ToolAnnotations::new()) }
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
            contract.parse(r#"{"approved":true}"#.into()).unwrap(),
            json!({"approved": true})
        );
        assert!(contract.parse(r#"{"approved":"yes"}"#.into()).is_err());
        assert!(contract.parse("```json\n{}\n```".into()).is_err());
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
        assert_eq!(contract.parse("[1, 2]".into()).unwrap(), json!([1, 2]));
    }

    #[test]
    fn unstructured_output_remains_text() {
        assert_eq!(
            structured_output("plain text".into(), None).unwrap(),
            Value::String("plain text".into())
        );
    }

    #[tokio::test]
    async fn fork_snapshot_releases_source_before_child_work() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = vec![Item::text(ItemKind::System, "system")];
        let opened = session::open(directory.path(), "source", false, false, transcript).unwrap();
        let manager = Subagents::new(
            ChildConfig {
                root: directory.path().to_path_buf(),
                model: "test".into(),
                mcp_config: None,
                credential_storage: Default::default(),
                harnesses: Default::default(),
                default_harness: crate::acp_child::BUILTIN_HARNESS.into(),
            },
            2,
        );
        let state = Arc::new(AsyncMutex::new(State {
            generation: 1,
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
        };

        let branch = manager
            .clone_for_fork(&prior, &TurnCancellation::default())
            .await
            .unwrap();

        assert!(
            state.try_lock().is_ok(),
            "source remained locked after snapshot"
        );
        assert!(session::load(directory.path(), &branch).is_ok());
        drop(opened);
    }
}
