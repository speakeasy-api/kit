use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use agentkit_acp::{AcpAgentFactoryContext, AcpRuntimeError};
use agentkit_core::{CancellationController, FinishReason, Item, ItemKind, Part};
use agentkit_loop::{Agent, LoopDriver, LoopError, LoopInterrupt, LoopStep, SessionConfig};
use agentkit_tool_compose::{
    BackendRun, ComposeBackend, ComposeConfig, ComposeOutcome, ComposeTool, RunletBackend,
};
use agentkit_tools_core::{Tool, ToolName, ToolSource, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    provider::{OpenAiSubscriptionAdapter, OpenAiSubscriptionSession, SubscriptionConfig},
    tools::{A2aTool, EditTool, Observed, ShellTool, SubagentTool},
};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

pub struct Runtime {
    root: PathBuf,
    adapter: OpenAiSubscriptionAdapter,
    max_subagent_depth: usize,
}

impl Runtime {
    pub fn new(root: impl AsRef<Path>, model: impl Into<String>) -> Result<Arc<Self>, String> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|error| format!("could not open runtime root: {error}"))?;
        if !root.is_dir() {
            return Err(format!(
                "runtime root is not a directory: {}",
                root.display()
            ));
        }
        let adapter = OpenAiSubscriptionAdapter::new(SubscriptionConfig::new(model.into())?)?;
        Ok(Arc::new(Self {
            root,
            adapter,
            max_subagent_depth: 2,
        }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub const fn max_subagent_depth(&self) -> usize {
        self.max_subagent_depth
    }

    pub fn compose(self: &Arc<Self>, depth: usize) -> ComposeOnly {
        let children = agentkit_tools_core::ToolRegistry::new()
            .with(Observed::new(ShellTool::new(self.root.clone())))
            .with(Observed::new(EditTool::new(self.root.clone())))
            .with(Observed::new(SubagentTool::new(Arc::clone(self), depth)))
            .with(Observed::new(A2aTool::new()));
        let child_specs = children.specs();
        ComposeOnly(
            ComposeTool::wrap(children)
                .with_config(ComposeConfig::new().with_max_nested_tool_calls(128))
                .with_backend(HiddenRunletBackend(child_specs)),
        )
    }

    pub async fn run(self: &Arc<Self>, prompt: String, depth: usize) -> Result<String, LoopError> {
        self.run_cancelled(prompt, depth, None).await
    }

    pub async fn run_cancelled(
        self: &Arc<Self>,
        prompt: String,
        depth: usize,
        cancellation: Option<CancellationToken>,
    ) -> Result<String, LoopError> {
        let session = format!("run-{}", NEXT_SESSION.fetch_add(1, Ordering::Relaxed));
        let controller = CancellationController::new();
        let agent = Agent::builder()
            .model(self.adapter.clone())
            .add_tool_source(self.compose(depth))
            .transcript(vec![Item::text(
                ItemKind::System,
                self.system_prompt(depth),
            )])
            .input(vec![Item::text(ItemKind::User, prompt)])
            .cancellation(controller.handle())
            .build()?;
        let mut driver = agent
            .start(SessionConfig::new(session).without_cache())
            .await?;
        let bridge = cancellation.map(|token| {
            let controller = controller.clone();
            tokio::spawn(async move {
                token.cancelled().await;
                controller.interrupt();
            })
        });
        let result = drive(&mut driver).await;
        if let Some(bridge) = bridge {
            bridge.abort();
        }
        result
    }

    pub(crate) async fn start_acp_driver(
        self: &Arc<Self>,
        context: AcpAgentFactoryContext,
    ) -> Result<LoopDriver<OpenAiSubscriptionSession>, AcpRuntimeError> {
        let cwd = context
            .cwd
            .canonicalize()
            .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?;
        if cwd != self.root || !context.additional_directories.is_empty() {
            return Err(AcpRuntimeError::Loop(format!(
                "this Kit runtime is fixed to {} and does not accept additional directories",
                self.root.display()
            )));
        }
        Agent::builder()
            .model(self.adapter.clone())
            .add_tool_source(self.compose(0))
            .observer(context.integration.as_ref().clone())
            .transcript(vec![Item::text(ItemKind::System, self.system_prompt(0))])
            .cancellation(context.cancellation)
            .build()
            .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?
            .start(SessionConfig::new(context.agentkit_session_id).without_cache())
            .await
            .map_err(|error| AcpRuntimeError::Loop(error.to_string()))
    }

    fn system_prompt(&self, depth: usize) -> String {
        format!(
            "You are a coding agent rooted at {}. The only model-visible tool is compose. Use Runlet scripts inside compose to call the hidden shell, edit, subagent, and a2a tools. Make minimal changes, inspect before editing, and run the smallest useful check. Current subagent depth: {depth}/{}.",
            self.root.display(),
            self.max_subagent_depth
        )
    }
}

pub struct ComposeOnly(ComposeTool);

impl ToolSource for ComposeOnly {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![self.0.spec().clone()]
    }

    fn get(&self, name: &ToolName) -> Option<Arc<dyn Tool>> {
        ToolSource::get(&self.0, name)
    }
}

struct HiddenRunletBackend(Vec<ToolSpec>);

#[async_trait]
impl ComposeBackend for HiddenRunletBackend {
    fn name(&self) -> &'static str {
        RunletBackend.name()
    }

    fn description(&self, catalog: Option<&[ToolSpec]>) -> String {
        RunletBackend.description(catalog)
    }

    fn script_description(&self) -> &'static str {
        RunletBackend.script_description()
    }

    async fn execute(&self, mut run: BackendRun) -> Result<Value, ComposeOutcome> {
        run.visible_specs.clone_from(&self.0);
        RunletBackend.execute(run).await
    }
}

async fn drive(driver: &mut LoopDriver<OpenAiSubscriptionSession>) -> Result<String, LoopError> {
    loop {
        match driver.next().await? {
            LoopStep::Finished(result) => {
                if result.finish_reason == FinishReason::Cancelled {
                    return Err(LoopError::Cancelled);
                }
                return Ok(result
                    .items
                    .iter()
                    .flat_map(|item| &item.parts)
                    .filter_map(|part| match part {
                        Part::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""));
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(_) => {
                return Err(LoopError::InvalidState(
                    "agent stopped for unsupported input or approval".into(),
                ));
            }
        }
    }
}
