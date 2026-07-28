# agentkit-acp design

## Purpose

`agentkit-acp` is the Agent Client Protocol integration crate for agentkit.

Its primary job is to make an existing agentkit host ACP-addressable. The crate should return the loop-facing pieces a host wires into its own application: observer, input surface, cancellation handle, approval resolver, and session registry glue. That enables hybrid applications where local UI, background work, and ACP can all feed and observe the same agent sessions.

A fully headless ACP agent is an optional convenience layer built from those same parts:

```rust,ignore
use agentkit_acp::{AcpHeadlessRuntime, AcpIntegration, ClientPermissionResolver};

let integration = AcpIntegration::builder()
    .name("agentkit")
    .approval_resolver(ClientPermissionResolver::new())
    .build()?;

AcpHeadlessRuntime::builder()
    .agent_factory(agent_factory)
    .integration(integration)
    .serve_stdio()
    .await?;
```

ACP standardizes communication between code editors and coding agents. The local agent usually runs as an editor child process over JSON-RPC on stdio, with remote transports still developing. The protocol includes session lifecycle, prompt turns, streamed updates, tool call reporting, optional file and terminal client callbacks, authentication, and extensibility.

There is already an official Rust ACP SDK:

- `agent-client-protocol`
- `agent-client-protocol-schema`
- `agent-client-protocol-tokio`
- `agent-client-protocol-rmcp`

So agentkit should not build a parallel protocol crate by default. A separate `racp` crate only makes sense if the official SDK becomes unsuitable. The first implementation should mirror `agentkit-mcp`: re-export upstream wire types and make agentkit own only the lifecycle and conversion glue.

## Non-goals

`agentkit-acp` should not own:

- the ACP schema or JSON-RPC framing
- provider-specific model behavior
- the generic agent loop
- the tool registry or executor contracts
- a hosted remote agent platform
- a complete editor UI
- long-term session persistence
- policy storage for remembered approval choices

The crate integrates agentkit into ACP. It should not fork ACP or turn agentkit into a single opinionated coding-agent product.

## Dependencies

Recommended initial crate:

```toml
[dependencies]
agent-client-protocol = "1.0.0"
agent-client-protocol-tokio = { version = "0.11.1", optional = true }
agentkit-core = { path = "../agentkit-core", version = "0.10.2" }
agentkit-loop = { path = "../agentkit-loop", version = "0.10.2" }
agentkit-tools-core = { path = "../agentkit-tools-core", version = "0.10.2" }
async-trait = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true, features = ["sync"] }
tracing = { workspace = true }

[features]
default = ["stdio"]
stdio = ["dep:agent-client-protocol-tokio"]
unstable-acp = ["agent-client-protocol/unstable"]
```

The umbrella crate should later add:

```toml
acp = ["dep:agentkit-acp", "loop"]
```

and re-export:

```rust,ignore
#[cfg(feature = "acp")]
pub use agentkit_acp as acp;
```

## Design principles

### 1. Use upstream ACP wire types directly

Like `agentkit-mcp`, this crate should avoid a second protocol vocabulary. Requests, responses, content blocks, session updates, tool call payloads, and client callback payloads should be re-exported from `agent-client-protocol`.

Agentkit-owned types should be limited to runtime configuration, adapters, errors, session storage, and resolver traits.

### 2. Full observer session addressing is a Phase 0 prerequisite

Before `agentkit-acp` should be implemented, agentkit's observer surfaces should expose session context consistently:

- `LoopObserver` should receive an observed-event envelope containing `session_id` and `AgentEvent`.
- transcript observation should receive a transcript event containing `session_id` and `Item`, or otherwise expose session context.

This should be a separate breaking `agentkit-loop` change before `agentkit-acp` starts. Prefer an envelope over restructuring every `AgentEvent` variant:

```rust,ignore
pub struct ObservedEvent {
    pub session_id: agentkit_core::SessionId,
    pub event: AgentEvent,
}

impl agentkit_loop::LoopObserver for AcpIntegration {
    fn handle_event(&self, event: ObservedEvent) {
        self.route_event(&event.session_id, event.event);
    }
}
```

An envelope keeps the `AgentEvent` enum shape stable for match sites and concentrates the break in the observer trait signature. The same idea applies to transcript observation: replace item-only callbacks with a session-addressed transcript event.

Without full session addressing, ACP can still work by creating session-scoped observer adapter instances, but that forces a less consistent API and prevents the primary integration object from being passed around like other agentkit trait implementations.

The session id does not remove the need for per-session ACP state. It only makes routing possible from a shared observer. The integration still stores ACP session id mappings, client handles, cancellation controllers, delta reconstruction state, synthesized ACP message ids, and prompt channels per session.

### 3. Hybrid integration is the primitive

`agentkit-acp` should first expose components that plug into a host-owned loop:

- a shared `LoopObserver` implementation that converts session-addressed agentkit events into ACP notifications
- a `TranscriptObserver` hook when exact transcript persistence or replay is needed
- an input surface that converts ACP prompt requests into agentkit `Item`s
- a cancellation handle for ACP cancel notifications
- an approval bridge that resolves agentkit approval interrupts through ACP permission requests
- a session registry that binds ACP session ids to host sessions

The observer is intentionally not responsible for mutation or control. It does not submit input, resolve approvals, or cancel turns. Input goes through the host's turn arbiter, approvals go through `LoopStep::Interrupt`, and cancellation goes through a separately wired `CancellationController`.

The headless runtime helper owns the agent factory, session table, turn arbiter, and stdio serving loop for applications that do not already have these pieces.

### 4. ACP sessions map to loop drivers in the headless helper

ACP session lifecycle should map to agentkit loop drivers:

- `session/new` starts an `agentkit-loop::Agent` with `SessionConfig`
- `session/prompt` submits one user turn into that driver's pending input
- streamed `AgentEvent`s become ACP `session/update` notifications
- `LoopStep::Finished` becomes `PromptResponse`
- `session/cancel` interrupts the session's cancellation controller
- `session/close` drops the driver if the stable ACP protocol supports it

The runtime should keep a session table:

```rust,ignore
struct AcpHeadlessRuntimeState<M>
where
    M: agentkit_loop::ModelAdapter,
{
    sessions: HashMap<acp::SessionId, AcpSession<M::Session>>,
}

struct AcpSession<S> {
    acp_session_id: acp::SessionId,
    agentkit_session_id: agentkit_core::SessionId,
    cwd: PathBuf,
    driver: agentkit_loop::LoopDriver<S>,
}
```

Each session should own or reference a `Mutex<LoopDriver<S>>` rather than holding a runtime-wide lock across a prompt turn. A slow or approval-blocked session must not serialize unrelated sessions.

### 5. The initial runtime should be local-first

Phase 1 should support stdio through `agent-client-protocol-tokio::Stdio`. HTTP and WebSocket transports should wait until the ACP transport working group and upstream SDK settle the server-side ergonomics.

### 6. Client callbacks are host boundaries

ACP clients can expose filesystem, terminal, permission, and auth methods to the agent. Agentkit has local filesystem and shell tools, plus tool approvals, but those are not the same boundary:

- agentkit tools run in the host process or via MCP
- ACP filesystem and terminal methods are requests from the agent runtime back to the editor client
- ACP permission prompts are UI-facing RPC requests

The first crate can expose the runtime without implementing all client callback surfaces. Permission is the exception because it is required to make agentkit's approval interrupts usable in ACP clients.

## Public API sketch

### Hybrid integration

```rust,ignore
pub struct AcpIntegration {
    // private
}

impl AcpIntegration {
    pub fn builder() -> AcpIntegrationBuilder;

    pub fn bind_session(&self, binding: AcpSessionBinding) -> Result<AcpSessionHandle, AcpRuntimeError>;
    pub fn cancellation_handle(&self, session_id: &acp::SessionId) -> agentkit_core::CancellationHandle;
    pub fn interrupt_session(&self, session_id: &acp::SessionId) -> Result<(), AcpRuntimeError>;
    pub fn input_port(&self) -> AcpInputPort;
    pub fn session_registry(&self) -> AcpSessionRegistry;
}

impl agentkit_loop::LoopObserver for AcpIntegration {
    fn handle_event(&self, event: agentkit_loop::ObservedEvent) {
        // Routes by event.session_id into per-session ACP state.
    }
}

pub struct AcpIntegrationBuilder {
    // private
}

impl AcpIntegrationBuilder {
    pub fn name(self, name: impl Into<String>) -> Self;
    pub fn version(self, version: impl Into<String>) -> Self;
    pub fn approval_resolver(self, resolver: impl AcpApprovalResolver) -> Self;
    pub fn approval_memory(self, memory: impl AcpApprovalMemory) -> Self;
    pub fn build(self) -> Result<AcpIntegration, AcpRuntimeError>;
}
```

Hybrid hosts wire the returned pieces into their existing loop:

```rust,ignore
let acp = Arc::new(AcpIntegration::builder()
    .name("agentkit")
    .approval_resolver(ClientPermissionResolver::new())
    .build()?);

let session = acp.bind_session(AcpSessionBinding::new(
    acp_session_id,
    agentkit_session_id,
    client_connection,
))?;

let agent = Agent::builder()
    .model(adapter)
    .observer(acp.clone())
    .cancellation(session.cancellation_handle())
    .build()?;
```

The host remains the turn arbiter. ACP prompt requests are another input source; they do not automatically own the loop.

### Headless helper

```rust,ignore
pub struct AcpHeadlessRuntime<M>
where
    M: agentkit_loop::ModelAdapter,
{
    // private
}

impl<M> AcpHeadlessRuntime<M>
where
    M: agentkit_loop::ModelAdapter + Send + Sync + 'static,
    M::Session: Send + 'static,
{
    pub fn builder() -> AcpHeadlessRuntimeBuilder<M>;
}

impl<M> AcpHeadlessRuntimeBuilder<M>
where
    M: agentkit_loop::ModelAdapter + Send + Sync + 'static,
    M::Session: Send + 'static,
{
    pub fn agent_factory(self, factory: impl AcpAgentFactory<M>) -> Self;
    pub fn integration(self, integration: AcpIntegration) -> Self;

    #[cfg(feature = "stdio")]
    pub async fn serve_stdio(self) -> Result<(), AcpRuntimeError>;
}
```

The headless helper should take an agent factory, an `AgentBuilder`, or another template that lets it construct agents with `Arc<AcpIntegration>` wired as the observer before `Agent::start`. It should not take only an already-built `Agent<M>` if it needs ACP streaming. Its canonical configuration should be `agent_factory(...)` plus `integration(...)`; convenience passthroughs may be added later, but the integration object is the source of truth.

Each factory invocation should receive the ACP session id, the agentkit session id, `cwd`, additional workspace roots, the shared `Arc<AcpIntegration>`, the session cancellation handle, and session metadata. That is the load-bearing contract that lets the factory wire observer, transcript observer, cancellation, workspace metadata, tools, context, and model adapter configuration before calling `Agent::start`.

Internally, the headless helper wires the upstream ACP `Agent.builder()` handlers:

- `initialize`
- `session/new`
- `session/prompt`
- `session/cancel`
- `session/close` if stable ACP v1 supports it
- optionally `session/list`, `session/resume`, and `session/delete` once persistence exists

Confirm the exact stable request and enum names against the upstream `agent-client-protocol` crate before implementation. Anything not in stable ACP v1 should be gated behind `unstable-acp` or implemented as an agentkit extension. This includes lifecycle methods and stop-reason names such as `EndTurn`, `MaxTokens`, `Cancelled`, and `Refusal`.

## Protocol mapping

### Initialize

Handle `InitializeRequest` and return `InitializeResponse` with:

- the requested protocol version when supported
- basic `AgentCapabilities`
- agent implementation metadata

Advertise only stable capabilities that are actually implemented. In phase 1:

- text prompts
- session lifecycle needed for new/prompt/cancel, plus close if stable ACP supports it
- no file system client callbacks
- no terminal client callbacks
- no ACP auth methods unless a host resolver is installed

### New session

Handle `NewSessionRequest`:

1. Generate or convert an agentkit `SessionId` from the ACP `SessionId`.
2. Invoke the agent factory with session context: ACP session id, agentkit session id, `cwd`, additional workspace roots, shared `Arc<AcpIntegration>`, cancellation handle, and session metadata.
3. Call `Agent::start` on the agent returned by the factory.
4. Store the returned `LoopDriver`.
5. Return `NewSessionResponse`.

The `cwd` and `additional_directories` should be copied into session metadata so model adapters, tools, context loaders, or future filesystem policy can inspect them.

### Prompt

Handle `PromptRequest`:

1. Convert ACP prompt content into agentkit input items.
2. Submit the items to the stored `LoopDriver`.
3. Drive the loop until it returns `LoopStep::Finished`.
4. Convert the finish reason to ACP `StopReason`.
5. Return `PromptResponse`.

Minimum content conversion:

```text
ACP ContentBlock::Text         -> ItemKind::User + Part::Text
ACP ContentBlock::ResourceLink -> ItemKind::Context or Part::Custom metadata placeholder
ACP ContentBlock::Resource     -> ItemKind::Context where possible
ACP Image/Audio                -> Part::Media when supported by the provider path
```

Unsupported content should return a structured ACP error rather than silently dropping user input.

### Streaming updates

ACP streaming depends on observed events carrying `session_id`. With that in place, `AcpIntegration` can implement `LoopObserver` directly and route events into per-session ACP state. In the headless helper this means constructing the agent through an agent factory or builder template with `Arc<AcpIntegration>` registered before `Agent::start`. In hybrid integrations, the host wires the same shared integration object into its own `AgentBuilder`.

Per-session state remains internal to `AcpIntegration`. The shared observer routes by `session_id`, then updates that session's delta reconstruction state and sends events into that session's notification path.

`LoopObserver::handle_event` is synchronous, so the ACP observer should bridge to async JSON-RPC sending with a `tokio::sync::mpsc::UnboundedSender`. The prompt handler or host event task drains that channel and sends ACP `SessionNotification`s.

Convert events:

```text
Delta::BeginPart { part_id, kind }       -> remember part kind for later chunks
Delta::AppendText for PartKind::Text     -> SessionUpdate::AgentMessageChunk
Delta::AppendText for PartKind::Reasoning -> SessionUpdate::AgentThoughtChunk
Delta::CommitPart                        -> finalize local reconstruction state
AgentEvent::ToolCallRequested            -> SessionUpdate::ToolCall
AgentEvent::ToolExecutionStarted         -> SessionUpdate::ToolCallUpdate { status = InProgress }
AgentEvent::ToolExecutionProgress        -> SessionUpdate::ToolCallUpdate { status = InProgress }
AgentEvent::ToolResultReceived           -> terminal SessionUpdate::ToolCallUpdate
AgentEvent::UsageUpdated                 -> SessionUpdate::UsageUpdate only when context window size is known
AgentEvent::Warning                      -> log or metadata update, not AgentThoughtChunk
AgentEvent::RunFailed                    -> prompt error
AgentEvent::TurnFinished                 -> PromptResponse stop_reason
```

Tool call mapping should preserve the agentkit `ToolCallId` as ACP `ToolCallId`. Tool inputs should be exposed as `raw_input`, and tool outputs as `raw_output` plus text content when available.

The observer must be stateful. `Delta::AppendText` only carries `part_id` and `chunk`; the observer must remember the earlier `Delta::BeginPart { part_id, kind }` to decide whether the chunk is assistant message text or reasoning. The observer may synthesize ACP `message_id`s per turn because agentkit deltas identify parts rather than ACP messages.

`AgentEvent::ApprovalRequired` is informational on the observer stream. The runtime must not double-prompt from that event. Approval resolution is owned by `LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(...))`.

### Stop reasons

Recommended mapping, subject to verifying the exact stable upstream ACP names:

```text
FinishReason::Completed       -> StopReason::EndTurn
FinishReason::MaxTokens       -> StopReason::MaxTokens
FinishReason::Cancelled       -> StopReason::Cancelled
FinishReason::Blocked         -> StopReason::Refusal
FinishReason::Error           -> JSON-RPC error
FinishReason::ToolCall        -> continue driving, not a final ACP stop
FinishReason::Other(_)        -> StopReason::EndTurn with metadata
```

`FinishReason::ToolCall` is an internal agentkit turn boundary. The ACP prompt call should continue through tool execution until the agentkit loop finishes or blocks on approval/cancellation.

## Approval resolver abstraction

Agentkit approval interrupts are loop-level blocking interrupts:

```rust,ignore
LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending))
```

ACP permission prompts are client-facing JSON-RPC requests:

```text
session/request_permission
```

`agentkit-acp` needs a resolver layer between those two worlds.

### Resolver trait

```rust,ignore
#[async_trait]
pub trait AcpApprovalResolver: Send + Sync + 'static {
    async fn resolve(
        &self,
        ctx: AcpApprovalContext,
        client: AcpClientHandle,
    ) -> Result<AcpApprovalDecision, AcpRuntimeError>;
}
```

Context:

```rust,ignore
pub struct AcpApprovalContext {
    pub acp_session_id: acp::SessionId,
    pub agentkit_session_id: agentkit_core::SessionId,
    pub request: agentkit_tools_core::ApprovalRequest,
    pub tool_call: Option<acp::ToolCallUpdate>,
}
```

`AcpClientHandle` is a cloneable wrapper around the upstream `ConnectionTo<Client>` or equivalent send capability. It is passed per resolution so `ClientPermissionResolver::new()` can stay stateless and can still send `session/request_permission` for the current client connection.

Decision:

```rust,ignore
pub enum AcpApprovalDecision {
    AllowOnce,
    AllowAlways,
    RejectOnce { reason: Option<String> },
    RejectAlways { reason: Option<String> },
    PatchAndAllow {
        input: serde_json::Value,
    },
}
```

Built-in resolvers:

```rust,ignore
pub struct AutoApproveResolver;
pub struct AutoDenyResolver;
pub struct ClientPermissionResolver;
```

`AutoApproveResolver` and `AutoDenyResolver` are useful for tests and non-interactive hosts. `ClientPermissionResolver` is the standard resolver for interactive ACP clients, but hosts should still opt into it explicitly.

### ClientPermissionResolver

`ClientPermissionResolver` should:

1. Convert `agentkit_tools_core::ApprovalRequest` into ACP `RequestPermissionRequest`.
2. Send the request through the `AcpClientHandle` for this session.
3. Await `RequestPermissionResponse`.
4. Convert the selected ACP `PermissionOptionId` into `AcpApprovalDecision`.
5. Return the decision to the runtime.

Default ACP options:

```text
allow_once    -> PermissionOptionKind::AllowOnce
allow_always  -> PermissionOptionKind::AllowAlways
reject_once   -> PermissionOptionKind::RejectOnce
reject_always -> PermissionOptionKind::RejectAlways
```

If patched input is supported later, expose it through ACP `_meta` or an extension method first. Do not overload stable ACP permission options with agentkit-specific JSON without a documented compatibility story.

### Applying decisions

The runtime applies resolver decisions to the loop driver:

```rust,ignore
match decision {
    AcpApprovalDecision::AllowOnce | AcpApprovalDecision::AllowAlways => {
        driver.resolve_approval_for(call_id, ApprovalDecision::Approve)?;
    }
    AcpApprovalDecision::RejectOnce { reason }
    | AcpApprovalDecision::RejectAlways { reason } => {
        driver.resolve_approval_for(call_id, ApprovalDecision::Deny { reason })?;
    }
    AcpApprovalDecision::PatchAndAllow { input } => {
        send ToolCallUpdate { raw_input: input.clone() };
        driver.resolve_approval_for_with_patched_input(call_id, input)?;
    }
}
```

`call_id` should come from `ApprovalRequest::call_id`. If no call id is present and exactly one approval is pending, the runtime may use `resolve_approval`; otherwise it should return an internal state error.

### Approval memory

Remembered decisions should be separate from the resolver:

```rust,ignore
pub trait AcpApprovalMemory: Send + Sync + 'static {
    fn lookup(
        &self,
        session_id: &agent_client_protocol::schema::v1::SessionId,
        request: &agentkit_tools_core::ApprovalRequest,
    ) -> Option<AcpApprovalDecision>;

    fn remember(
        &self,
        session_id: &agent_client_protocol::schema::v1::SessionId,
        request: &agentkit_tools_core::ApprovalRequest,
        decision: &AcpApprovalDecision,
    );
}
```

The runtime checks memory before calling the resolver:

```text
memory lookup hit       -> apply remembered decision
memory lookup miss      -> call resolver
AllowAlways/RejectAlways -> remember after applying
AllowOnce/RejectOnce     -> do not remember
PatchAndAllow            -> never remember patched input
```

Initial implementations:

- `NoopApprovalMemory`
- `InMemoryApprovalMemory`

Persistent trust stores should remain host-owned until the policy key shape is proven. A durable memory entry needs a stable key that includes at least request kind, tool name or call target, workspace root, and risk metadata.

### Interaction with permission policy

The resolver does not replace `agentkit-tools-core` permission checks. Permission policy still decides whether an action is allowed, denied, or requires approval. The ACP resolver only decides how to resolve an already-surfaced approval request.

This keeps boundaries clean:

- tools and policy decide whether approval is required
- the loop pauses execution
- `agentkit-acp` asks the ACP client for a decision
- the loop resumes with approve, deny, or patched input

## Runtime prompt loop

In the headless helper, pseudo-flow for one ACP `session/prompt`:

```rust,ignore
fn handle_prompt(request: PromptRequest, cx: ConnectionTo<Client>) {
    let session = sessions.get(&request.session_id)?;
    let items = convert_prompt(request.prompt)?;
    let mut driver = session.driver.lock().await;
    driver.submit_input(items)?;

    loop {
        match driver.next().await? {
            LoopStep::Finished(result) => {
                return Ok(PromptResponse::new(stop_reason(result.finish_reason)));
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                let ctx = approval_context(session, &pending.request);
                let generation = session.cancellation.handle().generation();
                let decision = tokio::select! {
                    decision = resolve_with_memory(ctx, session.client.clone()) => decision?,
                    _ = session.cancellation.handle().cancelled_since(generation) => {
                        driver.cancel_pending_approvals().await?;
                        return Ok(PromptResponse::new(StopReason::Cancelled));
                    }
                };
                apply_approval_decision(&mut driver, &pending.request, decision)?;
            }
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                return Ok(PromptResponse::new(StopReason::EndTurn));
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {
                continue;
            }
        }
    }
}
```

The session's observer sends event notifications through an mpsc bridge to the ACP client. Approval notifications from that stream are display-only; the driver interrupt is the only channel that resolves the approval.

ACP `session/cancel` should interrupt the session's `CancellationController`. The integration owns one controller per session, hands `controller.handle()` to `AgentBuilder::cancellation(...)`, and calls `controller.interrupt()` when ACP cancellation arrives. Cancellation is not an observer responsibility, but it is a Phase 1 runtime responsibility because `StopReason::Cancelled` is otherwise unreachable.

Approval waits must be cancellation-aware. A pending `session/request_permission` round trip can block on the user; the runtime should race the resolver future against the session cancellation handle. If cancellation wins, stop waiting for the permission response and resume the driver so the loop can observe cancellation and finish with `FinishReason::Cancelled`.

The `cancel_pending_approvals` call in the pseudocode clears every unresolved approval interrupt for the cancelled turn and appends matching error tool results so the transcript remains provider-valid.

In hybrid integrations, the host's turn arbiter decides what to do when ACP input arrives:

```text
idle between turns       -> submit input and drive a new turn
running a turn           -> reject busy, enqueue next, or cancel/restart
AfterToolResult yield    -> optionally interject via ToolRoundInfo::submit
awaiting approval        -> route through approval resolver, not prompt input
```

`agentkit-acp` should provide input-port helpers for these policies, but it should not force one global queueing strategy on host applications.

## Phased implementation

### Phase 0: loop prerequisites

- change `LoopObserver` to receive a session-addressed observed-event envelope, for example `ObservedEvent { session_id, event }`
- change transcript observation to receive a session-addressed transcript event, for example `TranscriptEvent { session_id, item }`
- add `AgentEvent::ToolExecutionStarted` or equivalent so ACP can report `InProgress` without inference
- add loop support for cancellation while an approval interrupt is pending, either by prioritizing cancellation before unresolved approvals or by exposing a method to cancel/clear a pending approval interrupt
- update in-repo observer implementations and tests for the breaking trait changes

### Phase 1: minimal ACP runtime

- add `crates/agentkit-acp`
- add workspace and umbrella feature entries
- re-export ACP wire types from the crate root, matching `agentkit-mcp`
- implement `AcpIntegrationBuilder`
- implement `LoopObserver` for `AcpIntegration` with stateful per-session delta reconstruction
- implement `AcpHeadlessRuntimeBuilder` as a helper over `AcpIntegration`
- own a `CancellationController` per ACP session and pass its handle into the agent factory
- implement stdio serving
- implement `initialize`
- implement `session/new`
- implement `session/prompt` for text prompts
- implement `session/cancel`
- stream assistant text chunks
- map final stop reasons
- add an example binary that serves an OpenRouter-backed ACP agent

### Phase 2: approval bridge

- add `AcpApprovalResolver`
- add `AutoApproveResolver`, `AutoDenyResolver`, and `ClientPermissionResolver`
- add `AcpApprovalMemory`
- add `AcpClientHandle` or equivalent connection wrapper for resolver calls
- require an explicit approval resolver choice in builders; do not silently default to client prompting or auto-approval
- map agentkit `ApprovalRequest` to ACP `RequestPermissionRequest`
- add tests for allow, deny, remembered allow, remembered deny, and missing call id

### Phase 3: tool call UX

- stream `ToolCallRequested` as ACP tool call creation
- stream `ToolExecutionStarted`, `ToolExecutionProgress`, and `ToolResultReceived` as ACP tool call updates
- include `raw_input`, `raw_output`, status, and text content
- map common file-edit tool outputs to ACP diffs where possible

### Phase 4: session lifecycle and persistence hooks

- implement `session/close` if stable ACP supports it
- preserve transcript snapshots through `LoopDriver::snapshot`
- include ACP session metadata updates for title and timestamps

### Phase 5: optional client callbacks

- add filesystem client callback traits for ACP `fs/read_text_file` and `fs/write_text_file`
- add terminal callback traits for ACP `terminal/*`
- decide how these relate to `agentkit-tool-fs` and `agentkit-tool-shell`
- add auth resolver hooks if ACP auth is needed for hosted agents

## Decisions

- `agentkit-acp` does not own session persistence. Hosts can already extract transcript state from `LoopDriver::snapshot` and persist it in their own storage layer.
- Builder APIs should force an explicit approval resolver choice. A host should choose `ClientPermissionResolver`, `AutoDenyResolver`, `AutoApproveResolver`, or a custom resolver intentionally.
- ACP reporting should be implemented through `AcpIntegration` as `LoopObserver` plus transcript observation. If this proves insufficient during implementation, the gap should be fixed in the observer events rather than by adding a separate reporting subsystem.
- `agentkit-loop` provides explicit tool execution start/progress events so ACP can report `ToolCallStatus::InProgress` without guessing from approval or result timing.

## Remaining design details

### Workspace context from ACP `cwd`

ACP `NewSessionRequest` carries a `cwd` and may carry additional workspace roots. There are two different responsibilities here:

- session metadata should preserve `cwd` and additional roots so model adapters, tools, policy, and host code can inspect them;
- context loading from those paths should remain host-configurable, because `agentkit-context` can be expensive, policy-sensitive, and application-specific.

Recommended default: `agentkit-acp` records workspace roots in session metadata and exposes helpers that let hosts opt into `agentkit-context` discovery from ACP roots. It should not automatically read project files or `AGENTS.md` in the minimal runtime without an explicit host choice.

The headless helper may offer an opt-in:

```rust,ignore
AcpHeadlessRuntime::builder()
    .agent_factory(agent_factory)
    .integration(integration)
    .load_context_from_acp_roots(true)
```

but the default should be metadata-only.

### Unsupported media prompts

ACP prompts can include text, resource links, embedded resources, images, and audio. Agentkit can represent media with `Part::Media`, but not every model adapter can consume every modality.

Reasonable options:

- **Strict error:** return a structured ACP error when the prompt contains unsupported media. This is the safest default because the user knows their input was not processed.
- **Metadata-only degrade:** preserve unsupported media as `Part::Custom` or session metadata and add a text notice saying the runtime could not process it. This is useful for hosts that want auditability, but it risks surprising users if treated as success.
- **Resource-link fallback:** when media has a URI, pass it as a resource link or context item instead of inline bytes. This is useful only if the downstream model/tools can access that URI.
- **Host converter:** let the host install a conversion hook, for example image OCR, audio transcription, or file upload to a provider-specific asset store.

Recommended default for Phase 1: strict unsupported-content errors for non-text media. Add host converters later rather than silently dropping or weakening content.

### Approval memory keys

`AllowAlways` and `RejectAlways` require a stable memory key. The nuance is that a raw `ApprovalRequest` includes per-call details that should not all be part of a durable trust decision. If the key is too broad, one approval may authorize unrelated future work. If it is too narrow, "always" behaves like "once".

The key should be explicit and policy-owned. A good default key should include:

- request kind;
- tool name or permission target;
- workspace root or ACP session scope;
- normalized resource path, command family, MCP server id, or other risk target when present;
- relevant risk metadata, excluding volatile call ids, task ids, timestamps, and free-form model text.

`AcpApprovalMemory` should therefore not derive durable keys implicitly from the full request without a documented key builder. Start with in-memory examples, then add a host-provided `AcpApprovalKeyer` if persistent approval memory becomes part of the crate.

## References

- ACP introduction: <https://agentclientprotocol.com/get-started/introduction>
- ACP Rust SDK: <https://agentclientprotocol.com/libraries/rust>
- ACP protocol docs: <https://agentclientprotocol.com/protocol/v1/overview>
- Rust crate docs: <https://docs.rs/agent-client-protocol/1.0.0/agent_client_protocol/>
- Existing MCP integration design: [`mcp.md`](./mcp.md)
- Agentkit permission design: [`permissions.md`](./permissions.md)
