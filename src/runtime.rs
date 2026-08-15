use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use agentkit_acp::{AcpAgentFactoryContext, AcpRuntimeError};
use agentkit_context::{AgentsMd, ContextLoader};
use agentkit_core::{
    CancellationController, CancellationHandle, FinishReason, Item, ItemKind, Part,
};
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
    tools::{A2aTool, AuthTool, EditTool, McpTool, Observed, ShellTool, SubagentTool, ToolSearch},
};

#[cfg(test)]
mod tests;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct SessionRequest {
    pub id: String,
    pub resume: bool,
    pub force: bool,
}

pub struct Runtime {
    root: PathBuf,
    adapter: OpenAiSubscriptionAdapter,
    max_subagent_depth: usize,
    session: Option<SessionRequest>,
    mcp: crate::tools::mcp::McpRuntime,
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
            session: None,
            mcp: crate::tools::mcp::empty(),
        }))
    }

    /// Configures the single persistent ACP session served by this runtime.
    pub fn with_session(
        root: impl AsRef<Path>,
        model: impl Into<String>,
        session: SessionRequest,
    ) -> Result<Arc<Self>, String> {
        let mut runtime = Arc::try_unwrap(Self::new(root, model)?)
            .map_err(|_| "could not configure runtime session".to_string())?;
        runtime.session = Some(session);
        Ok(Arc::new(runtime))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Connects the explicitly configured MCP servers before the runtime is served.
    pub async fn with_mcp_config(
        runtime: Arc<Self>,
        path: Option<&Path>,
        interactive_oauth_enabled: bool,
        credential_storage: crate::tools::mcp::CredentialStorage,
    ) -> Result<Arc<Self>, String> {
        let Some(path) = path else {
            return Ok(runtime);
        };
        let mcp =
            crate::tools::mcp::connect(path, interactive_oauth_enabled, credential_storage).await?;
        let mut runtime = Arc::try_unwrap(runtime)
            .map_err(|_| "could not configure MCP after runtime was shared".to_string())?;
        runtime.mcp = mcp;
        Ok(Arc::new(runtime))
    }

    pub const fn max_subagent_depth(&self) -> usize {
        self.max_subagent_depth
    }

    pub fn compose(self: &Arc<Self>, depth: usize) -> ComposeOnly {
        let children = agentkit_tools_core::ToolRegistry::new()
            .with(Observed::new(ShellTool::new(self.root.clone())))
            .with(Observed::new(EditTool::new(self.root.clone())))
            .with(Observed::new(SubagentTool::new(Arc::clone(self), depth)))
            .with(Observed::new(A2aTool::new()))
            .with(Observed::new(ToolSearch::new(self.mcp.clone())))
            .with(Observed::new(AuthTool::new(self.mcp.clone())))
            .with(Observed::new(McpTool::new(self.mcp.catalog())));
        let child_specs = children.specs();
        ComposeOnly(
            ComposeTool::wrap(children)
                .with_source(self.mcp.catalog().unadvertised())
                .with_config(ComposeConfig::new().with_max_nested_tool_calls(128))
                .with_backend(HiddenRunletBackend(child_specs)),
        )
    }

    pub async fn run(self: &Arc<Self>, prompt: String, depth: usize) -> Result<String, LoopError> {
        self.run_interruptible(prompt, depth, None).await
    }

    /// Runs one prompt in the configured durable session.
    pub async fn run_persistent(self: &Arc<Self>, prompt: String) -> Result<String, String> {
        let request = self
            .session
            .clone()
            .ok_or_else(|| "persistent run requires a configured session".to_string())?;
        let initial = if request.resume {
            vec![Item::text(ItemKind::System, self.system_prompt(0))]
        } else {
            self.initial_transcript(0).await?
        };
        let opened = crate::session::open(
            &self.root,
            &request.id,
            request.resume,
            request.force,
            initial,
        )?;
        let compactor = crate::compaction::automatic(
            self.adapter.clone(),
            Some(opened.observer.clone()),
            format!("compaction-{}", crate::session::new_id()),
        )?;
        let agent = Agent::builder()
            .model(self.adapter.clone())
            .add_tool_source(self.compose(0))
            .mutator(compactor)
            .transcript_observer(opened.observer)
            .transcript(opened.transcript)
            .input(vec![Item::text(ItemKind::User, prompt)])
            .build()
            .map_err(|error| error.to_string())?;
        let mut driver = agent
            .start(SessionConfig::new(request.id).without_cache())
            .await
            .map_err(|error| error.to_string())?;
        drive(&mut driver).await.map_err(|error| error.to_string())
    }

    pub async fn run_cancelled(
        self: &Arc<Self>,
        prompt: String,
        depth: usize,
        cancellation: Option<CancellationToken>,
    ) -> Result<String, LoopError> {
        let controller = CancellationController::new();
        let bridge = cancellation.map(|token| {
            let controller = controller.clone();
            tokio::spawn(async move {
                token.cancelled().await;
                controller.interrupt();
            })
        });
        let result = self
            .run_interruptible(prompt, depth, Some(controller.handle()))
            .await;
        if let Some(bridge) = bridge {
            bridge.abort();
        }
        result
    }

    /// Runs a nested prompt under a cancellation handle owned by the caller, so
    /// interrupting the outer turn also ends everything it started.
    pub(crate) async fn run_interruptible(
        self: &Arc<Self>,
        prompt: String,
        depth: usize,
        cancellation: Option<CancellationHandle>,
    ) -> Result<String, LoopError> {
        let session = format!("run-{}", NEXT_SESSION.fetch_add(1, Ordering::Relaxed));
        let transcript = self
            .initial_transcript(depth)
            .await
            .map_err(LoopError::InvalidState)?;
        let compactor = crate::compaction::automatic(
            self.adapter.clone(),
            None,
            format!("compaction-{session}"),
        )
        .map_err(LoopError::InvalidState)?;
        let mut builder = Agent::builder()
            .model(self.adapter.clone())
            .add_tool_source(self.compose(depth))
            .mutator(compactor)
            .transcript(transcript)
            .input(vec![Item::text(ItemKind::User, prompt)]);
        if let Some(cancellation) = cancellation {
            builder = builder.cancellation(cancellation);
        }
        let mut driver = builder
            .build()?
            .start(SessionConfig::new(session).without_cache())
            .await?;
        drive(&mut driver).await
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
        let request = self
            .session
            .clone()
            .unwrap_or_else(|| crate::runtime::SessionRequest {
                id: crate::session::new_id(),
                resume: false,
                force: false,
            });
        let initial = if request.resume {
            vec![Item::text(ItemKind::System, self.system_prompt(0))]
        } else {
            self.initial_transcript(0)
                .await
                .map_err(AcpRuntimeError::Loop)?
        };
        let opened = crate::session::open(
            &self.root,
            &request.id,
            request.resume,
            request.force,
            initial,
        )
        .map_err(AcpRuntimeError::Loop)?;
        let compactor = crate::compaction::automatic(
            self.adapter.clone(),
            Some(opened.observer.clone()),
            format!("compaction-{}", crate::session::new_id()),
        )
        .map_err(AcpRuntimeError::Loop)?;
        Agent::builder()
            .model(self.adapter.clone())
            .add_tool_source(self.compose(0))
            .mutator(compactor)
            .observer(context.integration.as_ref().clone())
            .transcript_observer(opened.observer)
            .transcript(opened.transcript)
            .cancellation(context.cancellation)
            .build()
            .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?
            // ACP routes observer events by its own bound AgentKit id. The
            // persisted id names storage; it must not replace that routing key.
            .start(SessionConfig::new(context.agentkit_session_id).without_cache())
            .await
            .map_err(|error| AcpRuntimeError::Loop(error.to_string()))
    }

    async fn initial_transcript(&self, depth: usize) -> Result<Vec<Item>, String> {
        load_initial_transcript(&self.root, self.system_prompt(depth)).await
    }

    fn system_prompt(&self, depth: usize) -> String {
        format!(
            "You are a coding agent using Kit version {} as your harness, rooted at {}. The only model-visible tool is compose. Use Runlet scripts inside compose to call the hidden shell, edit, subagent, and a2a tools, plus the MCP meta-tools tool_search, auth, and tool. Use tool_search to discover MCP servers and tools. When a matching server requires authentication, call auth with its exact name and give the returned URL to the user; search again after they complete it. Invoke only MCP tool names returned by tool_search. Make minimal changes, inspect before editing, and run the smallest useful check. Current subagent depth: {depth}/{}.",
            env!("CARGO_PKG_VERSION"),
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

    fn description(&self, _catalog: Option<&[ToolSpec]>) -> String {
        let mut description = RunletBackend.description(None);
        description.push_str(
            "\n\nHidden callable tools. Each entry includes the exact compact JSON schemas \
             used by Runlet for input checking and output typing:",
        );
        for spec in &self.0 {
            let _ = write!(
                description,
                "\n\n- `{}`: {}\n  Input JSON schema: `{}`\n  Output JSON schema: `{}`",
                spec.name.0,
                spec.description,
                spec.input_schema,
                spec.output_schema.as_ref().unwrap_or(&Value::Null),
            );
        }
        description
    }

    fn script_description(&self) -> &'static str {
        RunletBackend.script_description()
    }

    async fn execute(&self, mut run: BackendRun) -> Result<Value, ComposeOutcome> {
        run.visible_specs.clone_from(&self.0);
        RunletBackend.execute(run).await
    }
}

async fn load_initial_transcript(root: &Path, system_prompt: String) -> Result<Vec<Item>, String> {
    let mut transcript = vec![Item::text(ItemKind::System, system_prompt)];
    let context = ContextLoader::new()
        .with_source(AgentsMd::discover_all(root))
        .load()
        .await
        .map_err(|error| format!("could not load AGENTS.md context: {error}"))?;
    transcript.extend(context);
    Ok(transcript)
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
