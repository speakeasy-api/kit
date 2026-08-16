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
use serde_json::json;
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
    child: ChildSession,
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SubagentValue {
    pub id: String,
    pub output: String,
    pub generation: u64,
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

    async fn create(
        &self,
        prompt: String,
        depth: usize,
        cancellation: TurnCancellation,
    ) -> Result<SubagentValue, ChildError> {
        self.check_depth(depth)?;
        let permit = self.reserve()?;
        let id = session::new_id();
        let child = ChildSession::start(
            self.config.clone(),
            id.clone(),
            false,
            depth + 1,
            cancellation.clone(),
        )
        .await?;
        let output = child.prompt(prompt, cancellation).await?;
        self.insert(
            id.clone(),
            State {
                generation: 1,
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
    ) -> Result<SubagentValue, ChildError> {
        let state = self.lookup(&prior)?;
        let mut locked = tokio::select! {
            locked = state.lock() => locked,
            () = cancellation.cancelled() => return Err(ChildError::Cancelled),
        };
        self.check_generation(&prior, locked.generation)?;
        let generation = locked
            .generation
            .checked_add(1)
            .ok_or_else(|| ChildError::Failed("subagent generation overflow".into()))?;
        match locked.child.prompt(prompt, cancellation).await {
            Ok(output) => {
                locked.generation = generation;
                Ok(SubagentValue {
                    id: prior.id,
                    output,
                    generation,
                })
            }
            Err(error) => {
                // Any dispatched unsuccessful turn may have changed the durable
                // transcript. Retire it rather than accepting the old generation.
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
    ) -> Result<SubagentValue, ChildError> {
        self.check_depth(depth)?;
        let permit = self.reserve()?;
        let id = self.clone_for_fork(&prior, &cancellation).await?;
        let child = ChildSession::start(
            self.config.clone(),
            id.clone(),
            true,
            depth + 1,
            cancellation.clone(),
        )
        .await?;
        let output = child.prompt(prompt, cancellation).await?;
        let generation = prior
            .generation
            .checked_add(1)
            .ok_or_else(|| ChildError::Failed("subagent generation overflow".into()))?;
        self.insert(
            id.clone(),
            State {
                generation,
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
    json!({"type":"object","properties":{"id":{"type":"string"},"output":{"type":"string"},"generation":{"type":"integer","minimum":1}},"required":["id","output","generation"],"additionalProperties":false})
}
fn continuation_schema() -> serde_json::Value {
    json!({"type":"object","properties":{"subagent":value_schema(),"prompt":{"type":"string"}},"required":["subagent","prompt"],"additionalProperties":false})
}

impl SubagentTool {
    pub fn new(manager: Subagents, depth: usize) -> Self {
        Self { manager, depth, spec: ToolSpec::new(ToolName::new("subagent"), "Start a parent-owned Kit subprocess over ACP, prompt it, and return its reusable session value.", json!({"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"],"additionalProperties":false})).with_output_schema(value_schema()).with_annotations(ToolAnnotations::new()) }
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
        Self { manager, depth, spec: ToolSpec::new(ToolName::new("fork"), "Clone a completed subagent transcript into a new ACP-backed Kit child, prompt it, and return the new session value.", continuation_schema()).with_output_schema(value_schema()).with_annotations(ToolAnnotations::new()) }
    }
}

#[derive(Deserialize)]
struct Input {
    prompt: String,
}
#[derive(Deserialize)]
struct Continuation {
    subagent: SubagentValue,
    prompt: String,
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
        result(
            request,
            self.manager
                .create(input.prompt, self.depth, cancellation(context))
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
        result(
            request,
            self.manager
                .prompt(input.subagent, input.prompt, cancellation(context))
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
        result(
            request,
            self.manager
                .fork(
                    input.subagent,
                    input.prompt,
                    self.depth,
                    cancellation(context),
                )
                .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use agentkit_core::{Item, ItemKind};

    use super::*;

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
            },
            2,
        );
        let state = Arc::new(AsyncMutex::new(State {
            generation: 1,
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
            output: "done".into(),
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
