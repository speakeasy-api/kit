use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use agentkit_acp::{AcpIntegration, AcpRuntimeError};
use agentkit_context::{AgentsMd, ContextLoader};
use agentkit_core::{
    CancellationController, CancellationHandle, FinishReason, Item, ItemKind, Part, SessionId,
};
use agentkit_loop::{Agent, LoopDriver, LoopError, LoopInterrupt, LoopStep, SessionConfig};
use agentkit_task_manager::{AsyncTaskManager, RoutingDecision, TaskManager, TaskManagerHandle};
use agentkit_tool_compose::{
    BackendRun, ComposeBackend, ComposeConfig, ComposeOutcome, ComposeTool, RunletBackend,
};
use agentkit_tool_skills::SkillRegistry;
use agentkit_tools_core::{
    PermissionRequest, Tool, ToolContext, ToolError, ToolExecutionOutcome, ToolName, ToolRegistry,
    ToolRequest, ToolResult, ToolSource, ToolSpec,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    acp_child::{AcpHarnesses, BUILTIN_HARNESS, ChildConfig},
    provider::{KitAdapter, KitSession, ProviderKind},
    tools::{
        A2aTool, AuthTool, CloseTool, DocsTool, EditTool, ForkTool, McpTool, Observed, PromptTool,
        ShellTool, SubagentTool, Subagents, SubagentsTool, ToolSearch, observe_shared,
    },
};

#[cfg(test)]
mod tests;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
const MAX_BACKGROUND_AFTER_SECONDS: u64 = 86_400;

#[derive(Clone, Debug)]
pub struct SessionRequest {
    pub id: String,
    pub resume: bool,
    pub force: bool,
}

#[derive(Default)]
struct SessionSelection {
    configured: Option<SessionRequest>,
    claimed: bool,
}

impl SessionSelection {
    fn claim(&mut self) -> (SessionRequest, bool) {
        if !self.claimed
            && let Some(request) = self.configured.clone()
        {
            self.claimed = true;
            return (request, true);
        }
        (
            SessionRequest {
                id: crate::session::new_id(),
                resume: false,
                force: false,
            },
            false,
        )
    }

    fn finish(&mut self, configured: bool, succeeded: bool, opened_new: bool) {
        if configured {
            if succeeded {
                self.configured.take();
            } else if opened_new && let Some(request) = &mut self.configured {
                // Opening already persisted the bootstrap transcript. A retry
                // must resume it rather than trying to create the same file.
                request.resume = true;
            }
            self.claimed = false;
        }
    }
}

pub(crate) struct AcpDriverContext {
    pub acp_session_id: agentkit_acp::SessionId,
    pub agentkit_session_id: SessionId,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub integration: Arc<AcpIntegration>,
    pub cancellation: CancellationHandle,
}

pub(crate) struct AcpDriver {
    pub driver: LoopDriver<KitSession>,
    pub tasks: TaskManagerHandle,
}

pub struct Runtime {
    root: PathBuf,
    adapter: KitAdapter,
    provider: ProviderKind,
    model: String,
    max_subagent_depth: usize,
    base_depth: usize,
    subagents: Subagents,
    /// The explicitly selected session is consumed by the first ACP session.
    /// Later ACP sessions receive their own persisted ids.
    session: Mutex<SessionSelection>,
    mcp: crate::tools::mcp::McpRuntime,
    skills: ToolRegistry,
}

impl Runtime {
    pub fn new(root: impl AsRef<Path>, model: impl Into<String>) -> Result<Arc<Self>, String> {
        Self::new_with_provider(root, model, ProviderKind::default())
    }

    pub fn new_with_provider(
        root: impl AsRef<Path>,
        model: impl Into<String>,
        provider: ProviderKind,
    ) -> Result<Arc<Self>, String> {
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
        let skills = SkillRegistry::from_paths(default_skill_roots(&root)).tool_registry();
        let model = model.into();
        let adapter = KitAdapter::new(provider, model.clone())?;
        let max_subagent_depth = 2;
        let subagents = Subagents::new(
            ChildConfig {
                root: root.clone(),
                model: model.clone(),
                provider,
                mcp_config: None,
                credential_storage: Default::default(),
                harnesses: AcpHarnesses::default(),
                default_harness: BUILTIN_HARNESS.into(),
            },
            max_subagent_depth,
        );
        Ok(Arc::new(Self {
            root,
            adapter,
            provider,
            model,
            max_subagent_depth,
            base_depth: 0,
            subagents,
            session: Mutex::new(SessionSelection::default()),
            mcp: crate::tools::mcp::empty(),
            skills,
        }))
    }

    /// Selects the persistent session used by the first ACP session. Later ACP
    /// sessions receive fresh persisted ids.
    pub fn with_session(
        root: impl AsRef<Path>,
        model: impl Into<String>,
        session: SessionRequest,
    ) -> Result<Arc<Self>, String> {
        Self::with_session_and_provider(root, model, ProviderKind::default(), session)
    }

    pub fn with_session_and_provider(
        root: impl AsRef<Path>,
        model: impl Into<String>,
        provider: ProviderKind,
        session: SessionRequest,
    ) -> Result<Arc<Self>, String> {
        let mut runtime = Arc::try_unwrap(Self::new_with_provider(root, model, provider)?)
            .map_err(|_| "could not configure runtime session".to_string())?;
        runtime
            .session
            .get_mut()
            .map_err(|_| "could not configure poisoned runtime session".to_string())?
            .configured = Some(session);
        Ok(Arc::new(runtime))
    }

    /// Sets the inherited nesting depth for an ACP subprocess.
    pub fn with_depth(runtime: Arc<Self>, depth: usize) -> Result<Arc<Self>, String> {
        if depth > runtime.max_subagent_depth {
            return Err(format!(
                "subagent depth {depth} exceeds limit {}",
                runtime.max_subagent_depth
            ));
        }
        let mut runtime = Arc::try_unwrap(runtime)
            .map_err(|_| "could not configure runtime depth after it was shared".to_string())?;
        runtime.base_depth = depth;
        Ok(Arc::new(runtime))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Configures the trusted named ACP harnesses used by nested agents.
    pub fn with_acp_harnesses(
        runtime: Arc<Self>,
        harnesses: AcpHarnesses,
        default_harness: String,
    ) -> Result<Arc<Self>, String> {
        if !harnesses.contains(&default_harness) {
            return Err(format!("unknown subagent ACP harness {default_harness:?}"));
        }
        let mut runtime = Arc::try_unwrap(runtime).map_err(|_| {
            "could not configure ACP harnesses after runtime was shared".to_string()
        })?;
        runtime.subagents = Subagents::new(
            ChildConfig {
                root: runtime.root.clone(),
                model: runtime.model.clone(),
                provider: runtime.provider,
                mcp_config: None,
                credential_storage: Default::default(),
                harnesses,
                default_harness,
            },
            runtime.max_subagent_depth,
        );
        Ok(Arc::new(runtime))
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
            crate::tools::mcp::connect(path, interactive_oauth_enabled, credential_storage.clone())
                .await?;
        let mut runtime = Arc::try_unwrap(runtime)
            .map_err(|_| "could not configure MCP after runtime was shared".to_string())?;
        runtime.mcp = mcp;
        let previous = runtime.subagents.child_config();
        runtime.subagents = Subagents::new(
            ChildConfig {
                root: runtime.root.clone(),
                model: runtime.model.clone(),
                provider: runtime.provider,
                mcp_config: Some(path.to_path_buf()),
                credential_storage,
                harnesses: previous.harnesses,
                default_harness: previous.default_harness,
            },
            runtime.max_subagent_depth,
        );
        Ok(Arc::new(runtime))
    }

    pub const fn max_subagent_depth(&self) -> usize {
        self.max_subagent_depth
    }

    /// Returns the depth inherited by this runtime process.
    pub const fn base_depth(&self) -> usize {
        self.base_depth
    }

    pub fn compose(self: &Arc<Self>, depth: usize) -> ComposeOnly {
        self.compose_with(depth, self.subagents.fresh())
    }

    fn compose_with(&self, depth: usize, subagents: Subagents) -> ComposeOnly {
        let mut children = agentkit_tools_core::ToolRegistry::new()
            .with(Observed::new(DocsTool::new()))
            .with(Observed::new(ShellTool::new(self.root.clone())))
            .with(Observed::new(EditTool::new(self.root.clone())))
            .with(Observed::new(SubagentTool::new(subagents.clone(), depth)))
            .with(Observed::new(PromptTool::new(subagents.clone())))
            .with(Observed::new(ForkTool::new(subagents.clone(), depth)))
            .with(Observed::new(SubagentsTool::new(subagents.clone())))
            .with(Observed::new(CloseTool::new(subagents)))
            .with(Observed::new(A2aTool::new()))
            .with(Observed::new(ToolSearch::new(self.mcp.clone())))
            .with(Observed::new(AuthTool::new(self.mcp.clone())))
            .with(Observed::new(McpTool::new(self.mcp.catalog())));
        if let Some(skill_tool) = self.skills.get(&ToolName::new("activate_skill")) {
            children.register(observe_shared(skill_tool));
        }
        let child_specs = children.specs();
        let compose = ComposeTool::wrap(children)
            .with_source(self.mcp.catalog().unadvertised())
            .with_config(ComposeConfig::new().with_max_nested_tool_calls(128))
            .with_backend(HiddenRunletBackend(child_specs));
        ComposeOnly {
            backgroundable: BackgroundableCompose::new(compose.clone()),
            compose,
        }
    }

    pub async fn run(self: &Arc<Self>, prompt: String, depth: usize) -> Result<String, LoopError> {
        self.run_interruptible(prompt, depth, None).await
    }

    /// Runs one prompt in the configured durable session.
    pub async fn run_persistent(self: &Arc<Self>, prompt: String) -> Result<String, String> {
        let request = self
            .session
            .lock()
            .map_err(|_| "runtime session selection is poisoned".to_string())?
            .configured
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
        let subagents = self.subagents.fresh();
        let agent = Agent::builder()
            .model(self.adapter.clone())
            .add_tool_source(self.compose_with(0, subagents))
            .task_manager(background_task_manager())
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
        let subagents = self.subagents.fresh();
        let mut builder = Agent::builder()
            .model(self.adapter.clone())
            .add_tool_source(self.compose_with(depth, subagents))
            .task_manager(background_task_manager())
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
        context: AcpDriverContext,
    ) -> Result<AcpDriver, AcpRuntimeError> {
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
        let (request, configured) = self
            .session
            .lock()
            .map_err(|_| AcpRuntimeError::Loop("runtime session selection is poisoned".into()))?
            .claim();
        let acp_session_id = context.acp_session_id.to_string();
        let mut opened_new = false;
        let result = async {
            let initial = if request.resume {
                vec![Item::text(
                    ItemKind::System,
                    self.system_prompt(self.base_depth),
                )]
            } else {
                self.initial_transcript(self.base_depth)
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
            opened_new = !request.resume;
            let compactor = crate::compaction::automatic(
                self.adapter.clone(),
                Some(opened.observer.clone()),
                format!("compaction-{}", crate::session::new_id()),
            )
            .map_err(AcpRuntimeError::Loop)?;
            let subagents = self.subagents.fresh();
            let task_manager = background_task_manager();
            let tasks = task_manager.handle();
            let driver = Agent::builder()
                .model(self.adapter.clone())
                .add_tool_source(self.compose_with(self.base_depth, subagents))
                .task_manager(task_manager)
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
                .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?;
            Ok(AcpDriver { driver, tasks })
        }
        .await;
        self.session
            .lock()
            .map_err(|_| AcpRuntimeError::Loop("runtime session selection is poisoned".into()))?
            .finish(configured, result.is_ok(), opened_new);
        let driver = result?;
        crate::events::emit(&crate::events::RuntimeEvent::SessionStarted {
            acp_session_id,
            id: request.id,
        });
        Ok(driver)
    }

    async fn initial_transcript(&self, depth: usize) -> Result<Vec<Item>, String> {
        load_initial_transcript(&self.root, self.system_prompt(depth)).await
    }

    fn system_prompt(&self, depth: usize) -> String {
        format!(
            concat!(
                "You are a coding agent using Kit version {} as your harness, rooted at {}. ",
                "Make minimal changes, inspect before editing, and run the smallest useful check.\n\n",
                "Use compose as a dependency graph: independent calls and `for` iterations run concurrently, including effectful calls; ",
                "express required ordering with data dependencies or `after`, and use `fold` only for reductions or genuinely sequential chains. ",
                "Parallelize independent work deliberately. Background long-running compose work when keeping the session responsive or doing other independent work meanwhile is more useful than waiting; it also suits one-shot triggers. ",
                "Set the outer `background` argument to `true` to detach immediately or to a positive integer to wait that many seconds before detaching. ",
                "Keep work foregrounded when the next step needs its immediate result, and do not treat backgrounding as durable job execution.\n\n",
                "When several subagents need the same context, first complete one context-loading subagent, then fork it into parallel branches. ",
                "When work changes phase or objective, start fresh subagents from concise summaries of prior results instead of carrying unrelated history. ",
                "Keep outputs focused, pass only necessary context, reuse sessions only when continuity helps, and close subagents when no longer needed.\n\n",
                "Current subagent depth: {depth}/{}."
            ),
            env!("CARGO_PKG_VERSION"),
            self.root.display(),
            self.max_subagent_depth,
            depth = depth
        )
    }
}

fn default_skill_roots(root: &Path) -> Vec<PathBuf> {
    let mut roots = vec![root.join(".agents/skills")];
    #[cfg(unix)]
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty());
    #[cfg(not(unix))]
    let home = std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|value| !value.is_empty()));
    if let Some(home) = home {
        roots.push(PathBuf::from(home).join(".agents/skills"));
    }
    roots
}

pub struct ComposeOnly {
    compose: ComposeTool,
    backgroundable: BackgroundableCompose,
}

impl ToolSource for ComposeOnly {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            self.backgroundable
                .current_spec()
                .unwrap_or_else(|| self.backgroundable.spec().clone()),
        ]
    }

    fn get(&self, name: &ToolName) -> Option<Arc<dyn Tool>> {
        if name.0 == agentkit_tool_compose::COMPOSE_TOOL_NAME {
            Some(Arc::new(self.backgroundable.clone()))
        } else {
            ToolSource::get(&self.compose, name)
        }
    }
}

#[derive(Clone)]
struct BackgroundableCompose {
    inner: ComposeTool,
    spec: ToolSpec,
}

impl BackgroundableCompose {
    fn new(inner: ComposeTool) -> Self {
        let spec = backgroundable_spec(inner.spec().clone());
        Self { inner, spec }
    }

    fn sanitized(mut request: ToolRequest) -> Result<ToolRequest, ToolError> {
        let Some(object) = request.input.as_object_mut() else {
            return Ok(request);
        };
        if let Some(background) = object.remove("background") {
            let valid = matches!(background, Value::Bool(_))
                || background
                    .as_u64()
                    .is_some_and(|seconds| (1..=MAX_BACKGROUND_AFTER_SECONDS).contains(&seconds));
            if !valid {
                return Err(ToolError::InvalidInput(format!(
                    "background must be a boolean or an integer from 1 to {MAX_BACKGROUND_AFTER_SECONDS}"
                )));
            }
        }
        Ok(request)
    }
}

#[async_trait]
impl Tool for BackgroundableCompose {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn current_spec(&self) -> Option<ToolSpec> {
        self.inner.current_spec().map(backgroundable_spec)
    }

    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        self.inner
            .proposed_requests(&Self::sanitized(request.clone())?)
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.inner.invoke(Self::sanitized(request)?, ctx).await
    }

    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        match Self::sanitized(request) {
            Ok(request) => self.inner.invoke_outcome(request, ctx).await,
            Err(error) => ToolExecutionOutcome::Failed(error),
        }
    }
}

fn backgroundable_spec(mut spec: ToolSpec) -> ToolSpec {
    if let Some(properties) = spec
        .input_schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    {
        properties.insert(
            "background".into(),
            json!({
                "description": "Run immediately in the background when true, or move to the background after this many seconds. False keeps the call in the foreground.",
                "oneOf": [
                    { "type": "boolean" },
                    {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_BACKGROUND_AFTER_SECONDS
                    }
                ]
            }),
        );
    }
    spec
}

fn background_task_manager() -> AsyncTaskManager {
    AsyncTaskManager::new().routing(background_route)
}

fn background_route(request: &ToolRequest) -> RoutingDecision {
    if request.tool_name.0 != agentkit_tool_compose::COMPOSE_TOOL_NAME {
        return RoutingDecision::Foreground;
    }
    match request.input.get("background") {
        Some(Value::Bool(true)) => RoutingDecision::ForegroundThenDetachAfter(Duration::ZERO),
        Some(Value::Number(number)) => number
            .as_u64()
            .filter(|seconds| (1..=MAX_BACKGROUND_AFTER_SECONDS).contains(seconds))
            .map(|seconds| RoutingDecision::ForegroundThenDetachAfter(Duration::from_secs(seconds)))
            .unwrap_or(RoutingDecision::Foreground),
        _ => RoutingDecision::Foreground,
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

async fn drive(driver: &mut LoopDriver<KitSession>) -> Result<String, LoopError> {
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
