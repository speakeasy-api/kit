use std::collections::HashMap;
use std::error::Error;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_client_protocol::Channel;
use agent_client_protocol::schema::ProtocolVersion;
use agentkit_acp::{
    AcpAgentFactory, AcpAgentFactoryContext, AcpHeadlessRuntime, AcpRuntimeError, AutoDenyResolver,
};
use agentkit_core::{
    Delta, Item, ItemKind, MetadataMap, PartId, PartKind, SessionId as AgentkitSessionId,
    ToolOutput, ToolResultPart, Usage,
};
use agentkit_loop::{
    Agent, AgentEvent, LoopObserver, ModelAdapter, ObservedEvent, PromptCacheRequest,
    PromptCacheRetention, SessionConfig,
};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use agentkit_tools_core::{
    CompositePermissionChecker, PathPolicy, PermissionCode, PermissionDecision, PermissionDenial,
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRegistry, ToolRequest, ToolResult,
    ToolSource, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot};

type SessionOutputMap = Arc<Mutex<HashMap<agentkit_acp::SessionId, String>>>;

const MAX_ACP_PROMPT_HOPS: usize = 6;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";

const MENU: &[(&str, &str)] = &[
    (
        "Fix the subtraction bug",
        "Fix the subtraction behavior in the scratch project. Have the worker edit code and have the reviewer validate the fix.",
    ),
    (
        "Add multiplication",
        "Add a multiply(a, b) function, export it, and update the tests. Have the reviewer validate the change.",
    ),
    (
        "Harden division",
        "Change divide(a, b) so division by zero throws a clear error, and update tests. Have the reviewer validate the change.",
    ),
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();

    let workspace = seed_project()?;
    println!("[setup] scratch project: {}", workspace.display());
    println!(
        "[setup] run tests with: node {}",
        workspace.join("test.js").display()
    );

    let config = OpenRouterConfig::from_env()?
        .with_temperature(0.0)
        .with_max_completion_tokens(1200)
        .with_parallel_tool_calls(false)
        .with_app_name("agentkit-openrouter-acp-trio");
    let adapter = OpenRouterAdapter::new(config)?;

    let directory = EndpointDirectory::default();
    let worker = start_agent_endpoint(
        Role::Worker,
        adapter.clone(),
        workspace.clone(),
        directory.clone(),
    )
    .await?;
    directory.insert("worker", worker.clone());

    let reviewer = start_agent_endpoint(
        Role::Reviewer,
        adapter.clone(),
        workspace.clone(),
        directory.clone(),
    )
    .await?;
    directory.insert("reviewer", reviewer.clone());

    let orchestrator = start_agent_endpoint(
        Role::Orchestrator,
        adapter,
        workspace.clone(),
        directory.clone(),
    )
    .await?;
    directory.insert("orchestrator", orchestrator.clone());

    print_menu();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    print!("select task [1-3] or enter custom prompt (/q to quit): ");
    std::io::stdout().flush()?;

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if matches!(trimmed, "/q" | "/quit" | "/exit") {
            break;
        }
        if trimmed.is_empty() {
            print!("> ");
            std::io::stdout().flush()?;
            continue;
        }

        let prompt = selected_prompt(trimmed).unwrap_or_else(|| trimmed.to_string());
        println!("[user -> orchestrator] {}", truncate(&prompt, 180));
        match orchestrator.prompt(prompt).await {
            Ok(output) => {
                println!("\n[orchestrator final]\n{}\n", output.trim());
            }
            Err(error) => {
                eprintln!("[error] {error}");
            }
        }

        print!("follow-up (/q to quit): ");
        std::io::stdout().flush()?;
    }

    println!("[setup] scratch project left at {}", workspace.display());
    Ok(())
}

fn print_menu() {
    println!();
    for (index, (title, _)) in MENU.iter().enumerate() {
        println!("  {}) {title}", index + 1);
    }
    println!();
}

fn selected_prompt(input: &str) -> Option<String> {
    let idx = input.parse::<usize>().ok()?.checked_sub(1)?;
    MENU.get(idx).map(|(_, prompt)| (*prompt).to_string())
}

#[derive(Clone, Copy, Debug)]
enum Role {
    Orchestrator,
    Worker,
    Reviewer,
}

impl Role {
    fn id(self) -> &'static str {
        match self {
            Self::Orchestrator => "orchestrator",
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Orchestrator => "ORCH",
            Self::Worker => "WORKER",
            Self::Reviewer => "REVIEW",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Orchestrator => CYAN,
            Self::Worker => GREEN,
            Self::Reviewer => MAGENTA,
        }
    }

    fn system_prompt(self, workspace: &Path) -> String {
        let root = workspace.display();
        match self {
            Self::Orchestrator => format!(
                "\
You are the orchestrator agent for a three-agent ACP demo.
Scratch project root: {root}

Your only local authority is read-only filesystem inspection plus ACP tools.
Use fs_read_file and fs_list_directory to understand files.
Use acp_worker to request code edits.
Use acp_reviewer to request review.
Do not edit files yourself. Do not claim completion until reviewer replies with APPROVED.
For each user task: inspect, delegate to worker, ask reviewer, then summarize the final state."
            ),
            Self::Worker => format!(
                "\
You are the worker agent.
Scratch project root: {root}

You may edit files in this project using filesystem tools.
Make minimal, targeted changes. Read before editing. After edits, explain exactly what changed.
If you need review, use acp_reviewer. If you need orchestration context, use acp_orchestrator.
Do not ask the user questions unless blocked by missing requirements."
            ),
            Self::Reviewer => format!(
                "\
You are the review agent.
Scratch project root: {root}

Review the worker's changes against the requested task.
Inspect files with read-only tools. If something is wrong, call acp_worker with a precise fix request.
If the work is complete, respond with a line starting APPROVED: followed by a concise reason.
When reviewing a request from the orchestrator, signal completion by returning APPROVED directly.
Use acp_orchestrator only when you are independently coordinating a fresh task.
If you requested fixes, review again before approving."
            ),
        }
    }
}

#[derive(Clone, Default)]
struct EndpointDirectory {
    inner: Arc<Mutex<HashMap<String, AcpEndpoint>>>,
    active_hops: Arc<AtomicUsize>,
}

impl EndpointDirectory {
    fn insert(&self, name: impl Into<String>, endpoint: AcpEndpoint) {
        self.inner.lock().unwrap().insert(name.into(), endpoint);
    }

    fn get(&self, name: &str) -> Option<AcpEndpoint> {
        self.inner.lock().unwrap().get(name).cloned()
    }

    fn enter_prompt_hop(&self) -> Result<PromptHopGuard, ToolError> {
        let previous = self.active_hops.fetch_add(1, Ordering::SeqCst);
        if previous >= MAX_ACP_PROMPT_HOPS {
            self.active_hops.fetch_sub(1, Ordering::SeqCst);
            return Err(ToolError::ExecutionFailed(format!(
                "ACP prompt recursion exceeded {MAX_ACP_PROMPT_HOPS} active hops"
            )));
        }
        Ok(PromptHopGuard {
            active_hops: Arc::clone(&self.active_hops),
        })
    }
}

struct PromptHopGuard {
    active_hops: Arc<AtomicUsize>,
}

impl Drop for PromptHopGuard {
    fn drop(&mut self) {
        self.active_hops.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct AcpEndpoint {
    tx: mpsc::Sender<PromptCommand>,
}

impl AcpEndpoint {
    async fn prompt(&self, prompt: String) -> Result<String, AcpRuntimeError> {
        self.prompt_with_mode(prompt, PromptSessionMode::Persistent)
            .await
    }

    async fn prompt_fresh(&self, prompt: String) -> Result<String, AcpRuntimeError> {
        self.prompt_with_mode(prompt, PromptSessionMode::Fresh)
            .await
    }

    async fn prompt_with_mode(
        &self,
        prompt: String,
        session_mode: PromptSessionMode,
    ) -> Result<String, AcpRuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(PromptCommand {
                prompt,
                session_mode,
                response: tx,
            })
            .await
            .map_err(|_| AcpRuntimeError::ClientClosed)?;
        rx.await.map_err(|_| AcpRuntimeError::ClientClosed)?
    }
}

struct PromptCommand {
    prompt: String,
    session_mode: PromptSessionMode,
    response: oneshot::Sender<Result<String, AcpRuntimeError>>,
}

#[derive(Clone, Copy, Debug)]
enum PromptSessionMode {
    Persistent,
    Fresh,
}

async fn start_agent_endpoint(
    role: Role,
    adapter: OpenRouterAdapter,
    workspace: PathBuf,
    directory: EndpointDirectory,
) -> Result<AcpEndpoint, Box<dyn Error>> {
    let integration = agentkit_acp::AcpIntegration::builder()
        .name(format!("agentkit-{}", role.id()))
        .approval_resolver(AutoDenyResolver)
        .build()?;
    let (client_transport, agent_transport) = Channel::duplex();
    let (tx, rx) = mpsc::channel::<PromptCommand>(8);

    let factory = RoleFactory {
        role,
        adapter,
        workspace: workspace.clone(),
        directory,
    };
    tokio::spawn(async move {
        if let Err(error) = AcpHeadlessRuntime::<OpenRouterAdapter>::builder()
            .integration(integration)
            .agent_factory(factory)
            .serve_transport(agent_transport)
            .await
        {
            eprintln!("[{} acp-server] {error}", role.label());
        }
    });

    tokio::spawn(run_acp_client(role, workspace, client_transport, rx));

    Ok(AcpEndpoint { tx })
}

async fn run_acp_client(
    role: Role,
    workspace: PathBuf,
    transport: Channel,
    rx: mpsc::Receiver<PromptCommand>,
) {
    let outputs = Arc::new(Mutex::new(HashMap::new()));
    let outputs_for_notifications = Arc::clone(&outputs);
    let result = agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            async move |notification: agentkit_acp::SessionNotification, _cx| {
                render_acp_update(role, &notification, &outputs_for_notifications);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |cx| {
            cx.send_request(agentkit_acp::InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(agentkit_acp::NewSessionRequest::new(workspace.clone()))
                .block_task()
                .await?;
            process_prompt_commands(role, cx, session.session_id, rx, outputs, workspace).await
        })
        .await;

    if let Err(error) = result {
        eprintln!("[{} acp-client] {error}", role.label());
    }
}

async fn process_prompt_commands(
    role: Role,
    cx: agent_client_protocol::ConnectionTo<agent_client_protocol::Agent>,
    session_id: agentkit_acp::SessionId,
    mut rx: mpsc::Receiver<PromptCommand>,
    outputs: SessionOutputMap,
    workspace: PathBuf,
) -> Result<(), agent_client_protocol::Error> {
    while let Some(command) = rx.recv().await {
        let cx = cx.clone();
        let persistent_session_id = session_id.clone();
        let outputs = Arc::clone(&outputs);
        let workspace = workspace.clone();
        tokio::spawn(async move {
            let result =
                run_prompt_command(role, cx, persistent_session_id, command, outputs, workspace)
                    .await;
            if let Err(error) = result {
                eprintln!("[{} acp] command task failed: {error}", role.label());
            }
        });
    }
    Ok(())
}

async fn run_prompt_command(
    role: Role,
    cx: agent_client_protocol::ConnectionTo<agent_client_protocol::Agent>,
    persistent_session_id: agentkit_acp::SessionId,
    command: PromptCommand,
    outputs: SessionOutputMap,
    workspace: PathBuf,
) -> Result<(), agent_client_protocol::Error> {
    let session_id = match command.session_mode {
        PromptSessionMode::Persistent => persistent_session_id,
        PromptSessionMode::Fresh => {
            cx.send_request(agentkit_acp::NewSessionRequest::new(workspace))
                .block_task()
                .await?
                .session_id
        }
    };

    outputs
        .lock()
        .unwrap()
        .insert(session_id.clone(), String::new());
    let prompt_result = cx
        .send_request(agentkit_acp::PromptRequest::new(
            session_id.clone(),
            vec![agentkit_acp::ContentBlock::Text(
                agentkit_acp::TextContent::new(command.prompt),
            )],
        ))
        .block_task()
        .await;
    let output = take_session_output(&outputs, &session_id);
    let result = prompt_result
        .map(|_| output)
        .map_err(|error| AcpRuntimeError::Sdk(error.to_string()));

    if matches!(command.session_mode, PromptSessionMode::Fresh) {
        let _ = cx
            .send_request(agentkit_acp::CloseSessionRequest::new(session_id))
            .block_task()
            .await;
    }

    let _ = command.response.send(result);
    println!("[{} acp] turn complete", role.label());
    Ok(())
}

fn take_session_output(outputs: &SessionOutputMap, session_id: &agentkit_acp::SessionId) -> String {
    outputs
        .lock()
        .unwrap()
        .remove(session_id)
        .unwrap_or_default()
}

fn render_acp_update(
    _role: Role,
    notification: &agentkit_acp::SessionNotification,
    outputs: &SessionOutputMap,
) {
    let update = &notification.update;
    match update {
        agentkit_acp::SessionUpdate::AgentMessageChunk(chunk) => {
            if let agentkit_acp::ContentBlock::Text(text) = &chunk.content {
                outputs
                    .lock()
                    .unwrap()
                    .entry(notification.session_id.clone())
                    .or_default()
                    .push_str(&text.text);
            }
        }
        agentkit_acp::SessionUpdate::ToolCall(_)
        | agentkit_acp::SessionUpdate::ToolCallUpdate(_) => {}
        _ => {}
    }
}

#[derive(Clone)]
struct RoleFactory {
    role: Role,
    adapter: OpenRouterAdapter,
    workspace: PathBuf,
    directory: EndpointDirectory,
}

#[async_trait]
impl AcpAgentFactory<OpenRouterAdapter> for RoleFactory {
    async fn start(
        &self,
        ctx: AcpAgentFactoryContext,
    ) -> Result<
        agentkit_loop::LoopDriver<<OpenRouterAdapter as ModelAdapter>::Session>,
        AcpRuntimeError,
    > {
        let permissions =
            CompositePermissionChecker::new(PermissionDecision::Deny(PermissionDenial {
                code: PermissionCode::UnknownRequest,
                message: "tool action is not allowed in the ACP trio example".into(),
                metadata: MetadataMap::new(),
            }))
            .with_policy(
                PathPolicy::new()
                    .allow_root(self.workspace.clone())
                    .require_approval_outside_allowed(false),
            );

        let fs_resources = agentkit_tool_fs::FileSystemToolResources::new().with_policy(
            agentkit_tool_fs::FileSystemToolPolicy::new().require_read_before_write(true),
        );

        let mut builder = Agent::builder()
            .model(self.adapter.clone())
            .permissions(permissions)
            .resources(fs_resources)
            .observer(DecoratedObserver::new(self.role))
            .observer(ctx.integration.as_ref().clone())
            .transcript(vec![Item::text(
                ItemKind::System,
                self.role.system_prompt(&self.workspace),
            )])
            .cancellation(ctx.cancellation);

        match self.role {
            Role::Orchestrator => {
                builder = builder.add_tool_source(read_only_fs()).add_tool_source(
                    ToolRegistry::new()
                        .with(AcpPromptTool::new(
                            self.role,
                            "worker",
                            self.directory.clone(),
                        ))
                        .with(AcpPromptTool::new(
                            self.role,
                            "reviewer",
                            self.directory.clone(),
                        )),
                );
            }
            Role::Worker => {
                builder = builder
                    .add_tool_source(agentkit_tool_fs::registry())
                    .add_tool_source(
                        ToolRegistry::new()
                            .with(AcpPromptTool::new(
                                self.role,
                                "orchestrator",
                                self.directory.clone(),
                            ))
                            .with(AcpPromptTool::new(
                                self.role,
                                "reviewer",
                                self.directory.clone(),
                            )),
                    );
            }
            Role::Reviewer => {
                builder = builder.add_tool_source(read_only_fs()).add_tool_source(
                    ToolRegistry::new()
                        .with(AcpPromptTool::new(
                            self.role,
                            "orchestrator",
                            self.directory.clone(),
                        ))
                        .with(AcpPromptTool::new(
                            self.role,
                            "worker",
                            self.directory.clone(),
                        )),
                );
            }
        }

        builder
            .build()
            .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?
            .start(SessionConfig::new(ctx.agentkit_session_id).with_cache(
                PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
            ))
            .await
            .map_err(|error| AcpRuntimeError::Loop(error.to_string()))
    }
}

fn read_only_fs() -> impl ToolSource {
    agentkit_tool_fs::registry()
        .filtered(|name| matches!(name.0.as_ref(), "fs_read_file" | "fs_list_directory"))
}

struct DecoratedObserver {
    role: Role,
    state: Arc<Mutex<RenderState>>,
}

#[derive(Default)]
struct RenderState {
    part_kinds: HashMap<PartId, PartKind>,
    streaming_text: bool,
    pending_usage: Option<Usage>,
    usage_totals: UsageTotals,
}

#[derive(Default)]
struct UsageTotals {
    turns: u64,
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    cost_amount: f64,
    currency: Option<String>,
}

impl UsageTotals {
    fn absorb(&mut self, usage: &Usage) -> UsageDelta {
        self.turns += 1;
        let mut delta = UsageDelta::default();

        if let Some(tokens) = &usage.tokens {
            delta.input_tokens = tokens.input_tokens;
            delta.output_tokens = tokens.output_tokens;
            delta.reasoning_tokens = tokens.reasoning_tokens.unwrap_or(0);
            delta.cached_input_tokens = tokens.cached_input_tokens.unwrap_or(0);
            delta.cache_write_input_tokens = tokens.cache_write_input_tokens.unwrap_or(0);

            self.input_tokens += delta.input_tokens;
            self.output_tokens += delta.output_tokens;
            self.reasoning_tokens += delta.reasoning_tokens;
        }

        if let Some(cost) = &usage.cost {
            delta.cost_amount = cost.amount;
            delta.currency = Some(cost.currency.clone());
            self.cost_amount += cost.amount;
            self.currency = Some(cost.currency.clone());
        }

        delta
    }

    fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.reasoning_tokens
    }
}

#[derive(Default)]
struct UsageDelta {
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    cost_amount: f64,
    currency: Option<String>,
}

impl UsageDelta {
    fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.reasoning_tokens
    }
}

impl DecoratedObserver {
    fn new(role: Role) -> Self {
        Self {
            role,
            state: Arc::new(Mutex::new(RenderState::default())),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, RenderState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn finish_stream(&self) {
        let was_streaming = {
            let mut state = self.lock_state();
            let was_streaming = state.streaming_text;
            state.streaming_text = false;
            was_streaming
        };
        if was_streaming {
            println!("{RESET}");
        }
    }

    fn print_text_chunk(&self, chunk: &str) {
        let start_stream = {
            let mut state = self.lock_state();
            if state.streaming_text {
                false
            } else {
                state.streaming_text = true;
                true
            }
        };
        if start_stream {
            println!();
            println!(
                "{}{}[{}]{} assistant{}",
                self.role.color(),
                BOLD,
                self.role.label(),
                RESET,
                RESET
            );
        }
        print!("{chunk}");
        let _ = std::io::stdout().flush();
    }

    fn note_part(&self, part_id: PartId, kind: PartKind) {
        let finish_stream = {
            let mut state = self.lock_state();
            state.part_kinds.insert(part_id, kind);
            let finish_stream =
                !matches!(kind, PartKind::Text | PartKind::Reasoning) && state.streaming_text;
            if finish_stream {
                state.streaming_text = false;
            }
            finish_stream
        };
        if finish_stream {
            println!("{RESET}");
        }
    }

    fn part_kind(&self, part_id: &PartId) -> Option<PartKind> {
        self.lock_state().part_kinds.get(part_id).copied()
    }

    fn clear_parts(&self) {
        self.lock_state().part_kinds.clear();
    }

    fn remember_usage(&self, usage: Usage) {
        self.lock_state().pending_usage = Some(usage);
    }

    fn commit_usage(&self, session_id: &AgentkitSessionId, usage: Option<Usage>) {
        let mut state = self.lock_state();
        let usage = match usage {
            Some(usage) => {
                state.pending_usage = None;
                usage
            }
            None => {
                let Some(usage) = state.pending_usage.take() else {
                    return;
                };
                usage
            }
        };
        if usage.tokens.is_none() && usage.cost.is_none() {
            return;
        };
        let delta = state.usage_totals.absorb(&usage);
        let turn = state.usage_totals.turns;
        let total_tokens = state.usage_totals.total_tokens();
        let total_cost = state.usage_totals.cost_amount;
        let currency = state
            .usage_totals
            .currency
            .as_deref()
            .or(delta.currency.as_deref());

        println!(
            "{}[{}]{} usage session={} turn={} tokens +{} (in {} out {} reasoning {}) total={} {}{}",
            self.role.color(),
            self.role.label(),
            RESET,
            short_session_id(session_id),
            turn,
            delta.total_tokens(),
            delta.input_tokens,
            delta.output_tokens,
            delta.reasoning_tokens,
            total_tokens,
            format_cost_summary(delta.cost_amount, total_cost, currency),
            usage_cache_suffix(&delta)
        );
    }
}

impl LoopObserver for DecoratedObserver {
    fn handle_event(&self, event: ObservedEvent) {
        let session_id = event.session_id.clone();
        match event.event {
            AgentEvent::ToolCallRequested(call) => {
                self.finish_stream();
                println!(
                    "{}{}[{}]{} tool {}",
                    self.role.color(),
                    BOLD,
                    self.role.label(),
                    RESET,
                    call.name,
                );
                println!(
                    "{}       input {}{}",
                    DIM,
                    truncate_middle(&call.input.to_string(), 180),
                    RESET
                );
            }
            AgentEvent::ToolExecutionStarted(call) => {
                self.finish_stream();
                println!(
                    "{}[{}]{} running {}",
                    self.role.color(),
                    self.role.label(),
                    RESET,
                    call.name
                );
            }
            AgentEvent::ToolExecutionProgress(result) => {
                self.finish_stream();
                println!(
                    "{}[{}]{} progress {}",
                    self.role.color(),
                    self.role.label(),
                    RESET,
                    truncate_middle(&result.call_id.to_string(), 48),
                );
            }
            AgentEvent::ToolResultReceived(result) => {
                self.finish_stream();
                let status = if result.is_error {
                    format!("{RED}error{RESET}")
                } else {
                    format!("{GREEN}ok{RESET}")
                };
                println!(
                    "{}[{}]{} result {} {}",
                    self.role.color(),
                    self.role.label(),
                    RESET,
                    truncate_middle(&result.call_id.to_string(), 48),
                    status
                );
            }
            AgentEvent::ContentDelta(Delta::BeginPart { part_id, kind }) => {
                self.note_part(part_id, kind);
            }
            AgentEvent::ContentDelta(Delta::AppendText { part_id, chunk }) => {
                if matches!(
                    self.part_kind(&part_id),
                    Some(PartKind::Text | PartKind::Reasoning) | None
                ) {
                    self.print_text_chunk(&chunk);
                }
            }
            AgentEvent::ContentDelta(Delta::CommitPart { .. }) => self.clear_parts(),
            AgentEvent::UsageUpdated(usage) => self.remember_usage(usage),
            AgentEvent::TurnFinished(result) => {
                self.finish_stream();
                self.commit_usage(&session_id, result.usage.clone());
                println!(
                    "{}[{}]{} done {:?}",
                    self.role.color(),
                    self.role.label(),
                    RESET,
                    result.finish_reason
                );
            }
            _ => {}
        }
    }
}

#[derive(Clone)]
struct AcpPromptTool {
    caller: Role,
    target: String,
    directory: EndpointDirectory,
    spec: ToolSpec,
}

impl AcpPromptTool {
    fn new(caller: Role, target: &'static str, directory: EndpointDirectory) -> Self {
        Self {
            caller,
            target: target.into(),
            directory,
            spec: ToolSpec::new(
                ToolName::new(format!("acp_{target}")),
                format!("Send a prompt to the {target} agent over ACP and return its response."),
                json!({
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" }
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                }),
            )
            .with_annotations(ToolAnnotations::new()),
        }
    }
}

#[derive(Deserialize)]
struct AcpPromptInput {
    prompt: String,
}

#[async_trait]
impl Tool for AcpPromptTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: AcpPromptInput = serde_json::from_value(request.input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        println!();
        println!(
            "{}{}[{}]{} ACP -> {}{} {}{}",
            self.caller.color(),
            BOLD,
            self.caller.label(),
            RESET,
            self.target.to_uppercase(),
            DIM,
            truncate_middle(&input.prompt, 180),
            RESET
        );
        let endpoint = self.directory.get(&self.target).ok_or_else(|| {
            ToolError::ExecutionFailed(format!("ACP endpoint {} is not registered", self.target))
        })?;
        let _hop = self.directory.enter_prompt_hop()?;
        let output = endpoint
            .prompt_fresh(input.prompt)
            .await
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::text(output),
        )))
    }
}

fn seed_project() -> Result<PathBuf, Box<dyn Error>> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let root = std::env::temp_dir().join(format!("agentkit-acp-trio-{stamp}"));
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"agentkit-acp-trio-scratch","private":true,"scripts":{"test":"node test.js"}}"#,
    )?;
    std::fs::write(
        root.join("src/calculator.js"),
        r#"function add(a, b) {
  return a + b;
}

function subtract(a, b) {
  return a + b;
}

function divide(a, b) {
  return a / b;
}

module.exports = { add, subtract, divide };
"#,
    )?;
    std::fs::write(
        root.join("test.js"),
        r#"const assert = require("assert");
const { add, subtract, divide } = require("./src/calculator");

assert.strictEqual(add(2, 3), 5);
assert.strictEqual(subtract(10, 4), 6);
assert.strictEqual(divide(8, 2), 4);

console.log("ok");
"#,
    )?;
    std::fs::write(
        root.join("README.md"),
        "Scratch calculator project for the OpenRouter ACP trio example.\n",
    )?;
    Ok(root)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("...");
    out
}

fn truncate_middle(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    if max <= 8 {
        return truncate(s, max);
    }

    let head = (max - 3) / 2;
    let tail = max - 3 - head;
    let prefix: String = s.chars().take(head).collect();
    let suffix: String = s
        .chars()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{prefix}...{suffix}")
}

fn short_session_id(session_id: &AgentkitSessionId) -> String {
    truncate_middle(&session_id.to_string(), 32)
}

fn format_cost(amount: f64, currency: Option<&str>) -> String {
    match currency {
        Some("USD") => format!("${amount:.6}"),
        Some(currency) => format!("{amount:.6} {currency}"),
        None if amount > 0.0 => format!("{amount:.6}"),
        None => "n/a".into(),
    }
}

fn format_cost_summary(delta: f64, total: f64, currency: Option<&str>) -> String {
    if currency.is_none() && delta == 0.0 && total == 0.0 {
        "cost n/a".into()
    } else {
        format!(
            "cost +{} total {}",
            format_cost(delta, currency),
            format_cost(total, currency)
        )
    }
}

fn usage_cache_suffix(delta: &UsageDelta) -> String {
    let mut parts = Vec::new();
    if delta.cached_input_tokens > 0 {
        parts.push(format!("cache_read {}", delta.cached_input_tokens));
    }
    if delta.cache_write_input_tokens > 0 {
        parts.push(format!("cache_write {}", delta.cache_write_input_tokens));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(" "))
    }
}
