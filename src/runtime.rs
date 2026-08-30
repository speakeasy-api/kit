use std::{
    collections::{HashMap, VecDeque},
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
    CancellationController, CancellationHandle, FinishReason, Item, ItemKind, Part,
};
use agentkit_loop::{
    Agent, LoopDriver, LoopError, LoopInterrupt, LoopObserver, LoopStep, SessionConfig,
};
use agentkit_task_manager::{AsyncTaskManager, RoutingDecision, TaskManager, TaskManagerHandle};
use agentkit_tool_compose::{
    BackendRun, ComposeBackend, ComposeConfig, ComposeOutcome, ComposeTool, RunletBackend,
};
use agentkit_tool_skills::SkillRegistry;
use agentkit_tools_core::{
    PermissionRequest, Tool, ToolContext, ToolError, ToolExecutionOutcome, ToolName, ToolRequest,
    ToolResult, ToolSource, ToolSpec,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{
    acp_child::{AcpHarnesses, BUILTIN_HARNESS, ChildConfig},
    provider::{ProviderKind, SelectableAdapter, SelectableSession},
    tools::{
        A2aTool, AuthTool, CloseTool, DocsTool, EditTool, ForkTool, McpTool, Observed, PromptTool,
        ShellTool, SubagentTool, Subagents, SubagentsTool, ToolSearch, observe_shared,
    },
};

#[cfg(test)]
mod tests;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
const MAX_BACKGROUND_AFTER_SECONDS: u64 = 86_400;
const MAX_COMPOSE_RESULT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SessionRequest {
    pub id: String,
    pub resume: bool,
    pub force: bool,
}

#[derive(Default)]
struct SessionSelection {
    configured: Option<SessionRequest>,
    configured_claimed: bool,
    generated_retries: VecDeque<SessionRequest>,
}

impl SessionSelection {
    fn claim(&mut self) -> (SessionRequest, bool) {
        if !self.configured_claimed
            && let Some(request) = self.configured.clone()
        {
            self.configured_claimed = true;
            return (request, true);
        }
        if let Some(request) = self.generated_retries.pop_front() {
            return (request, false);
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

    fn claim_load(&mut self, id: &str) -> (SessionRequest, bool) {
        let matching_configured = self.configured.as_ref().filter(|request| request.id == id);
        let configured = !self.configured_claimed && matching_configured.is_some();
        let force = configured
            && matching_configured.is_some_and(|request| request.resume && request.force);
        if configured {
            self.configured_claimed = true;
        }
        (
            SessionRequest {
                id: id.into(),
                resume: true,
                force,
            },
            configured,
        )
    }

    fn finish_new(
        &mut self,
        request: &SessionRequest,
        configured: bool,
        succeeded: bool,
        opened_new: bool,
    ) {
        if configured {
            if succeeded {
                self.configured.take();
            } else if opened_new && let Some(request) = &mut self.configured {
                // Opening already persisted the bootstrap transcript. A retry
                // must resume it rather than trying to create the same file.
                request.resume = true;
            }
            self.configured_claimed = false;
        } else if !succeeded && (request.resume || opened_new) {
            let mut request = request.clone();
            request.resume = true;
            self.generated_retries.push_back(request);
        }
    }

    fn finish_load(&mut self, request: &SessionRequest, reserved: bool, succeeded: bool) {
        if succeeded
            && self
                .configured
                .as_ref()
                .is_some_and(|configured| configured.id == request.id)
        {
            self.configured.take();
        }
        if reserved {
            self.configured_claimed = false;
        }
    }
}

pub(crate) struct AcpDriverContext<I = AcpIntegration> {
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub integration: Arc<I>,
    pub cancellation: CancellationHandle,
    pub response_attempt_replacement: bool,
}

#[derive(Clone, Copy)]
enum SessionClaimKind {
    New { configured: bool, opened_new: bool },
    Load { configured: bool },
}

pub(crate) struct SessionClaim {
    runtime: Arc<Runtime>,
    request: SessionRequest,
    kind: SessionClaimKind,
    committed: bool,
}

impl SessionClaim {
    pub(crate) fn id(&self) -> &str {
        &self.request.id
    }

    pub(crate) fn is_configured(&self) -> bool {
        match self.kind {
            SessionClaimKind::New { configured, .. } | SessionClaimKind::Load { configured } => {
                configured
            }
        }
    }

    fn mark_opened(&mut self) {
        if let SessionClaimKind::New { opened_new, .. } = &mut self.kind {
            *opened_new = !self.request.resume;
        }
    }

    pub(crate) fn commit(mut self) -> Result<(), AcpRuntimeError> {
        let mut selection =
            self.runtime.session.lock().map_err(|_| {
                AcpRuntimeError::Loop("runtime session selection is poisoned".into())
            })?;
        match self.kind {
            SessionClaimKind::New {
                configured,
                opened_new,
            } => selection.finish_new(&self.request, configured, true, opened_new),
            SessionClaimKind::Load { configured } => {
                selection.finish_load(&self.request, configured, true)
            }
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for SessionClaim {
    fn drop(&mut self) {
        if !self.committed
            && let Ok(mut selection) = self.runtime.session.lock()
        {
            match self.kind {
                SessionClaimKind::New {
                    configured,
                    opened_new,
                } => selection.finish_new(&self.request, configured, false, opened_new),
                SessionClaimKind::Load { configured } => {
                    selection.finish_load(&self.request, configured, false)
                }
            }
        }
    }
}

pub(crate) struct AcpDriver {
    pub driver: LoopDriver<SelectableSession>,
    pub tasks: TaskManagerHandle,
    pub background_jobs: BackgroundJobs,
    pub structured_completion: bool,
    pub adapter: SelectableAdapter,
    pub canonical_transcript: Vec<Item>,
}

pub struct Runtime {
    root: PathBuf,
    adapter: SelectableAdapter,
    provider: ProviderKind,
    model: String,
    reasoning_effort: Option<crate::provider::ReasoningEffort>,
    credential_storage: crate::credentials::CredentialStorage,
    openrouter_api_key: Option<crate::provider::OpenRouterApiKey>,
    telemetry: crate::telemetry::Settings,
    max_subagent_depth: usize,
    base_depth: usize,
    subagents: Subagents,
    /// The explicitly selected session is consumed by the first ACP session.
    /// Later ACP sessions receive their own persisted ids.
    session: Mutex<SessionSelection>,
    mcp: crate::tools::mcp::McpRuntime,
    skills: Arc<SkillRegistry>,
    skill_package_roots: Vec<PathBuf>,
    skill_directories: Vec<PathBuf>,
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
        Self::new_with_provider_and_credentials(root, model, provider, Default::default())
    }

    pub fn new_with_provider_and_credentials(
        root: impl AsRef<Path>,
        model: impl Into<String>,
        provider: ProviderKind,
        credential_storage: crate::credentials::CredentialStorage,
    ) -> Result<Arc<Self>, String> {
        Self::new_with_provider_credentials_and_effort(
            root,
            model,
            provider,
            credential_storage,
            None,
        )
    }

    pub fn new_with_provider_credentials_and_effort(
        root: impl AsRef<Path>,
        model: impl Into<String>,
        provider: ProviderKind,
        credential_storage: crate::credentials::CredentialStorage,
        reasoning_effort: Option<crate::provider::ReasoningEffort>,
    ) -> Result<Arc<Self>, String> {
        Self::new_with_provider_credentials_effort_and_openrouter_key(
            root,
            model,
            provider,
            credential_storage,
            reasoning_effort,
            None,
        )
    }

    #[doc(hidden)]
    pub fn new_with_provider_credentials_effort_and_openrouter_key(
        root: impl AsRef<Path>,
        model: impl Into<String>,
        provider: ProviderKind,
        credential_storage: crate::credentials::CredentialStorage,
        reasoning_effort: Option<crate::provider::ReasoningEffort>,
        openrouter_api_key: Option<crate::provider::OpenRouterApiKey>,
    ) -> Result<Arc<Self>, String> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|error| format!("could not open working directory: {error}"))?;
        if !root.is_dir() {
            return Err(format!(
                "working directory is not a directory: {}",
                root.display()
            ));
        }
        let skills = build_skill_tools(&root, &[], &[]);
        let model = model.into();
        let adapter = SelectableAdapter::new_with_credentials_effort_and_openrouter_key(
            provider,
            model.clone(),
            credential_storage.clone(),
            reasoning_effort,
            openrouter_api_key.clone(),
        )?;
        let max_subagent_depth = 2;
        let subagents = Subagents::new(
            ChildConfig {
                root: root.clone(),
                model: model.clone(),
                provider,
                reasoning_effort,
                openrouter_api_key: openrouter_api_key.clone(),
                mcp_config: None,
                credential_storage: credential_storage.clone(),
                telemetry: Default::default(),
                harnesses: AcpHarnesses::default(),
                default_harness: BUILTIN_HARNESS.into(),
                parent_id: None,
                parent_name: None,
            },
            max_subagent_depth,
        );
        Ok(Arc::new(Self {
            root,
            adapter,
            provider,
            model,
            reasoning_effort,
            credential_storage,
            openrouter_api_key,
            telemetry: Default::default(),
            max_subagent_depth,
            base_depth: 0,
            subagents,
            session: Mutex::new(SessionSelection::default()),
            mcp: crate::tools::mcp::empty(),
            skills,
            skill_package_roots: Vec::new(),
            skill_directories: Vec::new(),
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
        Self::with_session_provider_and_credentials(
            root,
            model,
            provider,
            session,
            Default::default(),
        )
    }

    pub fn with_session_provider_and_credentials(
        root: impl AsRef<Path>,
        model: impl Into<String>,
        provider: ProviderKind,
        session: SessionRequest,
        credential_storage: crate::credentials::CredentialStorage,
    ) -> Result<Arc<Self>, String> {
        Self::with_session_provider_credentials_and_effort(
            root,
            model,
            provider,
            session,
            credential_storage,
            None,
        )
    }

    pub fn with_session_provider_credentials_and_effort(
        root: impl AsRef<Path>,
        model: impl Into<String>,
        provider: ProviderKind,
        session: SessionRequest,
        credential_storage: crate::credentials::CredentialStorage,
        reasoning_effort: Option<crate::provider::ReasoningEffort>,
    ) -> Result<Arc<Self>, String> {
        Self::with_session_provider_credentials_effort_and_openrouter_key(
            root,
            model,
            provider,
            session,
            credential_storage,
            reasoning_effort,
            None,
        )
    }

    #[doc(hidden)]
    pub fn with_session_provider_credentials_effort_and_openrouter_key(
        root: impl AsRef<Path>,
        model: impl Into<String>,
        provider: ProviderKind,
        session: SessionRequest,
        credential_storage: crate::credentials::CredentialStorage,
        reasoning_effort: Option<crate::provider::ReasoningEffort>,
        openrouter_api_key: Option<crate::provider::OpenRouterApiKey>,
    ) -> Result<Arc<Self>, String> {
        let mut runtime = Arc::try_unwrap(
            Self::new_with_provider_credentials_effort_and_openrouter_key(
                root,
                model,
                provider,
                credential_storage,
                reasoning_effort,
                openrouter_api_key,
            )?,
        )
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

    /// Sets private immediate-parent context inherited by this runtime's direct children.
    pub fn with_subagent_parent_context(
        runtime: Arc<Self>,
        parent: Option<(String, String)>,
    ) -> Result<Arc<Self>, String> {
        let mut runtime = Arc::try_unwrap(runtime).map_err(|_| {
            "could not configure subagent parent context after runtime was shared".to_string()
        })?;
        let mut config = runtime.subagents.child_config();
        (config.parent_id, config.parent_name) = match parent {
            Some((id, name)) => (Some(id), Some(name)),
            None => (None, None),
        };
        runtime.subagents = Subagents::new(config, runtime.max_subagent_depth);
        Ok(Arc::new(runtime))
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
        let previous = runtime.subagents.child_config();
        runtime.subagents = Subagents::new(
            ChildConfig {
                harnesses,
                default_harness,
                ..previous
            },
            runtime.max_subagent_depth,
        );
        Ok(Arc::new(runtime))
    }

    /// Propagates the host-resolved telemetry settings to nested Kit children.
    pub fn with_telemetry(
        runtime: Arc<Self>,
        telemetry: crate::telemetry::Settings,
    ) -> Result<Arc<Self>, String> {
        telemetry.agentkit_config()?;
        let mut runtime = Arc::try_unwrap(runtime)
            .map_err(|_| "could not configure telemetry after runtime was shared".to_string())?;
        runtime.telemetry = telemetry.clone();
        let previous = runtime.subagents.child_config();
        runtime.subagents = Subagents::new(
            ChildConfig {
                telemetry,
                ..previous
            },
            runtime.max_subagent_depth,
        );
        Ok(Arc::new(runtime))
    }

    /// Adds validated Agent Plugin skills to the existing project and user catalog.
    pub fn with_plugin_skills(
        runtime: Arc<Self>,
        package_roots: Vec<PathBuf>,
        skill_directories: Vec<PathBuf>,
    ) -> Result<Arc<Self>, String> {
        if package_roots.is_empty() {
            return Ok(runtime);
        }
        let mut runtime = Arc::try_unwrap(runtime)
            .map_err(|_| "could not configure plugins after runtime was shared".to_string())?;
        runtime.skills = build_skill_tools(&runtime.root, &package_roots, &skill_directories);
        runtime.skill_package_roots = package_roots;
        runtime.skill_directories = skill_directories;
        Ok(Arc::new(runtime))
    }

    /// Registers validated plugin MCP servers and overlays an optional explicit
    /// MCP file, then starts connecting all servers in the background.
    pub async fn with_mcp_config(
        runtime: Arc<Self>,
        path: Option<&Path>,
        plugin_mcps: Vec<crate::plugins::ResolvedPluginMcp>,
        interactive_oauth_enabled: bool,
        credential_storage: crate::tools::mcp::CredentialStorage,
    ) -> Result<Arc<Self>, String> {
        if path.is_none() && plugin_mcps.is_empty() {
            return Ok(runtime);
        }
        let mcp = crate::tools::mcp::connect(
            path,
            &plugin_mcps,
            interactive_oauth_enabled,
            credential_storage.clone(),
        )
        .await?;
        let mut runtime = Arc::try_unwrap(runtime)
            .map_err(|_| "could not configure MCP after runtime was shared".to_string())?;
        runtime.mcp = mcp;
        runtime.credential_storage = credential_storage.clone();
        let previous = runtime.subagents.child_config();
        runtime.subagents = Subagents::new(
            ChildConfig {
                root: runtime.root.clone(),
                model: runtime.model.clone(),
                provider: runtime.provider,
                reasoning_effort: runtime.reasoning_effort,
                openrouter_api_key: runtime.openrouter_api_key.clone(),
                mcp_config: path.map(Path::to_path_buf),
                credential_storage,
                telemetry: previous.telemetry,
                harnesses: previous.harnesses,
                default_harness: previous.default_harness,
                parent_id: previous.parent_id,
                parent_name: previous.parent_name,
            },
            runtime.max_subagent_depth,
        );
        Ok(Arc::new(runtime))
    }

    pub(crate) fn subscribe_mcp(&self, session_id: String) -> crate::tools::mcp::McpSubscription {
        self.mcp.subscribe(session_id)
    }

    pub const fn max_subagent_depth(&self) -> usize {
        self.max_subagent_depth
    }

    /// Returns the depth inherited by this runtime process.
    pub const fn base_depth(&self) -> usize {
        self.base_depth
    }

    fn agentkit_telemetry(&self) -> agentkit_loop::TelemetryConfig {
        self.telemetry
            .agentkit_config()
            .expect("runtime telemetry settings are validated before storage")
    }

    pub fn compose(self: &Arc<Self>, depth: usize) -> ComposeOnly {
        self.compose_with(depth, self.subagents.fresh())
    }

    fn fresh_skills(&self) -> Arc<SkillRegistry> {
        build_skill_tools(
            &self.root,
            &self.skill_package_roots,
            &self.skill_directories,
        )
    }

    fn compose_with(&self, depth: usize, subagents: Subagents) -> ComposeOnly {
        self.compose_with_jobs(
            depth,
            subagents,
            BackgroundJobs::default(),
            Arc::clone(&self.skills),
        )
    }

    fn compose_with_jobs(
        &self,
        depth: usize,
        subagents: Subagents,
        background_jobs: BackgroundJobs,
        skills: Arc<SkillRegistry>,
    ) -> ComposeOnly {
        let mut children = agentkit_tools_core::ToolRegistry::new()
            .with(Observed::new(DocsTool::new()))
            .with(Observed::new(ShellTool::new(self.root.clone())))
            .with(Observed::new(EditTool::new(self.root.clone())));
        if depth < self.max_subagent_depth {
            children
                .register(Observed::new(SubagentTool::new(subagents.clone(), depth)))
                .register(Observed::new(ForkTool::new(subagents.clone(), depth)));
        }
        children
            .register(Observed::new(PromptTool::new(subagents.clone())))
            .register(Observed::new(SubagentsTool::new(subagents.clone())))
            .register(Observed::new(CloseTool::new(subagents, {
                let background_jobs = background_jobs.clone();
                move |call_id, allow_pending| {
                    if allow_pending {
                        background_jobs.cancel(call_id)
                    } else {
                        background_jobs.cancel_running(call_id)
                    }
                }
            })))
            .register(Observed::new(A2aTool::new()))
            .register(Observed::new(ToolSearch::new(self.mcp.clone())))
            .register(Observed::new(AuthTool::new(self.mcp.clone())))
            .register(Observed::new(McpTool::new(self.mcp.clone())));
        let skill_tools = skills.tool_registry();
        if let Some(skill_tool) = skill_tools.get(&ToolName::new("skill")) {
            children.register(observe_shared(skill_tool));
        }
        let child_specs = children.specs();
        let compose = ComposeTool::wrap(children)
            .with_source(self.mcp.catalog().unadvertised())
            .with_config(
                ComposeConfig::new()
                    .with_max_nested_tool_calls(128)
                    .with_max_result_bytes(MAX_COMPOSE_RESULT_BYTES),
            )
            .with_backend(HiddenRunletBackend(child_specs));
        ComposeOnly {
            backgroundable: BackgroundableCompose::new(
                compose.clone(),
                background_jobs,
                self.root.clone(),
            ),
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
        let session_id = request.id.clone();
        let initial = if request.resume {
            vec![Item::text(ItemKind::System, self.system_prompt(0))]
        } else {
            self.initial_transcript(0).await.map_err(|error| {
                record_runtime_failure(
                    &session_id,
                    crate::fatal::Surface::Prompt,
                    "initial_transcript",
                    error,
                )
            })?
        };
        let opened = crate::session::open(
            &self.root,
            &request.id,
            request.resume,
            request.force,
            initial,
        )
        .map_err(|error| {
            record_runtime_failure(
                &session_id,
                crate::fatal::Surface::Prompt,
                "session_open",
                error,
            )
        })?;
        let skills = self.fresh_skills();
        let compactor = crate::compaction::automatic(
            self.adapter.clone(),
            self.agentkit_telemetry(),
            Some(opened.observer.clone()),
            format!("compaction-{}", crate::session::new_id()),
        )
        .map_err(|error| {
            record_runtime_failure(
                &session_id,
                crate::fatal::Surface::Prompt,
                "compactor_build",
                error,
            )
        })?;
        let subagents = self.subagents.fresh();
        let agent = Agent::builder()
            .model(self.adapter.clone())
            .telemetry(self.agentkit_telemetry())
            .add_tool_source(self.compose_with_jobs(
                0,
                subagents,
                BackgroundJobs::default(),
                skills,
            ))
            .task_manager(background_task_manager())
            .mutator(compactor)
            .transcript_observer(opened.observer)
            .transcript(opened.transcript)
            .input(vec![Item::text(ItemKind::User, prompt)])
            .build()
            .map_err(|error| {
                record_runtime_failure(
                    &session_id,
                    crate::fatal::Surface::Prompt,
                    "agent_build",
                    error.to_string(),
                )
            })?;
        let mut driver = match agent
            .start(SessionConfig::new(session_id.clone()).without_cache())
            .await
        {
            Ok(driver) => driver,
            Err(error) => {
                return Err(record_loop_failure(
                    &session_id,
                    crate::fatal::Surface::Prompt,
                    &error,
                ));
            }
        };
        match drive(&mut driver).await {
            Ok(output) => Ok(output),
            Err(error) => Err(record_loop_failure(
                &session_id,
                crate::fatal::Surface::Prompt,
                &error,
            )),
        }
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
        let skills = self.fresh_skills();
        let compactor = crate::compaction::automatic(
            self.adapter.clone(),
            self.agentkit_telemetry(),
            None,
            format!("compaction-{session}"),
        )
        .map_err(LoopError::InvalidState)?;
        let subagents = self.subagents.fresh();
        let mut builder = Agent::builder()
            .model(self.adapter.clone())
            .telemetry(self.agentkit_telemetry())
            .add_tool_source(self.compose_with_jobs(
                depth,
                subagents,
                BackgroundJobs::default(),
                skills,
            ))
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

    pub(crate) fn claim_session(self: &Arc<Self>) -> Result<SessionClaim, AcpRuntimeError> {
        let (request, configured) = self
            .session
            .lock()
            .map_err(|_| AcpRuntimeError::Loop("runtime session selection is poisoned".into()))?
            .claim();
        Ok(SessionClaim {
            runtime: Arc::clone(self),
            request,
            kind: SessionClaimKind::New {
                configured,
                opened_new: false,
            },
            committed: false,
        })
    }

    pub(crate) fn claim_session_load(
        self: &Arc<Self>,
        id: &str,
    ) -> Result<SessionClaim, AcpRuntimeError> {
        let (request, configured) = self
            .session
            .lock()
            .map_err(|_| AcpRuntimeError::Loop("runtime session selection is poisoned".into()))?
            .claim_load(id);
        Ok(SessionClaim {
            runtime: Arc::clone(self),
            request,
            kind: SessionClaimKind::Load { configured },
            committed: false,
        })
    }

    pub(crate) async fn start_acp_driver<I>(
        self: &Arc<Self>,
        context: AcpDriverContext<I>,
        claim: &mut SessionClaim,
    ) -> Result<AcpDriver, AcpRuntimeError>
    where
        I: LoopObserver + Clone + 'static,
    {
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
        let request = &claim.request;
        let session_id = request.id.clone();
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
        claim.mark_opened();
        // Every ACP route owns its model selection. Changing one session
        // cannot redirect another session served by the same runtime.
        let adapter = SelectableAdapter::new_with_credentials_effort_and_openrouter_key(
            self.provider,
            self.model.clone(),
            self.credential_storage.clone(),
            self.reasoning_effort,
            self.openrouter_api_key.clone(),
        )
        .map_err(AcpRuntimeError::Loop)?;
        let skills = self.fresh_skills();
        let compactor = crate::compaction::automatic(
            adapter.clone(),
            self.agentkit_telemetry(),
            Some(opened.observer.clone()),
            format!("compaction-{}", crate::session::new_id()),
        )
        .map_err(AcpRuntimeError::Loop)?;
        let subagents = self.subagents.fresh();
        let task_manager = background_task_manager();
        let tasks = task_manager.handle();
        let background_jobs = BackgroundJobs::default();
        let canonical_transcript = opened.transcript.clone();
        let mut session_config = SessionConfig::new(session_id.clone()).without_cache();
        if context.response_attempt_replacement {
            session_config = session_config.with_response_attempt_supersession();
        }
        let driver = Agent::builder()
            .model(adapter.clone())
            .telemetry(self.agentkit_telemetry())
            .add_tool_source(self.compose_with_jobs(
                self.base_depth,
                subagents,
                background_jobs.clone(),
                skills,
            ))
            .task_manager(task_manager)
            .mutator(compactor)
            .observer(context.integration.as_ref().clone())
            .transcript_observer(opened.observer)
            .transcript(opened.transcript)
            .cancellation(context.cancellation)
            .build()
            .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?
            .start(session_config)
            .await
            .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?;
        let driver = AcpDriver {
            driver,
            tasks,
            background_jobs,
            structured_completion: self.base_depth > 0,
            adapter,
            canonical_transcript,
        };
        Ok(driver)
    }

    async fn initial_transcript(&self, depth: usize) -> Result<Vec<Item>, String> {
        load_initial_transcript(&self.root, self.system_prompt(depth)).await
    }

    fn system_prompt(&self, depth: usize) -> String {
        format!(
            concat!(
                "You are a coding agent using Kit version {} as your harness, working in {}. This is your cwd and project context, not a filesystem boundary. ",
                "Make minimal changes, inspect before editing, and run the smallest useful check. ",
                "Keep tool output lean: use targeted paths, ranges, filters, and bounded `head`/`tail` output. Do not dump whole trees, generated files, long successful build logs, credential files, or environment contents.\n\n",
                "Use compose as a dependency graph: independent calls and `for` iterations run concurrently, including effectful calls; ",
                "express required ordering with data dependencies or `after`, and use `fold` only for reductions or genuinely sequential chains. ",
                "Parallelize independent work deliberately. Prefer one compose program whenever the remaining tool graph is known: keep intermediate results inside it when they can directly drive downstream work, and return only the bare minimum information necessary to plan the next turn or provide the final answer. ",
                "Background long-running compose work when it can run across a turn boundary; it also suits one-shot triggers. ",
                "Set the outer `background` argument to `true` to detach immediately or to a positive integer to wait that many seconds before detaching. ",
                "After detaching, continue any independent work, including launching more detached work. When the remaining work depends on background results, yield; yielding continues the task with those results, so the user's answer need not be completed first. ",
                "Keep work foregrounded when the next step needs its result in the current turn, and do not treat backgrounding as durable job execution.\n\n",
                "When subagent tools are available and work changes phase or objective, start fresh subagents from concise summaries of prior results instead of carrying unrelated history. ",
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

fn build_skill_tools(
    root: &Path,
    package_roots: &[PathBuf],
    skill_directories: &[PathBuf],
) -> Arc<SkillRegistry> {
    let default_roots = default_skill_roots(root);
    let canonical_defaults = default_roots
        .iter()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect::<Vec<_>>();
    let canonical_package_roots = package_roots
        .iter()
        .filter_map(|path| path.canonicalize().ok())
        .collect::<Vec<_>>();
    let canonical_plugin_skills = skill_directories
        .iter()
        .filter_map(|path| path.canonicalize().ok())
        .collect::<Vec<_>>();
    let mut roots = default_roots;
    roots.extend(skill_directories.iter().cloned());
    Arc::new(SkillRegistry::from_paths(roots).with_filter(
        move |skill: &agentkit_tool_skills::Skill| {
            let Ok(base) = skill.base_dir.canonicalize() else {
                return false;
            };
            if canonical_defaults
                .iter()
                .any(|directory| base.starts_with(directory))
            {
                return true;
            }
            let Ok(location) = skill.location.canonicalize() else {
                return false;
            };
            canonical_plugin_skills.contains(&base)
                && canonical_package_roots
                    .iter()
                    .any(|package| location.starts_with(package))
        },
    ))
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

struct BackgroundJob {
    controller: CancellationController,
    foreground_cancellation: Option<agentkit_core::TurnCancellation>,
    cancellation_relay: Option<tokio::task::AbortHandle>,
    detached: bool,
    manual_detach: bool,
    terminal_published: bool,
}

#[derive(Default)]
struct BackgroundJobState {
    running: HashMap<agentkit_core::ToolCallId, BackgroundJob>,
    pending_cancellations: std::collections::HashSet<agentkit_core::ToolCallId>,
    pending_detaches: std::collections::HashSet<agentkit_core::ToolCallId>,
    unacknowledged_terminals: std::collections::HashSet<agentkit_core::ToolCallId>,
    generation: u64,
    background_started: u64,
    cancel_all: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DetachRegistration {
    Registered,
    AlreadyDetached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BackgroundActivity {
    pub generation: u64,
    pub active: bool,
    pub background_started: u64,
    pub unacknowledged_terminals: bool,
}

#[derive(Clone)]
pub(crate) struct BackgroundJobs {
    state: Arc<Mutex<BackgroundJobState>>,
    activity: watch::Sender<u64>,
}

impl Default for BackgroundJobs {
    fn default() -> Self {
        let (activity, _) = watch::channel(0);
        Self {
            state: Arc::new(Mutex::new(BackgroundJobState::default())),
            activity,
        }
    }
}

impl BackgroundJobs {
    fn changed(&self, jobs: &mut BackgroundJobState) {
        jobs.generation = jobs.generation.wrapping_add(1);
        self.activity.send_replace(jobs.generation);
    }

    pub(crate) fn activity(&self) -> BackgroundActivity {
        self.state.lock().map_or(
            BackgroundActivity {
                generation: *self.activity.borrow(),
                active: false,
                background_started: 0,
                unacknowledged_terminals: false,
            },
            |jobs| BackgroundActivity {
                generation: jobs.generation,
                active: !jobs.running.is_empty(),
                background_started: jobs.background_started,
                unacknowledged_terminals: !jobs.unacknowledged_terminals.is_empty(),
            },
        )
    }

    pub(crate) async fn activity_after(&self, generation: u64) -> BackgroundActivity {
        let mut receiver = self.activity.subscribe();
        loop {
            let current = self.activity();
            if current.generation != generation {
                return current;
            }
            if receiver.changed().await.is_err() {
                return self.activity();
            }
        }
    }

    pub(crate) async fn wait_for_quiescence(&self) {
        loop {
            let activity = self.activity();
            if !activity.active {
                return;
            }
            let _ = self.activity_after(activity.generation).await;
        }
    }

    pub(crate) fn acknowledge_terminal(&self, call_id: &agentkit_core::ToolCallId) {
        if let Ok(mut jobs) = self.state.lock() {
            let changed = if let Some(job) = jobs.running.get_mut(call_id) {
                !std::mem::replace(&mut job.terminal_published, true)
            } else {
                jobs.unacknowledged_terminals.remove(call_id)
            };
            if changed {
                self.changed(&mut jobs);
            }
        }
    }

    pub(crate) fn begin_turn(&self) {
        if let Ok(mut jobs) = self.state.lock() {
            jobs.cancel_all = false;
        }
    }

    pub(crate) fn cancel_all(&self) {
        if let Ok(mut jobs) = self.state.lock() {
            jobs.cancel_all = true;
            for job in jobs.running.values() {
                job.controller.interrupt();
            }
        }
    }

    fn cancel_running(&self, call_id: &str) -> bool {
        let call_id = agentkit_core::ToolCallId::new(call_id);
        let Ok(jobs) = self.state.lock() else {
            return false;
        };
        let Some(job) = jobs.running.get(&call_id) else {
            return false;
        };
        job.controller.interrupt();
        true
    }

    pub fn cancel(&self, call_id: &str) -> bool {
        let call_id = agentkit_core::ToolCallId::new(call_id);
        let Ok(mut jobs) = self.state.lock() else {
            return false;
        };
        if let Some(job) = jobs.running.get(&call_id) {
            job.controller.interrupt();
        } else {
            // ACP can expose the call just before its execution future registers.
            // Remember the request so registration and cancellation are atomic
            // from the user's perspective.
            jobs.pending_cancellations.insert(call_id);
        }
        true
    }

    pub(crate) fn detach(&self, call_id: &str) -> Option<DetachRegistration> {
        let call_id = agentkit_core::ToolCallId::new(call_id);
        let Ok(mut jobs) = self.state.lock() else {
            return None;
        };
        if let Some(job) = jobs.running.get_mut(&call_id) {
            if job.manual_detach {
                return Some(DetachRegistration::AlreadyDetached);
            }
            let newly_detached = !job.detached;
            job.detached = true;
            job.manual_detach = true;
            if job
                .foreground_cancellation
                .as_ref()
                .is_some_and(agentkit_core::TurnCancellation::is_cancelled)
            {
                job.detached = false;
                job.manual_detach = false;
                job.controller.interrupt();
                return None;
            }
            if newly_detached {
                jobs.background_started = jobs.background_started.wrapping_add(1);
                self.changed(&mut jobs);
            }
            return Some(DetachRegistration::Registered);
        }
        Some(if jobs.pending_detaches.insert(call_id) {
            DetachRegistration::Registered
        } else {
            DetachRegistration::AlreadyDetached
        })
    }

    pub(crate) fn restore_foreground(&self, call_id: &str) {
        let call_id = agentkit_core::ToolCallId::new(call_id);
        let Ok(mut jobs) = self.state.lock() else {
            return;
        };
        let Some(job) = jobs.running.get_mut(&call_id) else {
            jobs.pending_detaches.remove(&call_id);
            return;
        };
        job.manual_detach = false;
        if job.foreground_cancellation.is_some() {
            job.detached = false;
        }
        if job
            .foreground_cancellation
            .as_ref()
            .is_some_and(agentkit_core::TurnCancellation::is_cancelled)
        {
            job.controller.interrupt();
        }
    }

    fn propagate_foreground_cancellation(&self, call_id: &agentkit_core::ToolCallId) {
        let Ok(jobs) = self.state.lock() else {
            return;
        };
        if let Some(job) = jobs.running.get(call_id)
            && !job.detached
        {
            job.controller.interrupt();
        }
    }

    fn finish(&self, call_id: &agentkit_core::ToolCallId) {
        if let Ok(mut jobs) = self.state.lock() {
            if let Some(job) = jobs.running.remove(call_id) {
                if job.detached && !job.terminal_published {
                    jobs.unacknowledged_terminals.insert(call_id.clone());
                }
                if let Some(relay) = job.cancellation_relay {
                    relay.abort();
                }
            }
            jobs.pending_cancellations.remove(call_id);
            jobs.pending_detaches.remove(call_id);
            self.changed(&mut jobs);
        }
    }

    #[cfg(test)]
    pub(crate) fn register_foreground_for_test(&self, call_id: &str) {
        if let Ok(mut jobs) = self.state.lock() {
            let call_id = agentkit_core::ToolCallId::new(call_id);
            let manual_detach = jobs.pending_detaches.remove(&call_id);
            let controller = CancellationController::new();
            if jobs.cancel_all {
                controller.interrupt();
            }
            jobs.running.insert(
                call_id,
                BackgroundJob {
                    controller,
                    foreground_cancellation: None,
                    cancellation_relay: None,
                    detached: manual_detach,
                    manual_detach,
                    terminal_published: false,
                },
            );
            if manual_detach {
                jobs.background_started = jobs.background_started.wrapping_add(1);
            }
            self.changed(&mut jobs);
        }
    }

    #[cfg(test)]
    pub(crate) fn finish_for_test(&self, call_id: &str) {
        self.finish(&agentkit_core::ToolCallId::new(call_id));
    }

    #[cfg(test)]
    pub(crate) fn is_cancelled_for_test(&self, call_id: &str) -> bool {
        let call_id = agentkit_core::ToolCallId::new(call_id);
        self.state.lock().is_ok_and(|jobs| {
            jobs.running
                .get(&call_id)
                .is_some_and(|job| job.controller.handle().is_cancelled_since(0))
        })
    }

    #[cfg(test)]
    pub(crate) fn is_detached_for_test(&self, call_id: &str) -> bool {
        let call_id = agentkit_core::ToolCallId::new(call_id);
        self.state.lock().is_ok_and(|jobs| {
            jobs.running.get(&call_id).is_some_and(|job| job.detached)
                || jobs.pending_detaches.contains(&call_id)
        })
    }
}

struct BackgroundJobGuard {
    jobs: BackgroundJobs,
    call_id: agentkit_core::ToolCallId,
}

impl Drop for BackgroundJobGuard {
    fn drop(&mut self) {
        self.jobs.finish(&self.call_id);
    }
}

#[derive(Clone)]
struct BackgroundableCompose {
    inner: ComposeTool,
    spec: ToolSpec,
    background_jobs: BackgroundJobs,
    root: PathBuf,
}

impl BackgroundableCompose {
    fn new(inner: ComposeTool, background_jobs: BackgroundJobs, root: PathBuf) -> Self {
        let spec = backgroundable_spec(inner.spec().clone());
        Self {
            inner,
            spec,
            background_jobs,
            root,
        }
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
        let background = background_requested(&request);
        let call_id = request.call_id.clone();
        let artifact_directory =
            crate::artifacts::directory(&self.root, &request.session_id.0, &call_id.0);
        let request = Self::sanitized(request)?;
        let _job = self.begin_background(background, &call_id, ctx);
        match self.inner.invoke(request, ctx).await {
            Ok(mut result) => {
                match crate::compose_output::guard(&artifact_directory, result.result.output).await
                {
                    Ok(output) => {
                        result.result.output = output;
                        Ok(result)
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        let background = background_requested(&request);
        let call_id = request.call_id.clone();
        let artifact_directory =
            crate::artifacts::directory(&self.root, &request.session_id.0, &call_id.0);
        let request = match Self::sanitized(request) {
            Ok(request) => request,
            Err(error) => return ToolExecutionOutcome::Failed(error),
        };
        let _job = self.begin_background(background, &call_id, ctx);
        match self.inner.invoke_outcome(request, ctx).await {
            ToolExecutionOutcome::Completed(mut result) => {
                match crate::compose_output::guard(&artifact_directory, result.result.output).await
                {
                    Ok(output) => {
                        result.result.output = output;
                        ToolExecutionOutcome::Completed(result)
                    }
                    Err(error) => ToolExecutionOutcome::Failed(error),
                }
            }
            other => other,
        }
    }
}

impl BackgroundableCompose {
    fn begin_background(
        &self,
        background: bool,
        call_id: &agentkit_core::ToolCallId,
        ctx: &mut ToolContext<'_>,
    ) -> BackgroundJobGuard {
        let foreground_cancellation = (!background).then(|| ctx.cancellation.clone()).flatten();
        let controller = CancellationController::new();
        let cancellation = controller.handle().checkpoint();
        ctx.cancellation = Some(cancellation.clone());
        if let Some(scope) = &mut ctx.execution_scope {
            scope.cancellation = Some(cancellation);
        }
        if let Ok(mut jobs) = self.background_jobs.state.lock() {
            if jobs.pending_cancellations.remove(call_id) || jobs.cancel_all {
                controller.interrupt();
            }
            let manual_detach = jobs.pending_detaches.remove(call_id);
            let detached = background || manual_detach;
            if !detached
                && foreground_cancellation
                    .as_ref()
                    .is_some_and(agentkit_core::TurnCancellation::is_cancelled)
            {
                controller.interrupt();
            }
            jobs.running.insert(
                call_id.clone(),
                BackgroundJob {
                    controller,
                    foreground_cancellation: foreground_cancellation.clone(),
                    cancellation_relay: None,
                    detached,
                    manual_detach,
                    terminal_published: false,
                },
            );
            if detached {
                jobs.background_started = jobs.background_started.wrapping_add(1);
            }
            self.background_jobs.changed(&mut jobs);
        }
        if let Some(cancellation) = foreground_cancellation {
            let jobs = self.background_jobs.clone();
            let relay_call_id = call_id.clone();
            let relay = tokio::spawn(async move {
                cancellation.cancelled().await;
                jobs.propagate_foreground_cancellation(&relay_call_id);
            })
            .abort_handle();
            if let Ok(mut jobs) = self.background_jobs.state.lock()
                && let Some(job) = jobs.running.get_mut(call_id)
            {
                job.cancellation_relay = Some(relay);
            }
        }
        BackgroundJobGuard {
            jobs: self.background_jobs.clone(),
            call_id: call_id.clone(),
        }
    }
}

fn background_requested(request: &ToolRequest) -> bool {
    matches!(
        request.input.get("background"),
        Some(Value::Bool(true) | Value::Number(_))
    )
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

fn record_runtime_failure(
    session_id: &str,
    surface: crate::fatal::Surface,
    code: &str,
    rendered: String,
) -> String {
    match crate::fatal::record_runtime_error(session_id, surface, code) {
        Ok(path) => format!("{rendered}; fatal log: {}", path.display()),
        Err(log_error) => {
            eprintln!("could not store fatal error log for {session_id}: {log_error}");
            rendered
        }
    }
}

fn record_loop_failure(
    session_id: &str,
    surface: crate::fatal::Surface,
    error: &LoopError,
) -> String {
    let rendered = crate::fatal::render_loop_error(error);
    match crate::fatal::record_loop_error(session_id, surface, error) {
        Ok(Some(path)) => format!("{rendered}; fatal log: {}", path.display()),
        Ok(None) => rendered,
        Err(log_error) => {
            eprintln!("could not store fatal error log for {session_id}: {log_error}");
            rendered
        }
    }
}

async fn drive(driver: &mut LoopDriver<SelectableSession>) -> Result<String, LoopError> {
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
