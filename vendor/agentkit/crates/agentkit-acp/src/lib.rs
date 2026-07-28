//! Agent Client Protocol integration for agentkit hosts.
//!
//! This crate builds on the upstream [`agent_client_protocol`] SDK. It does not
//! define a parallel ACP schema; protocol wire types are re-exported from the
//! upstream crate and agentkit owns only host-facing lifecycle, routing, and
//! conversion glue.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::{Client, ConnectionTo, Handled};
use agentkit_core::{
    CancellationController, CancellationHandle, DataRef, Delta, FilePart, FinishReason, Item,
    ItemKind, MediaPart, MetadataMap, Modality, Part, PartId, PartKind,
    SessionId as AgentkitSessionId, StructuredPart, TextPart, ToolCallPart, ToolOutput,
    ToolResultPart, Usage as CoreUsage,
};
use agentkit_loop::{
    AgentEvent, LoopInterrupt, LoopObserver, LoopStep, ModelAdapter, ObservedEvent,
};
use agentkit_tools_core::{ApprovalDecision, ApprovalRequest};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

/// Re-export of the upstream ACP SDK.
pub use agent_client_protocol as sdk;
/// Stable upstream ACP v1 wire types.
pub use agent_client_protocol::schema::v1::*;

/// Re-exports of the stable upstream ACP v1 wire types.
///
/// Use these types directly for JSON-RPC payloads, content blocks, session
/// updates, tool call payloads, and client callback payloads.
pub mod wire {
    pub use agent_client_protocol::schema::v1::*;
}

const ALLOW_ONCE_OPTION: &str = "allow_once";
const ALLOW_ALWAYS_OPTION: &str = "allow_always";
const REJECT_ONCE_OPTION: &str = "reject_once";
const REJECT_ALWAYS_OPTION: &str = "reject_always";

fn sdk_error(error: AcpRuntimeError) -> agent_client_protocol::Error {
    agent_client_protocol::util::internal_error(error.to_string())
}

/// Error type for ACP integration state, conversion, and client callback
/// failures.
#[derive(Debug, Error)]
pub enum AcpRuntimeError {
    /// Required builder field was omitted.
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    /// ACP or agentkit session is not registered.
    #[error("session not found: {0}")]
    SessionNotFound(String),
    /// Attempted to bind an already registered session.
    #[error("session already bound: {0}")]
    SessionAlreadyBound(String),
    /// Prompt or event content is not supported by this integration layer.
    #[error("unsupported content: {0}")]
    UnsupportedContent(String),
    /// Client callback channel was closed.
    #[error("client channel closed")]
    ClientClosed,
    /// The client returned an unexpected or unsupported response.
    #[error("invalid client response: {0}")]
    InvalidClientResponse(String),
    /// Headless runtime support is not complete yet.
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    /// The active prompt was cancelled.
    #[error("cancelled")]
    Cancelled,
    /// Agent loop returned an error.
    #[error("loop error: {0}")]
    Loop(String),
    /// ACP SDK returned an error.
    #[error("ACP SDK error: {0}")]
    Sdk(String),
}

/// Message emitted by [`AcpClientHandle`] for a host task to forward over ACP
/// JSON-RPC.
#[derive(Debug)]
pub enum AcpClientMessage {
    /// Fire-and-forget `session/update` notification.
    SessionNotification(acp::SessionNotification),
    /// Barrier used to wait until previously queued client messages were drained.
    Flush {
        /// Completed once all earlier messages have been handled by the drain task.
        response: oneshot::Sender<()>,
    },
    /// `session/request_permission` request that expects a response.
    PermissionRequest {
        /// Request payload to send to the ACP client.
        request: acp::RequestPermissionRequest,
        /// Response channel the host must complete with the ACP client reply.
        response: oneshot::Sender<Result<acp::RequestPermissionResponse, AcpRuntimeError>>,
    },
}

/// Cloneable send capability used by observers and approval resolvers.
///
/// A hybrid host drains the receiver returned by [`AcpClientHandle::channel`]
/// and translates messages into upstream SDK sends for its active connection.
#[derive(Clone, Debug)]
pub struct AcpClientHandle {
    tx: mpsc::UnboundedSender<AcpClientMessage>,
}

impl AcpClientHandle {
    /// Creates a client handle and the corresponding host-owned receiver.
    #[must_use]
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<AcpClientMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    /// Queues a session update notification for the ACP client.
    pub fn notify_session(
        &self,
        notification: acp::SessionNotification,
    ) -> Result<(), AcpRuntimeError> {
        self.tx
            .send(AcpClientMessage::SessionNotification(notification))
            .map_err(|_| AcpRuntimeError::ClientClosed)
    }

    /// Waits until all previously queued client messages have been drained.
    pub async fn flush(&self) -> Result<(), AcpRuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(AcpClientMessage::Flush { response: tx })
            .map_err(|_| AcpRuntimeError::ClientClosed)?;
        rx.await.map_err(|_| AcpRuntimeError::ClientClosed)
    }

    /// Sends a permission request and waits for the host to supply the client
    /// response.
    pub async fn request_permission(
        &self,
        request: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse, AcpRuntimeError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(AcpClientMessage::PermissionRequest {
                request,
                response: tx,
            })
            .map_err(|_| AcpRuntimeError::ClientClosed)?;
        rx.await.map_err(|_| AcpRuntimeError::ClientClosed)?
    }
}

/// Binding between an ACP session, an agentkit loop session, and the current
/// ACP client connection.
#[derive(Clone)]
pub struct AcpSessionBinding {
    /// ACP session id visible to the client.
    pub acp_session_id: acp::SessionId,
    /// Agentkit loop session id used by `LoopDriver`.
    pub agentkit_session_id: AgentkitSessionId,
    /// ACP client send capability for this session.
    pub client: AcpClientHandle,
    /// Optional cancellation controller owned by the host.
    pub cancellation: Option<CancellationController>,
    /// Current working directory for the session.
    pub cwd: Option<PathBuf>,
    /// Additional workspace roots.
    pub additional_directories: Vec<PathBuf>,
    /// Host metadata associated with the session.
    pub metadata: MetadataMap,
}

impl AcpSessionBinding {
    /// Creates a minimal session binding.
    #[must_use]
    pub fn new(
        acp_session_id: acp::SessionId,
        agentkit_session_id: AgentkitSessionId,
        client: AcpClientHandle,
    ) -> Self {
        Self {
            acp_session_id,
            agentkit_session_id,
            client,
            cancellation: None,
            cwd: None,
            additional_directories: Vec::new(),
            metadata: MetadataMap::new(),
        }
    }

    /// Sets the cancellation controller used for ACP `session/cancel`.
    #[must_use]
    pub fn cancellation(mut self, cancellation: CancellationController) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    /// Sets workspace metadata from an ACP `session/new` request.
    #[must_use]
    pub fn workspace(mut self, cwd: PathBuf, additional_directories: Vec<PathBuf>) -> Self {
        self.cwd = Some(cwd);
        self.additional_directories = additional_directories;
        self
    }

    /// Replaces host metadata.
    #[must_use]
    pub fn metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Handle returned after binding a session.
#[derive(Clone)]
pub struct AcpSessionHandle {
    acp_session_id: acp::SessionId,
    agentkit_session_id: AgentkitSessionId,
    cancellation: CancellationController,
}

impl AcpSessionHandle {
    /// ACP session id visible to the client.
    #[must_use]
    pub fn acp_session_id(&self) -> &acp::SessionId {
        &self.acp_session_id
    }

    /// Agentkit session id used by the loop.
    #[must_use]
    pub fn agentkit_session_id(&self) -> &AgentkitSessionId {
        &self.agentkit_session_id
    }

    /// Cancellation handle to wire into `AgentBuilder::cancellation`.
    #[must_use]
    pub fn cancellation_handle(&self) -> CancellationHandle {
        self.cancellation.handle()
    }

    /// Interrupts the session's cancellation controller.
    pub fn interrupt(&self) {
        self.cancellation.interrupt();
    }
}

#[derive(Clone)]
struct AcpSessionState {
    acp_session_id: acp::SessionId,
    agentkit_session_id: AgentkitSessionId,
    client: AcpClientHandle,
    cancellation: CancellationController,
    cwd: Option<PathBuf>,
    additional_directories: Vec<PathBuf>,
    metadata: MetadataMap,
    part_kinds: Arc<Mutex<HashMap<PartId, PartKind>>>,
}

#[derive(Default)]
struct AcpIntegrationInner {
    by_acp: HashMap<acp::SessionId, AcpSessionState>,
    by_agentkit: HashMap<AgentkitSessionId, acp::SessionId>,
}

/// ACP integration object for hybrid hosts.
#[derive(Clone)]
pub struct AcpIntegration {
    name: String,
    version: String,
    approval_resolver: Arc<dyn AcpApprovalResolver>,
    approval_memory: Arc<dyn AcpApprovalMemory>,
    inner: Arc<RwLock<AcpIntegrationInner>>,
}

impl AcpIntegration {
    /// Starts building an [`AcpIntegration`].
    #[must_use]
    pub fn builder() -> AcpIntegrationBuilder {
        AcpIntegrationBuilder::default()
    }

    /// Agent implementation name advertised during initialization.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Agent implementation version advertised during initialization.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Registers a session and returns the loop-facing handle.
    pub fn bind_session(
        &self,
        binding: AcpSessionBinding,
    ) -> Result<AcpSessionHandle, AcpRuntimeError> {
        let cancellation = binding.cancellation.unwrap_or_default();
        let state = AcpSessionState {
            acp_session_id: binding.acp_session_id.clone(),
            agentkit_session_id: binding.agentkit_session_id.clone(),
            client: binding.client,
            cancellation: cancellation.clone(),
            cwd: binding.cwd,
            additional_directories: binding.additional_directories,
            metadata: binding.metadata,
            part_kinds: Arc::new(Mutex::new(HashMap::new())),
        };

        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if inner.by_acp.contains_key(&state.acp_session_id) {
            return Err(AcpRuntimeError::SessionAlreadyBound(
                state.acp_session_id.to_string(),
            ));
        }
        if inner.by_agentkit.contains_key(&state.agentkit_session_id) {
            return Err(AcpRuntimeError::SessionAlreadyBound(
                state.agentkit_session_id.to_string(),
            ));
        }
        inner.by_agentkit.insert(
            state.agentkit_session_id.clone(),
            state.acp_session_id.clone(),
        );
        inner
            .by_acp
            .insert(state.acp_session_id.clone(), state.clone());

        Ok(AcpSessionHandle {
            acp_session_id: state.acp_session_id,
            agentkit_session_id: state.agentkit_session_id,
            cancellation,
        })
    }

    /// Removes a registered ACP session.
    pub fn unbind_session(&self, session_id: &acp::SessionId) -> Result<(), AcpRuntimeError> {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let state = inner
            .by_acp
            .remove(session_id)
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))?;
        inner.by_agentkit.remove(&state.agentkit_session_id);
        Ok(())
    }

    /// Returns a cancellation handle for an ACP session.
    pub fn cancellation_handle(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<CancellationHandle, AcpRuntimeError> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let state = inner
            .by_acp
            .get(session_id)
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))?;
        Ok(state.cancellation.handle())
    }

    /// Interrupts the cancellation controller for an ACP session.
    pub fn interrupt_session(&self, session_id: &acp::SessionId) -> Result<(), AcpRuntimeError> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let state = inner
            .by_acp
            .get(session_id)
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))?;
        state.cancellation.interrupt();
        Ok(())
    }

    /// Waits until queued session notifications for this ACP session were sent.
    pub async fn flush_session_updates(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<(), AcpRuntimeError> {
        let client = {
            let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
            inner
                .by_acp
                .get(session_id)
                .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))?
                .client
                .clone()
        };
        client.flush().await
    }

    fn notify_tool_input_patch(
        &self,
        session_id: &acp::SessionId,
        call_id: &agentkit_core::ToolCallId,
        input: Value,
    ) -> Result<(), AcpRuntimeError> {
        let (acp_session_id, client) = {
            let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
            let state = inner
                .by_acp
                .get(session_id)
                .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))?;
            (state.acp_session_id.clone(), state.client.clone())
        };
        client.notify_session(acp::SessionNotification::new(
            acp_session_id,
            acp::SessionUpdate::ToolCallUpdate(tool_input_update(call_id, input)),
        ))
    }

    /// Returns a prompt conversion/input helper.
    #[must_use]
    pub fn input_port(&self) -> AcpInputPort {
        AcpInputPort {
            integration: self.clone(),
        }
    }

    /// Returns a cloneable session registry helper.
    #[must_use]
    pub fn session_registry(&self) -> AcpSessionRegistry {
        AcpSessionRegistry {
            integration: self.clone(),
        }
    }

    /// Resolves a pending agentkit approval request through memory and the
    /// configured approval resolver.
    pub async fn resolve_approval(
        &self,
        acp_session_id: &acp::SessionId,
        request: ApprovalRequest,
    ) -> Result<AcpApprovalDecision, AcpRuntimeError> {
        if let Some(decision) = self.approval_memory.lookup(acp_session_id, &request) {
            return Ok(decision);
        }

        let (agentkit_session_id, client, tool_call) = {
            let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
            let state = inner
                .by_acp
                .get(acp_session_id)
                .ok_or_else(|| AcpRuntimeError::SessionNotFound(acp_session_id.to_string()))?;
            (
                state.agentkit_session_id.clone(),
                state.client.clone(),
                Some(approval_tool_call_update(&request)),
            )
        };

        let ctx = AcpApprovalContext {
            acp_session_id: acp_session_id.clone(),
            agentkit_session_id,
            request: request.clone(),
            tool_call,
        };
        let decision = self.approval_resolver.resolve(ctx, client).await?;
        if decision.remember() {
            self.approval_memory
                .remember(acp_session_id, &request, &decision);
        }
        Ok(decision)
    }

    fn route_event(&self, session_id: &AgentkitSessionId, event: AgentEvent) {
        let notification = {
            let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
            let Some(acp_session_id) = inner.by_agentkit.get(session_id).cloned() else {
                tracing::debug!(%session_id, "dropping ACP event for unbound agentkit session");
                return;
            };
            let Some(state) = inner.by_acp.get(&acp_session_id) else {
                return;
            };
            let mut part_kinds = state.part_kinds.lock().unwrap_or_else(|e| e.into_inner());
            let Some(update) = event_to_update(&event, &mut part_kinds) else {
                return;
            };
            let notification = acp::SessionNotification::new(state.acp_session_id.clone(), update);
            (state.client.clone(), notification)
        };

        if let Err(error) = notification.0.notify_session(notification.1) {
            tracing::debug!(%error, "failed to queue ACP session notification");
        }
    }
}

impl LoopObserver for AcpIntegration {
    fn handle_event(&self, event: ObservedEvent) {
        self.route_event(&event.session_id, event.event);
    }
}

/// Builder for [`AcpIntegration`].
pub struct AcpIntegrationBuilder {
    name: Option<String>,
    version: Option<String>,
    approval_resolver: Option<Arc<dyn AcpApprovalResolver>>,
    approval_memory: Arc<dyn AcpApprovalMemory>,
}

impl Default for AcpIntegrationBuilder {
    fn default() -> Self {
        Self {
            name: None,
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            approval_resolver: None,
            approval_memory: Arc::new(NoopApprovalMemory),
        }
    }
}

impl AcpIntegrationBuilder {
    /// Sets the agent implementation name.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the agent implementation version.
    #[must_use]
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Sets the approval resolver.
    #[must_use]
    pub fn approval_resolver(mut self, resolver: impl AcpApprovalResolver) -> Self {
        self.approval_resolver = Some(Arc::new(resolver));
        self
    }

    /// Sets remembered approval storage.
    #[must_use]
    pub fn approval_memory(mut self, memory: impl AcpApprovalMemory) -> Self {
        self.approval_memory = Arc::new(memory);
        self
    }

    /// Builds the integration.
    pub fn build(self) -> Result<AcpIntegration, AcpRuntimeError> {
        Ok(AcpIntegration {
            name: self.name.ok_or(AcpRuntimeError::MissingField("name"))?,
            version: self.version.unwrap_or_default(),
            approval_resolver: self
                .approval_resolver
                .ok_or(AcpRuntimeError::MissingField("approval_resolver"))?,
            approval_memory: self.approval_memory,
            inner: Arc::new(RwLock::new(AcpIntegrationInner::default())),
        })
    }
}

/// Prompt conversion helper for hybrid hosts.
#[derive(Clone)]
pub struct AcpInputPort {
    integration: AcpIntegration,
}

impl AcpInputPort {
    /// Converts ACP prompt content into agentkit input items.
    pub fn prompt_to_items(
        &self,
        request: &acp::PromptRequest,
    ) -> Result<Vec<Item>, AcpRuntimeError> {
        let registry = self.integration.session_registry();
        registry.agentkit_session_id(&request.session_id)?;
        prompt_to_items(request)
    }
}

/// Cloneable helper for querying and mutating ACP session bindings.
#[derive(Clone)]
pub struct AcpSessionRegistry {
    integration: AcpIntegration,
}

impl AcpSessionRegistry {
    /// Returns the agentkit session id mapped to an ACP session.
    pub fn agentkit_session_id(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<AgentkitSessionId, AcpRuntimeError> {
        let inner = self
            .integration
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner());
        inner
            .by_acp
            .get(session_id)
            .map(|state| state.agentkit_session_id.clone())
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))
    }

    /// Returns the ACP session id mapped to an agentkit session.
    pub fn acp_session_id(
        &self,
        session_id: &AgentkitSessionId,
    ) -> Result<acp::SessionId, AcpRuntimeError> {
        let inner = self
            .integration
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner());
        inner
            .by_agentkit
            .get(session_id)
            .cloned()
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))
    }

    /// Returns metadata for an ACP session.
    pub fn metadata(&self, session_id: &acp::SessionId) -> Result<MetadataMap, AcpRuntimeError> {
        let inner = self
            .integration
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner());
        inner
            .by_acp
            .get(session_id)
            .map(|state| state.metadata.clone())
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))
    }

    /// Returns workspace roots for an ACP session.
    pub fn workspace(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<(Option<PathBuf>, Vec<PathBuf>), AcpRuntimeError> {
        let inner = self
            .integration
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner());
        inner
            .by_acp
            .get(session_id)
            .map(|state| (state.cwd.clone(), state.additional_directories.clone()))
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))
    }
}

/// Context passed to approval resolvers.
#[derive(Clone, Debug)]
pub struct AcpApprovalContext {
    /// ACP session id visible to the client.
    pub acp_session_id: acp::SessionId,
    /// Agentkit loop session id.
    pub agentkit_session_id: AgentkitSessionId,
    /// Agentkit approval request.
    pub request: ApprovalRequest,
    /// ACP tool-call update describing the blocked tool call when available.
    pub tool_call: Option<acp::ToolCallUpdate>,
}

/// Decision returned by an ACP approval resolver.
#[derive(Clone, Debug, PartialEq)]
pub enum AcpApprovalDecision {
    /// Approve this invocation only.
    AllowOnce,
    /// Approve this invocation and remember the decision.
    AllowAlways,
    /// Reject this invocation only.
    RejectOnce { reason: Option<String> },
    /// Reject this invocation and remember the decision.
    RejectAlways { reason: Option<String> },
    /// Replace tool input and approve.
    PatchAndAllow { input: Value },
}

impl AcpApprovalDecision {
    /// Whether this decision should be remembered.
    #[must_use]
    pub fn remember(&self) -> bool {
        matches!(self, Self::AllowAlways | Self::RejectAlways { .. })
    }

    /// Converts to the agentkit approval decision where possible.
    #[must_use]
    pub fn to_agentkit_decision(&self) -> Option<ApprovalDecision> {
        match self {
            Self::AllowOnce | Self::AllowAlways => Some(ApprovalDecision::Approve),
            Self::RejectOnce { reason } | Self::RejectAlways { reason } => {
                Some(ApprovalDecision::Deny {
                    reason: reason.clone(),
                })
            }
            Self::PatchAndAllow { .. } => None,
        }
    }
}

/// Resolver for loop-level approval interrupts.
#[async_trait]
pub trait AcpApprovalResolver: Send + Sync + 'static {
    /// Resolves one approval request.
    async fn resolve(
        &self,
        ctx: AcpApprovalContext,
        client: AcpClientHandle,
    ) -> Result<AcpApprovalDecision, AcpRuntimeError>;
}

/// Approval memory is deliberately separate from the resolver.
pub trait AcpApprovalMemory: Send + Sync + 'static {
    /// Looks up a remembered decision.
    fn lookup(
        &self,
        session_id: &acp::SessionId,
        request: &ApprovalRequest,
    ) -> Option<AcpApprovalDecision>;

    /// Stores a remembered decision.
    fn remember(
        &self,
        session_id: &acp::SessionId,
        request: &ApprovalRequest,
        decision: &AcpApprovalDecision,
    );
}

/// No-op approval memory.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopApprovalMemory;

impl AcpApprovalMemory for NoopApprovalMemory {
    fn lookup(
        &self,
        _session_id: &acp::SessionId,
        _request: &ApprovalRequest,
    ) -> Option<AcpApprovalDecision> {
        None
    }

    fn remember(
        &self,
        _session_id: &acp::SessionId,
        _request: &ApprovalRequest,
        _decision: &AcpApprovalDecision,
    ) {
    }
}

/// Process-local approval memory keyed by stable request fields.
#[derive(Clone, Debug, Default)]
pub struct InMemoryApprovalMemory {
    decisions: Arc<Mutex<HashMap<String, AcpApprovalDecision>>>,
}

impl InMemoryApprovalMemory {
    /// Creates an empty in-memory approval store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl AcpApprovalMemory for InMemoryApprovalMemory {
    fn lookup(
        &self,
        session_id: &acp::SessionId,
        request: &ApprovalRequest,
    ) -> Option<AcpApprovalDecision> {
        self.decisions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&approval_memory_key(session_id, request))
            .cloned()
    }

    fn remember(
        &self,
        session_id: &acp::SessionId,
        request: &ApprovalRequest,
        decision: &AcpApprovalDecision,
    ) {
        self.decisions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(approval_memory_key(session_id, request), decision.clone());
    }
}

fn approval_memory_key(session_id: &acp::SessionId, request: &ApprovalRequest) -> String {
    serde_json::to_string(&(
        session_id,
        &request.request_kind,
        &request.reason,
        &request.summary,
    ))
    .unwrap_or_else(|_| {
        format!(
            "{}:{}:{:?}:{}",
            session_id, request.request_kind, request.reason, request.summary
        )
    })
}

/// Resolver that approves every request.
#[derive(Clone, Copy, Debug, Default)]
pub struct AutoApproveResolver;

#[async_trait]
impl AcpApprovalResolver for AutoApproveResolver {
    async fn resolve(
        &self,
        _ctx: AcpApprovalContext,
        _client: AcpClientHandle,
    ) -> Result<AcpApprovalDecision, AcpRuntimeError> {
        Ok(AcpApprovalDecision::AllowOnce)
    }
}

/// Resolver that denies every request.
#[derive(Clone, Copy, Debug, Default)]
pub struct AutoDenyResolver;

#[async_trait]
impl AcpApprovalResolver for AutoDenyResolver {
    async fn resolve(
        &self,
        _ctx: AcpApprovalContext,
        _client: AcpClientHandle,
    ) -> Result<AcpApprovalDecision, AcpRuntimeError> {
        Ok(AcpApprovalDecision::RejectOnce {
            reason: Some("permission denied".into()),
        })
    }
}

/// Resolver that delegates approval decisions to the ACP client through
/// `session/request_permission`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClientPermissionResolver;

impl ClientPermissionResolver {
    /// Creates a client permission resolver.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AcpApprovalResolver for ClientPermissionResolver {
    async fn resolve(
        &self,
        ctx: AcpApprovalContext,
        client: AcpClientHandle,
    ) -> Result<AcpApprovalDecision, AcpRuntimeError> {
        let tool_call = ctx
            .tool_call
            .unwrap_or_else(|| approval_tool_call_update(&ctx.request));
        let response = client
            .request_permission(acp::RequestPermissionRequest::new(
                ctx.acp_session_id,
                tool_call,
                default_permission_options(),
            ))
            .await?;
        permission_response_to_decision(response)
    }
}

/// Agent factory context for the eventual headless ACP helper.
#[derive(Clone)]
pub struct AcpAgentFactoryContext {
    /// ACP session id visible to the client.
    pub acp_session_id: acp::SessionId,
    /// Agentkit loop session id.
    pub agentkit_session_id: AgentkitSessionId,
    /// Current working directory.
    pub cwd: PathBuf,
    /// Additional workspace roots.
    pub additional_directories: Vec<PathBuf>,
    /// Shared ACP integration object.
    pub integration: Arc<AcpIntegration>,
    /// Cancellation handle to wire into the agent builder.
    pub cancellation: CancellationHandle,
    /// Session metadata.
    pub metadata: MetadataMap,
}

/// Factory abstraction used by the headless runtime helper.
#[async_trait]
pub trait AcpAgentFactory<M>: Send + Sync + 'static
where
    M: ModelAdapter,
{
    /// Builds and starts an agent loop driver for a newly created ACP session.
    async fn start(
        &self,
        ctx: AcpAgentFactoryContext,
    ) -> Result<agentkit_loop::LoopDriver<M::Session>, AcpRuntimeError>;
}

/// Headless ACP runtime helper.
pub struct AcpHeadlessRuntime<M>
where
    M: ModelAdapter,
{
    _marker: std::marker::PhantomData<M>,
}

impl<M> AcpHeadlessRuntime<M>
where
    M: ModelAdapter + Send + Sync + 'static,
    M::Session: Send + 'static,
{
    /// Starts building a headless runtime.
    #[must_use]
    pub fn builder() -> AcpHeadlessRuntimeBuilder<M> {
        AcpHeadlessRuntimeBuilder::default()
    }
}

/// Builder for [`AcpHeadlessRuntime`].
pub struct AcpHeadlessRuntimeBuilder<M>
where
    M: ModelAdapter,
{
    factory: Option<Arc<dyn AcpAgentFactory<M>>>,
    integration: Option<Arc<AcpIntegration>>,
}

struct AcpHeadlessRuntimeState<M>
where
    M: ModelAdapter,
{
    factory: Arc<dyn AcpAgentFactory<M>>,
    integration: Arc<AcpIntegration>,
    sessions:
        AsyncMutex<HashMap<acp::SessionId, Arc<AsyncMutex<agentkit_loop::LoopDriver<M::Session>>>>>,
    next_session: AtomicU64,
}

impl<M> AcpHeadlessRuntimeState<M>
where
    M: ModelAdapter + Send + Sync + 'static,
    M::Session: Send + 'static,
{
    fn new(factory: Arc<dyn AcpAgentFactory<M>>, integration: Arc<AcpIntegration>) -> Self {
        Self {
            factory,
            integration,
            sessions: AsyncMutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
        }
    }

    fn next_acp_session_id(&self) -> acp::SessionId {
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        acp::SessionId::new(format!("session-{id}"))
    }

    async fn initialize(
        &self,
        request: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, AcpRuntimeError> {
        Ok(acp::InitializeResponse::new(request.protocol_version)
            .agent_capabilities(headless_capabilities())
            .agent_info(acp::Implementation::new(
                self.integration.name().to_string(),
                self.integration.version().to_string(),
            )))
    }

    async fn new_session(
        self: &Arc<Self>,
        request: acp::NewSessionRequest,
        cx: ConnectionTo<Client>,
    ) -> Result<acp::NewSessionResponse, AcpRuntimeError> {
        let acp_session_id = self.next_acp_session_id();
        let agentkit_session_id = AgentkitSessionId::new(acp_session_id.to_string());
        let cancellation = CancellationController::new();
        let (client, rx) = AcpClientHandle::channel();
        tokio::spawn(drain_client_messages(rx, cx));

        let mut metadata = MetadataMap::new();
        metadata.insert("acp.cwd".into(), json!(request.cwd));
        metadata.insert(
            "acp.additional_directories".into(),
            json!(request.additional_directories),
        );

        let binding =
            AcpSessionBinding::new(acp_session_id.clone(), agentkit_session_id.clone(), client)
                .cancellation(cancellation.clone())
                .workspace(request.cwd.clone(), request.additional_directories.clone())
                .metadata(metadata.clone());
        self.integration.bind_session(binding)?;

        let ctx = AcpAgentFactoryContext {
            acp_session_id: acp_session_id.clone(),
            agentkit_session_id,
            cwd: request.cwd,
            additional_directories: request.additional_directories,
            integration: Arc::clone(&self.integration),
            cancellation: cancellation.handle(),
            metadata,
        };
        let driver = match self.factory.start(ctx).await {
            Ok(driver) => driver,
            Err(error) => {
                let _ = self.integration.unbind_session(&acp_session_id);
                return Err(error);
            }
        };
        self.sessions
            .lock()
            .await
            .insert(acp_session_id.clone(), Arc::new(AsyncMutex::new(driver)));
        Ok(acp::NewSessionResponse::new(acp_session_id))
    }

    async fn prompt(
        &self,
        request: acp::PromptRequest,
    ) -> Result<acp::PromptResponse, AcpRuntimeError> {
        let items = self.integration.input_port().prompt_to_items(&request)?;
        let cancellation = self.integration.cancellation_handle(&request.session_id)?;
        let prompt_generation = cancellation.generation();
        let driver = self.driver(&request.session_id).await?;
        let mut driver = driver.lock().await;
        driver
            .submit_input(items)
            .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?;

        loop {
            match driver
                .next()
                .await
                .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?
            {
                LoopStep::Finished(result) => {
                    if result.finish_reason == FinishReason::ToolCall {
                        continue;
                    }
                    self.integration
                        .flush_session_updates(&request.session_id)
                        .await?;
                    return Ok(acp::PromptResponse::new(finish_reason_to_stop_reason(
                        &result.finish_reason,
                    )?));
                }
                LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                    self.integration
                        .flush_session_updates(&request.session_id)
                        .await?;
                    return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
                }
                LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
                LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                    if cancellation.is_cancelled_since(prompt_generation) {
                        driver
                            .cancel_pending_approvals()
                            .await
                            .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?;
                        self.integration
                            .flush_session_updates(&request.session_id)
                            .await?;
                        return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
                    }
                    tokio::select! {
                        decision = self.integration.resolve_approval(&request.session_id, pending.request.clone()) => {
                            let decision = match decision {
                                Ok(decision) => decision,
                                Err(AcpRuntimeError::Cancelled) => {
                                    driver
                                        .cancel_pending_approvals()
                                        .await
                                        .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?;
                                    self.integration
                                        .flush_session_updates(&request.session_id)
                                        .await?;
                                    return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
                                }
                                Err(error) => return Err(error),
                            };
                            if cancellation.is_cancelled_since(prompt_generation) {
                                driver
                                    .cancel_pending_approvals()
                                    .await
                                    .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?;
                                self.integration
                                    .flush_session_updates(&request.session_id)
                                    .await?;
                                return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
                            }
                            if let AcpApprovalDecision::PatchAndAllow { input } = &decision
                                && let Some(call_id) = &pending.request.call_id
                            {
                                self.integration.notify_tool_input_patch(
                                    &request.session_id,
                                    call_id,
                                    input.clone(),
                                )?;
                            }
                            apply_approval_decision(&mut driver, &pending.request, decision)?;
                        }
                        () = cancellation.cancelled_since(prompt_generation) => {
                            driver
                                .cancel_pending_approvals()
                                .await
                                .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?;
                            self.integration
                                .flush_session_updates(&request.session_id)
                                .await?;
                            return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
                        }
                    }
                }
            }
        }
    }

    async fn cancel(&self, notification: acp::CancelNotification) -> Result<(), AcpRuntimeError> {
        self.integration.interrupt_session(&notification.session_id)
    }

    async fn close(
        &self,
        request: acp::CloseSessionRequest,
    ) -> Result<acp::CloseSessionResponse, AcpRuntimeError> {
        self.sessions.lock().await.remove(&request.session_id);
        self.integration.unbind_session(&request.session_id)?;
        Ok(acp::CloseSessionResponse::new())
    }

    async fn driver(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<Arc<AsyncMutex<agentkit_loop::LoopDriver<M::Session>>>, AcpRuntimeError> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))
    }
}

impl<M> Default for AcpHeadlessRuntimeBuilder<M>
where
    M: ModelAdapter,
{
    fn default() -> Self {
        Self {
            factory: None,
            integration: None,
        }
    }
}

impl<M> AcpHeadlessRuntimeBuilder<M>
where
    M: ModelAdapter + Send + Sync + 'static,
    M::Session: Send + 'static,
{
    /// Sets the agent factory.
    #[must_use]
    pub fn agent_factory(mut self, factory: impl AcpAgentFactory<M>) -> Self {
        self.factory = Some(Arc::new(factory));
        self
    }

    /// Sets the shared integration object.
    #[must_use]
    pub fn integration(mut self, integration: AcpIntegration) -> Self {
        self.integration = Some(Arc::new(integration));
        self
    }

    /// Serves an ACP agent over stdio.
    #[cfg(feature = "stdio")]
    pub async fn serve_stdio(self) -> Result<(), AcpRuntimeError> {
        let factory = self
            .factory
            .ok_or(AcpRuntimeError::MissingField("agent_factory"))?;
        let integration = self
            .integration
            .ok_or(AcpRuntimeError::MissingField("integration"))?;
        Self::serve(agent_client_protocol::Stdio::new(), factory, integration).await
    }

    /// Serves an ACP agent over an arbitrary upstream ACP transport.
    pub async fn serve_transport(
        self,
        transport: impl agent_client_protocol::ConnectTo<agent_client_protocol::Agent> + 'static,
    ) -> Result<(), AcpRuntimeError> {
        let factory = self
            .factory
            .ok_or(AcpRuntimeError::MissingField("agent_factory"))?;
        let integration = self
            .integration
            .ok_or(AcpRuntimeError::MissingField("integration"))?;
        Self::serve(transport, factory, integration).await
    }

    async fn serve(
        transport: impl agent_client_protocol::ConnectTo<agent_client_protocol::Agent> + 'static,
        factory: Arc<dyn AcpAgentFactory<M>>,
        integration: Arc<AcpIntegration>,
    ) -> Result<(), AcpRuntimeError> {
        let state = Arc::new(AcpHeadlessRuntimeState::<M>::new(factory, integration));
        agent_client_protocol::Agent
            .builder()
            .name(state.integration.name())
            .on_receive_request(
                {
                    let state = Arc::clone(&state);
                    async move |request: acp::InitializeRequest, responder, _cx| {
                        responder
                            .respond_with_result(state.initialize(request).await.map_err(sdk_error))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let state = Arc::clone(&state);
                    async move |request: acp::NewSessionRequest, responder, cx| {
                        let state = Arc::clone(&state);
                        let connection = cx.clone();
                        cx.spawn(async move {
                            responder.respond_with_result(
                                state
                                    .new_session(request, connection)
                                    .await
                                    .map_err(sdk_error),
                            )
                        })?;
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let state = Arc::clone(&state);
                    async move |request: acp::PromptRequest, responder, cx| {
                        let state = Arc::clone(&state);
                        cx.spawn(async move {
                            responder
                                .respond_with_result(state.prompt(request).await.map_err(sdk_error))
                        })?;
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                {
                    let state = Arc::clone(&state);
                    async move |notification: acp::CancelNotification, _cx| {
                        state.cancel(notification).await.map_err(sdk_error)?;
                        Ok(Handled::Yes)
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                {
                    let state = Arc::clone(&state);
                    async move |request: acp::CloseSessionRequest, responder, _cx| {
                        responder.respond_with_result(state.close(request).await.map_err(sdk_error))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(transport)
            .await
            .map_err(|error| AcpRuntimeError::Sdk(error.to_string()))
    }
}

fn headless_capabilities() -> acp::AgentCapabilities {
    acp::AgentCapabilities::new()
        .prompt_capabilities(
            acp::PromptCapabilities::new()
                .image(true)
                .audio(true)
                .embedded_context(true),
        )
        .session_capabilities(
            acp::SessionCapabilities::new()
                .additional_directories(acp::SessionAdditionalDirectoriesCapabilities::new())
                .close(acp::SessionCloseCapabilities::new()),
        )
}

async fn drain_client_messages(
    mut rx: mpsc::UnboundedReceiver<AcpClientMessage>,
    cx: ConnectionTo<Client>,
) {
    while let Some(message) = rx.recv().await {
        match message {
            AcpClientMessage::SessionNotification(notification) => {
                if let Err(error) = cx.send_notification(notification) {
                    tracing::debug!(%error, "failed to send ACP session notification");
                }
            }
            AcpClientMessage::Flush { response } => {
                let _ = response.send(());
            }
            AcpClientMessage::PermissionRequest { request, response } => {
                let cx = cx.clone();
                tokio::spawn(async move {
                    let result = cx
                        .send_request(request)
                        .block_task()
                        .await
                        .map_err(|error| AcpRuntimeError::Sdk(error.to_string()));
                    let _ = response.send(result);
                });
            }
        }
    }
}

fn apply_approval_decision<S>(
    driver: &mut agentkit_loop::LoopDriver<S>,
    request: &ApprovalRequest,
    decision: AcpApprovalDecision,
) -> Result<(), AcpRuntimeError>
where
    S: agentkit_loop::ModelSession,
{
    match decision {
        AcpApprovalDecision::PatchAndAllow { input } => {
            let call_id = request.call_id.clone().ok_or_else(|| {
                AcpRuntimeError::Unsupported("patched approval requires call id".into())
            })?;
            driver
                .resolve_approval_for_with_patched_input(call_id, input)
                .map_err(|error| AcpRuntimeError::Loop(error.to_string()))
        }
        other => {
            let decision = other.to_agentkit_decision().ok_or_else(|| {
                AcpRuntimeError::Unsupported("unsupported approval decision".into())
            })?;
            match request.call_id.clone() {
                Some(call_id) => driver
                    .resolve_approval_for(call_id, decision)
                    .map_err(|error| AcpRuntimeError::Loop(error.to_string())),
                None => driver
                    .resolve_approval(decision)
                    .map_err(|error| AcpRuntimeError::Loop(error.to_string())),
            }
        }
    }
}

fn prompt_to_items(request: &acp::PromptRequest) -> Result<Vec<Item>, AcpRuntimeError> {
    let mut user_parts = Vec::new();
    let mut context_items = Vec::new();

    for block in &request.prompt {
        match block {
            acp::ContentBlock::Text(text) => {
                user_parts.push(Part::Text(TextPart::new(text.text.clone())));
            }
            acp::ContentBlock::Image(image) => {
                let data = data_url_ref(&image.mime_type, &image.data);
                user_parts.push(Part::media(Modality::Image, image.mime_type.clone(), data));
            }
            acp::ContentBlock::Audio(audio) => {
                user_parts.push(Part::media(
                    Modality::Audio,
                    audio.mime_type.clone(),
                    data_url_ref(&audio.mime_type, &audio.data),
                ));
            }
            acp::ContentBlock::ResourceLink(link) => {
                context_items.push(resource_link_item(link)?);
            }
            acp::ContentBlock::Resource(resource) => {
                context_items.push(resource_item(resource)?);
            }
            _ => {
                return Err(AcpRuntimeError::UnsupportedContent(
                    "unknown ACP content block".into(),
                ));
            }
        }
    }

    let mut items = Vec::new();
    items.extend(context_items);
    if !user_parts.is_empty() {
        items.push(Item::new(ItemKind::User, user_parts));
    } else if !items.is_empty() {
        items.push(Item::text(ItemKind::User, "Use the provided context."));
    }
    Ok(items)
}

fn resource_link_item(link: &acp::ResourceLink) -> Result<Item, AcpRuntimeError> {
    let mut metadata = MetadataMap::new();
    metadata.insert("acp.resource.uri".into(), json!(link.uri));
    metadata.insert("acp.resource.name".into(), json!(link.name));
    if let Some(description) = &link.description {
        metadata.insert("acp.resource.description".into(), json!(description));
    }
    if let Some(mime_type) = &link.mime_type {
        metadata.insert("acp.resource.mime_type".into(), json!(mime_type));
    }
    Ok(Item::new(
        ItemKind::Context,
        vec![Part::file(DataRef::uri(link.uri.clone()))],
    )
    .with_metadata(metadata))
}

fn resource_item(resource: &acp::EmbeddedResource) -> Result<Item, AcpRuntimeError> {
    match &resource.resource {
        acp::EmbeddedResourceResource::TextResourceContents(text) => {
            let mut metadata = MetadataMap::new();
            metadata.insert("acp.resource.uri".into(), json!(text.uri));
            if let Some(mime_type) = &text.mime_type {
                metadata.insert("acp.resource.mime_type".into(), json!(mime_type));
            }
            Ok(Item::text(ItemKind::Context, text.text.clone()).with_metadata(metadata))
        }
        acp::EmbeddedResourceResource::BlobResourceContents(blob) => {
            let mut metadata = MetadataMap::new();
            metadata.insert("acp.resource.uri".into(), json!(blob.uri));
            if let Some(mime_type) = &blob.mime_type {
                metadata.insert("acp.resource.mime_type".into(), json!(mime_type));
            }
            let mime_type = blob
                .mime_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".into());
            Ok(Item::new(
                ItemKind::Context,
                vec![Part::media(
                    Modality::Binary,
                    mime_type.clone(),
                    data_url_ref(&mime_type, &blob.blob),
                )],
            )
            .with_metadata(metadata))
        }
        _ => Err(AcpRuntimeError::UnsupportedContent(
            "unknown ACP embedded resource".into(),
        )),
    }
}

fn event_to_update(
    event: &AgentEvent,
    part_kinds: &mut HashMap<PartId, PartKind>,
) -> Option<acp::SessionUpdate> {
    match event {
        AgentEvent::ContentDelta(delta) => delta_to_update(delta, part_kinds),
        AgentEvent::ToolCallRequested(call) => Some(acp::SessionUpdate::ToolCall(tool_call(call))),
        AgentEvent::ToolExecutionStarted(call) => Some(acp::SessionUpdate::ToolCallUpdate(
            tool_status_update(&call.id, acp::ToolCallStatus::InProgress),
        )),
        AgentEvent::ToolExecutionProgress(result) => Some(acp::SessionUpdate::ToolCallUpdate(
            tool_progress_update(result),
        )),
        AgentEvent::ToolResultReceived(result) => Some(acp::SessionUpdate::ToolCallUpdate(
            tool_result_update(result),
        )),
        AgentEvent::UsageUpdated(usage) => usage_update(usage).map(acp::SessionUpdate::UsageUpdate),
        AgentEvent::Warning { message } => {
            tracing::warn!(%message, "agentkit warning while routing ACP event");
            None
        }
        AgentEvent::RunFailed { message } => {
            tracing::debug!(%message, "agentkit run failed while routing ACP event");
            None
        }
        AgentEvent::ApprovalRequired(_)
        | AgentEvent::ApprovalResolved { .. }
        | AgentEvent::RunStarted { .. }
        | AgentEvent::TurnStarted { .. }
        | AgentEvent::InputAccepted { .. }
        | AgentEvent::ToolCatalogChanged(_)
        | AgentEvent::MutationStarted { .. }
        | AgentEvent::MutationFinished { .. } => None,
        AgentEvent::TurnFinished(_) => {
            part_kinds.clear();
            None
        }
        _ => None,
    }
}

fn delta_to_update(
    delta: &Delta,
    part_kinds: &mut HashMap<PartId, PartKind>,
) -> Option<acp::SessionUpdate> {
    match delta {
        Delta::BeginPart { part_id, kind } => {
            part_kinds.insert(part_id.clone(), *kind);
            None
        }
        Delta::AppendText { part_id, chunk } => match part_kinds.get(part_id) {
            Some(PartKind::Reasoning) => Some(acp::SessionUpdate::AgentThoughtChunk(
                acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                    chunk.clone(),
                ))),
            )),
            Some(PartKind::Text) => Some(acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                    chunk.clone(),
                ))),
            )),
            None => Some(acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                    chunk.clone(),
                ))),
            )),
            Some(_) => None,
        },
        Delta::CommitPart { part } => {
            let _ = part;
            None
        }
        Delta::AppendBytes { .. } | Delta::ReplaceStructured { .. } | Delta::SetMetadata { .. } => {
            None
        }
    }
}

fn tool_call(call: &ToolCallPart) -> acp::ToolCall {
    acp::ToolCall::new(acp::ToolCallId::new(call.id.to_string()), call.name.clone())
        .status(acp::ToolCallStatus::Pending)
        .raw_input(call.input.clone())
}

fn tool_result_update(result: &ToolResultPart) -> acp::ToolCallUpdate {
    tool_output_update(
        result,
        if result.is_error {
            acp::ToolCallStatus::Failed
        } else {
            acp::ToolCallStatus::Completed
        },
    )
}

fn tool_progress_update(result: &ToolResultPart) -> acp::ToolCallUpdate {
    tool_output_update(result, acp::ToolCallStatus::InProgress)
}

fn tool_output_update(result: &ToolResultPart, status: acp::ToolCallStatus) -> acp::ToolCallUpdate {
    let fields = acp::ToolCallUpdateFields::new()
        .status(status)
        .raw_output(tool_output_raw(&result.output))
        .content(tool_output_content(&result.output));
    acp::ToolCallUpdate::new(acp::ToolCallId::new(result.call_id.to_string()), fields)
}

fn tool_status_update(
    call_id: &agentkit_core::ToolCallId,
    status: acp::ToolCallStatus,
) -> acp::ToolCallUpdate {
    acp::ToolCallUpdate::new(
        acp::ToolCallId::new(call_id.to_string()),
        acp::ToolCallUpdateFields::new().status(status),
    )
}

fn tool_input_update(call_id: &agentkit_core::ToolCallId, input: Value) -> acp::ToolCallUpdate {
    acp::ToolCallUpdate::new(
        acp::ToolCallId::new(call_id.to_string()),
        acp::ToolCallUpdateFields::new().raw_input(input),
    )
}

fn tool_output_raw(output: &ToolOutput) -> Option<Value> {
    match output {
        ToolOutput::Text(text) => Some(json!({ "text": text })),
        ToolOutput::Structured(value) => Some(value.clone()),
        ToolOutput::Parts(parts) => serde_json::to_value(parts).ok(),
        ToolOutput::Files(files) => serde_json::to_value(files).ok(),
    }
}

fn tool_output_content(output: &ToolOutput) -> Option<Vec<acp::ToolCallContent>> {
    let content = match output {
        ToolOutput::Text(text) => Some(vec![acp::ToolCallContent::Content(acp::Content::new(
            acp::ContentBlock::Text(acp::TextContent::new(text.clone())),
        ))]),
        ToolOutput::Structured(value) => {
            Some(vec![acp::ToolCallContent::Content(acp::Content::new(
                acp::ContentBlock::Text(acp::TextContent::new(value.to_string())),
            ))])
        }
        ToolOutput::Parts(parts) => Some(parts.iter().filter_map(part_to_tool_content).collect()),
        ToolOutput::Files(files) => Some(files.iter().map(file_to_tool_content).collect()),
    }?;
    if content.is_empty() {
        None
    } else {
        Some(content)
    }
}

fn part_to_tool_content(part: &Part) -> Option<acp::ToolCallContent> {
    match part {
        Part::Text(text) => Some(text_to_tool_content(text.text.clone())),
        Part::Structured(value) => Some(structured_to_tool_content(value)),
        Part::Media(media) => Some(media_to_tool_content(media)),
        Part::File(file) => Some(file_to_tool_content(file)),
        Part::Reasoning(reasoning) => reasoning
            .summary
            .as_ref()
            .map(|summary| text_to_tool_content(summary.clone())),
        Part::Custom(custom) => Some(text_to_tool_content(
            custom
                .value
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| custom.data.as_ref().map(data_ref_payload))
                .unwrap_or_else(|| custom.kind.clone()),
        )),
        Part::ToolCall(_) | Part::ToolResult(_) => None,
    }
}

fn text_to_tool_content(text: String) -> acp::ToolCallContent {
    acp::ToolCallContent::Content(acp::Content::new(acp::ContentBlock::Text(
        acp::TextContent::new(text),
    )))
}

fn structured_to_tool_content(part: &StructuredPart) -> acp::ToolCallContent {
    text_to_tool_content(part.value.to_string())
}

fn media_to_tool_content(media: &MediaPart) -> acp::ToolCallContent {
    match media.modality {
        Modality::Image
            if matches!(media.data, DataRef::InlineText(_) | DataRef::InlineBytes(_)) =>
        {
            let mut image = acp::ImageContent::new(
                data_ref_base64_payload(&media.data),
                media.mime_type.clone(),
            );
            if let Some(uri) = data_ref_uri(&media.data) {
                image = image.uri(uri);
            }
            acp::ToolCallContent::Content(acp::Content::new(acp::ContentBlock::Image(image)))
        }
        Modality::Audio
            if matches!(media.data, DataRef::InlineText(_) | DataRef::InlineBytes(_)) =>
        {
            acp::ToolCallContent::Content(acp::Content::new(acp::ContentBlock::Audio(
                acp::AudioContent::new(
                    data_ref_base64_payload(&media.data),
                    media.mime_type.clone(),
                ),
            )))
        }
        Modality::Image | Modality::Audio | Modality::Video | Modality::Binary => {
            data_ref_to_resource_content(None, Some(&media.mime_type), &media.data)
        }
    }
}

fn file_to_tool_content(file: &FilePart) -> acp::ToolCallContent {
    data_ref_to_resource_content(file.name.as_deref(), file.mime_type.as_deref(), &file.data)
}

fn data_ref_to_resource_content(
    name: Option<&str>,
    mime_type: Option<&str>,
    data: &DataRef,
) -> acp::ToolCallContent {
    match data {
        DataRef::Uri(uri) => {
            let mut link = acp::ResourceLink::new(name.unwrap_or(uri), uri.clone());
            if let Some(mime_type) = mime_type {
                link = link.mime_type(mime_type.to_string());
            }
            acp::ToolCallContent::Content(acp::Content::new(acp::ContentBlock::ResourceLink(link)))
        }
        DataRef::Handle(handle) => {
            let uri = format!("artifact://{handle}");
            let link_name = name.map(str::to_owned).unwrap_or_else(|| uri.clone());
            let mut link = acp::ResourceLink::new(link_name, uri);
            if let Some(mime_type) = mime_type {
                link = link.mime_type(mime_type.to_string());
            }
            acp::ToolCallContent::Content(acp::Content::new(acp::ContentBlock::ResourceLink(link)))
        }
        DataRef::InlineText(text) if mime_type.is_none_or(|mime| mime.starts_with("text/")) => {
            let mut resource = acp::TextResourceContents::new(
                text.clone(),
                inline_resource_uri(name.unwrap_or("tool-output")),
            );
            if let Some(mime_type) = mime_type {
                resource = resource.mime_type(mime_type.to_string());
            }
            acp::ToolCallContent::Content(acp::Content::new(acp::ContentBlock::Resource(
                acp::EmbeddedResource::new(acp::EmbeddedResourceResource::TextResourceContents(
                    resource,
                )),
            )))
        }
        _ => {
            let mut resource = acp::BlobResourceContents::new(
                data_ref_base64_payload(data),
                inline_resource_uri(name.unwrap_or("tool-output")),
            );
            if let Some(mime_type) = mime_type {
                resource = resource.mime_type(mime_type.to_string());
            }
            acp::ToolCallContent::Content(acp::Content::new(acp::ContentBlock::Resource(
                acp::EmbeddedResource::new(acp::EmbeddedResourceResource::BlobResourceContents(
                    resource,
                )),
            )))
        }
    }
}

fn data_url_ref(mime_type: &str, base64_data: &str) -> DataRef {
    DataRef::inline_text(format!("data:{mime_type};base64,{base64_data}"))
}

fn data_ref_payload(data: &DataRef) -> String {
    match data {
        DataRef::InlineText(text) => text.clone(),
        DataRef::InlineBytes(bytes) => base64::engine::general_purpose::STANDARD.encode(bytes),
        DataRef::Uri(uri) => uri.clone(),
        DataRef::Handle(handle) => handle.to_string(),
    }
}

fn data_ref_base64_payload(data: &DataRef) -> String {
    match data {
        DataRef::InlineText(text) => data_url_base64_payload(text)
            .unwrap_or_else(|| base64::engine::general_purpose::STANDARD.encode(text.as_bytes())),
        DataRef::InlineBytes(bytes) => base64::engine::general_purpose::STANDARD.encode(bytes),
        DataRef::Uri(_) | DataRef::Handle(_) => String::new(),
    }
}

fn data_url_base64_payload(text: &str) -> Option<String> {
    let data_url = text.strip_prefix("data:")?;
    let (metadata, payload) = data_url.split_once(',')?;
    if metadata
        .split(';')
        .any(|segment| segment.eq_ignore_ascii_case("base64"))
    {
        Some(payload.to_string())
    } else {
        percent_decode(payload)
            .map(|decoded| base64::engine::general_purpose::STANDARD.encode(decoded.as_bytes()))
    }
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = *bytes.get(i + 1)?;
                let lo = *bytes.get(i + 2)?;
                decoded.push((hex_value(hi)? << 4) | hex_value(lo)?);
                i += 3;
            }
            byte => {
                decoded.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn data_ref_uri(data: &DataRef) -> Option<String> {
    match data {
        DataRef::Uri(uri) => Some(uri.clone()),
        DataRef::Handle(handle) => Some(format!("artifact://{handle}")),
        DataRef::InlineText(_) | DataRef::InlineBytes(_) => None,
    }
}

fn inline_resource_uri(name: &str) -> String {
    format!("agentkit://tool-output/{name}")
}

fn usage_update(usage: &CoreUsage) -> Option<acp::UsageUpdate> {
    let tokens = usage.tokens.as_ref()?;
    let used = tokens.input_tokens + tokens.output_tokens;
    let reported_size = [
        "context_window",
        "context_window_tokens",
        "model.context_window",
        "model.context_length",
        "openrouter.context_length",
    ]
    .iter()
    // ACP couples token usage and context-window size in one update. When the
    // size is unknown, skip the gauge instead of sending a misleading 0/used
    // denominator; providers should populate one of these metadata keys.
    .find_map(|key| usage.metadata.get(*key).and_then(Value::as_u64))?;
    let update = acp::UsageUpdate::new(used, reported_size);
    Some(match &usage.cost {
        Some(cost) => update.cost(acp::Cost::new(cost.amount, cost.currency.clone())),
        None => update,
    })
}

fn approval_tool_call_update(request: &ApprovalRequest) -> acp::ToolCallUpdate {
    let tool_call_id = request
        .call_id
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| request.id.to_string());
    acp::ToolCallUpdate::new(
        acp::ToolCallId::new(tool_call_id),
        acp::ToolCallUpdateFields::new()
            .title(request.summary.clone())
            .status(acp::ToolCallStatus::Pending)
            .raw_input(json!({
                "approval_id": request.id.to_string(),
                "request_kind": request.request_kind,
                "reason": request.reason,
                "metadata": request.metadata,
            })),
    )
}

fn default_permission_options() -> Vec<acp::PermissionOption> {
    vec![
        acp::PermissionOption::new(
            ALLOW_ONCE_OPTION,
            "Allow once",
            acp::PermissionOptionKind::AllowOnce,
        ),
        acp::PermissionOption::new(
            ALLOW_ALWAYS_OPTION,
            "Allow always",
            acp::PermissionOptionKind::AllowAlways,
        ),
        acp::PermissionOption::new(
            REJECT_ONCE_OPTION,
            "Reject once",
            acp::PermissionOptionKind::RejectOnce,
        ),
        acp::PermissionOption::new(
            REJECT_ALWAYS_OPTION,
            "Reject always",
            acp::PermissionOptionKind::RejectAlways,
        ),
    ]
}

fn permission_response_to_decision(
    response: acp::RequestPermissionResponse,
) -> Result<AcpApprovalDecision, AcpRuntimeError> {
    match response.outcome {
        acp::RequestPermissionOutcome::Cancelled => Err(AcpRuntimeError::Cancelled),
        acp::RequestPermissionOutcome::Selected(selected) => {
            match selected.option_id.to_string().as_str() {
                ALLOW_ONCE_OPTION => Ok(AcpApprovalDecision::AllowOnce),
                ALLOW_ALWAYS_OPTION => Ok(AcpApprovalDecision::AllowAlways),
                REJECT_ONCE_OPTION => Ok(AcpApprovalDecision::RejectOnce { reason: None }),
                REJECT_ALWAYS_OPTION => Ok(AcpApprovalDecision::RejectAlways { reason: None }),
                other => Err(AcpRuntimeError::InvalidClientResponse(format!(
                    "unknown permission option id {other}"
                ))),
            }
        }
        _ => Err(AcpRuntimeError::InvalidClientResponse(
            "unknown permission outcome".into(),
        )),
    }
}

/// Converts agentkit finish reasons into stable ACP stop reasons.
#[must_use]
pub fn finish_reason_to_stop_reason(
    reason: &FinishReason,
) -> Result<acp::StopReason, AcpRuntimeError> {
    match reason {
        FinishReason::Completed => Ok(acp::StopReason::EndTurn),
        FinishReason::MaxTokens => Ok(acp::StopReason::MaxTokens),
        FinishReason::Cancelled => Ok(acp::StopReason::Cancelled),
        FinishReason::Blocked => Ok(acp::StopReason::Refusal),
        FinishReason::Error => Err(AcpRuntimeError::Loop(
            "agent turn finished with an error".into(),
        )),
        FinishReason::ToolCall => Ok(acp::StopReason::EndTurn),
        FinishReason::Other(_) => Ok(acp::StopReason::EndTurn),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::Any;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agent_client_protocol::Channel;
    use agent_client_protocol::schema::ProtocolVersion;
    use agentkit_core::{CostUsage, Delta, PartId, TokenUsage, ToolCallPart};
    use agentkit_integration_tests::mock_model::{MockAdapter, TurnScript};
    use agentkit_loop::{Agent, ModelTurnEvent, ModelTurnResult, SessionConfig};
    use agentkit_tools_core::{
        ApprovalReason, PermissionChecker, PermissionDecision, PermissionRequest, Tool,
        ToolContext, ToolError, ToolName, ToolRegistry, ToolRequest, ToolResult, ToolSpec,
    };
    use tokio::sync::Notify;

    #[derive(Clone)]
    struct TestFactory {
        adapter: MockAdapter,
        tools: Option<ToolRegistry>,
        require_approval: bool,
    }

    #[async_trait]
    impl AcpAgentFactory<MockAdapter> for TestFactory {
        async fn start(
            &self,
            ctx: AcpAgentFactoryContext,
        ) -> Result<
            agentkit_loop::LoopDriver<<MockAdapter as ModelAdapter>::Session>,
            AcpRuntimeError,
        > {
            let mut builder = Agent::builder()
                .model(self.adapter.clone())
                .observer(ctx.integration.as_ref().clone())
                .cancellation(ctx.cancellation);
            if let Some(tools) = self.tools.clone() {
                builder = builder.add_tool_source(tools);
            }
            if self.require_approval {
                builder = builder.permissions(RequireApproval);
            }
            let agent = builder
                .build()
                .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?;
            agent
                .start(SessionConfig::new(ctx.agentkit_session_id).with_metadata(ctx.metadata))
                .await
                .map_err(|error| AcpRuntimeError::Loop(error.to_string()))
        }
    }

    #[derive(Clone)]
    struct ApprovalTool {
        spec: ToolSpec,
        calls: Arc<AtomicUsize>,
    }

    impl ApprovalTool {
        fn new() -> Self {
            Self {
                spec: ToolSpec::new(
                    ToolName::new("approval_tool"),
                    "tool used to exercise ACP approval",
                    json!({ "type": "object", "additionalProperties": true }),
                ),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Tool for ApprovalTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn proposed_requests(
            &self,
            _request: &ToolRequest,
        ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
            Ok(vec![Box::new(TestPermission {
                metadata: MetadataMap::new(),
            })])
        }

        async fn invoke(
            &self,
            request: ToolRequest,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::new(ToolResultPart::success(
                request.call_id,
                ToolOutput::text("approved"),
            )))
        }
    }

    struct TestPermission {
        metadata: MetadataMap,
    }

    impl PermissionRequest for TestPermission {
        fn kind(&self) -> &'static str {
            "test.approval"
        }

        fn summary(&self) -> String {
            "approve test tool".into()
        }

        fn metadata(&self) -> &MetadataMap {
            &self.metadata
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct RequireApproval;

    impl PermissionChecker for RequireApproval {
        fn evaluate(&self, request: &dyn PermissionRequest) -> PermissionDecision {
            PermissionDecision::RequireApproval(ApprovalRequest::new(
                "approval-1",
                request.kind(),
                ApprovalReason::PolicyRequiresConfirmation,
                request.summary(),
            ))
        }
    }

    #[derive(Clone, Default)]
    struct InterruptThenAllowResolver {
        integration: Arc<Mutex<Option<AcpIntegration>>>,
    }

    impl InterruptThenAllowResolver {
        fn set_integration(&self, integration: AcpIntegration) {
            *self.integration.lock().unwrap() = Some(integration);
        }
    }

    #[async_trait]
    impl AcpApprovalResolver for InterruptThenAllowResolver {
        async fn resolve(
            &self,
            ctx: AcpApprovalContext,
            _client: AcpClientHandle,
        ) -> Result<AcpApprovalDecision, AcpRuntimeError> {
            let integration = self
                .integration
                .lock()
                .unwrap()
                .clone()
                .expect("integration must be installed");
            integration.interrupt_session(&ctx.acp_session_id)?;
            Ok(AcpApprovalDecision::AllowOnce)
        }
    }

    #[derive(Clone)]
    struct FailingFactory;

    #[async_trait]
    impl AcpAgentFactory<MockAdapter> for FailingFactory {
        async fn start(
            &self,
            _ctx: AcpAgentFactoryContext,
        ) -> Result<
            agentkit_loop::LoopDriver<<MockAdapter as ModelAdapter>::Session>,
            AcpRuntimeError,
        > {
            Err(AcpRuntimeError::Loop("factory failed".into()))
        }
    }

    fn streamed_text(text: &str) -> TurnScript {
        let item = Item::text(ItemKind::Assistant, text);
        TurnScript::new([
            ModelTurnEvent::Delta(Delta::BeginPart {
                part_id: PartId::new("part-1"),
                kind: PartKind::Text,
            }),
            ModelTurnEvent::Delta(Delta::AppendText {
                part_id: PartId::new("part-1"),
                chunk: text.to_string(),
            }),
            ModelTurnEvent::Finished(ModelTurnResult {
                model: None,
                response_id: None,
                finish_reason: FinishReason::Completed,
                output_items: vec![item],
                usage: None,
                metadata: MetadataMap::new(),
            }),
        ])
    }

    fn failed_turn() -> TurnScript {
        TurnScript::new([ModelTurnEvent::Finished(ModelTurnResult {
            model: None,
            response_id: None,
            finish_reason: FinishReason::Error,
            output_items: Vec::new(),
            usage: None,
            metadata: MetadataMap::new(),
        })])
    }

    #[tokio::test]
    async fn headless_runtime_serves_initialize_session_prompt_and_close() {
        let adapter = MockAdapter::new();
        adapter.enqueue(streamed_text("hello from agentkit"));
        let integration = AcpIntegration::builder()
            .name("agentkit-test")
            .approval_resolver(AutoApproveResolver)
            .build()
            .unwrap();
        let notifications = Arc::new(Mutex::new(Vec::new()));

        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn({
            let integration = integration.clone();
            let factory = TestFactory {
                adapter: adapter.clone(),
                tools: None,
                require_approval: false,
            };
            async move {
                AcpHeadlessRuntime::<MockAdapter>::builder()
                    .integration(integration)
                    .agent_factory(factory)
                    .serve_transport(agent_transport)
                    .await
            }
        });

        let client = agent_client_protocol::Client
            .builder()
            .on_receive_notification(
                {
                    let notifications = Arc::clone(&notifications);
                    async move |notification: acp::SessionNotification, _cx| {
                        notifications.lock().unwrap().push(notification.update);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, async move |cx| {
                let init = cx
                    .send_request(acp::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert_eq!(init.agent_info.unwrap().name, "agentkit-test");
                assert!(init.agent_capabilities.prompt_capabilities.embedded_context);

                let cwd = std::env::current_dir()
                    .map_err(agent_client_protocol::Error::into_internal_error)?;
                let new_session = cx
                    .send_request(acp::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let response = cx
                    .send_request(acp::PromptRequest::new(
                        new_session.session_id.clone(),
                        vec![acp::ContentBlock::Text(acp::TextContent::new("hello"))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, acp::StopReason::EndTurn);

                cx.send_notification(acp::CancelNotification::new(new_session.session_id.clone()))?;
                cx.send_request(acp::CloseSessionRequest::new(new_session.session_id))
                    .block_task()
                    .await?;
                Ok(())
            });
        tokio::time::timeout(std::time::Duration::from_secs(5), client)
            .await
            .expect("client timed out")
            .expect("client run");

        server.abort();
        let _ = server.await;

        let updates = notifications.lock().unwrap();
        assert!(updates.iter().any(|update| {
            matches!(update, acp::SessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, acp::ContentBlock::Text(text) if text.text == "hello from agentkit"))
        }));
    }

    #[test]
    fn integration_builder_requires_explicit_approval_resolver() {
        let error = AcpIntegration::builder()
            .name("agentkit-test")
            .build()
            .err()
            .expect("approval resolver should be required");
        assert!(matches!(
            error,
            AcpRuntimeError::MissingField("approval_resolver")
        ));
    }

    #[test]
    fn prompt_to_items_places_context_before_user_tail() {
        let request = acp::PromptRequest::new(
            acp::SessionId::new("session-1"),
            vec![
                acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
                    "notes",
                    "file:///tmp/notes.md",
                )),
                acp::ContentBlock::Text(acp::TextContent::new("summarize this")),
            ],
        );

        let items = prompt_to_items(&request).unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, ItemKind::Context);
        assert_eq!(items[1].kind, ItemKind::User);
    }

    #[test]
    fn prompt_to_items_adds_user_tail_for_context_only_prompts() {
        let request = acp::PromptRequest::new(
            acp::SessionId::new("session-1"),
            vec![acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                acp::EmbeddedResourceResource::TextResourceContents(
                    acp::TextResourceContents::new("context", "file:///tmp/context.txt"),
                ),
            ))],
        );

        let items = prompt_to_items(&request).unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, ItemKind::Context);
        assert_eq!(items[1].kind, ItemKind::User);
    }

    #[test]
    fn prompt_to_items_wraps_inline_media_as_data_urls() {
        let request = acp::PromptRequest::new(
            acp::SessionId::new("session-1"),
            vec![
                acp::ContentBlock::Image(acp::ImageContent::new("aGVsbG8=", "image/png")),
                acp::ContentBlock::Audio(acp::AudioContent::new("aGVsbG8=", "audio/wav")),
            ],
        );

        let items = prompt_to_items(&request).unwrap();

        let media_refs: Vec<_> = items[0]
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Media(media) => Some(&media.data),
                _ => None,
            })
            .collect();
        assert_eq!(
            media_refs,
            vec![
                &DataRef::inline_text("data:image/png;base64,aGVsbG8="),
                &DataRef::inline_text("data:audio/wav;base64,aGVsbG8="),
            ]
        );
    }

    #[test]
    fn prompt_to_items_wraps_embedded_blob_as_data_url() {
        let request = acp::PromptRequest::new(
            acp::SessionId::new("session-1"),
            vec![acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                acp::EmbeddedResourceResource::BlobResourceContents(
                    acp::BlobResourceContents::new("AQID", "file:///tmp/data.bin")
                        .mime_type("application/octet-stream"),
                ),
            ))],
        );

        let items = prompt_to_items(&request).unwrap();

        let Part::Media(media) = &items[0].parts[0] else {
            panic!("expected media part");
        };
        assert_eq!(
            media.data,
            DataRef::inline_text("data:application/octet-stream;base64,AQID")
        );
    }

    #[test]
    fn prompt_to_items_keeps_image_payload_when_uri_is_present() {
        let request = acp::PromptRequest::new(
            acp::SessionId::new("session-1"),
            vec![acp::ContentBlock::Image(
                acp::ImageContent::new("aGVsbG8=", "image/png").uri("file:///tmp/image.png"),
            )],
        );

        let items = prompt_to_items(&request).unwrap();

        let Part::Media(media) = &items[0].parts[0] else {
            panic!("expected media part");
        };
        assert_eq!(
            media.data,
            DataRef::inline_text("data:image/png;base64,aGVsbG8=")
        );
    }

    #[test]
    fn tool_call_update_starts_pending() {
        let update = tool_call(&ToolCallPart::new("call-1", "example_tool", json!({})));

        assert_eq!(update.status, acp::ToolCallStatus::Pending);
    }

    #[test]
    fn tool_input_update_carries_patched_raw_input() {
        let update = tool_input_update(
            &agentkit_core::ToolCallId::new("call-1"),
            json!({ "value": "patched" }),
        );

        assert_eq!(update.fields.raw_input, Some(json!({ "value": "patched" })));
    }

    #[tokio::test]
    async fn failed_session_start_unbinds_acp_session() {
        let integration = AcpIntegration::builder()
            .name("agentkit-test")
            .approval_resolver(AutoApproveResolver)
            .build()
            .unwrap();

        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn({
            let integration = integration.clone();
            async move {
                AcpHeadlessRuntime::<MockAdapter>::builder()
                    .integration(integration)
                    .agent_factory(FailingFactory)
                    .serve_transport(agent_transport)
                    .await
            }
        });

        let client = agent_client_protocol::Client.builder().connect_with(
            client_transport,
            async move |cx| {
                let cwd = std::env::current_dir()
                    .map_err(agent_client_protocol::Error::into_internal_error)?;
                let error = cx
                    .send_request(acp::NewSessionRequest::new(cwd))
                    .block_task()
                    .await
                    .err()
                    .expect("session creation should fail");
                assert!(error.to_string().contains("factory failed"));
                Ok(())
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), client)
            .await
            .expect("client timed out")
            .expect("client run");

        server.abort();
        let _ = server.await;

        assert!(matches!(
            integration.cancellation_handle(&acp::SessionId::new("session-1")),
            Err(AcpRuntimeError::SessionNotFound(_))
        ));
    }

    #[tokio::test]
    async fn headless_runtime_round_trips_approval_to_client() {
        let adapter = MockAdapter::new();
        adapter.enqueue_many([
            TurnScript::tool_call(ToolCallPart::new(
                "call-1",
                "approval_tool",
                json!({ "value": 1 }),
            )),
            streamed_text("tool approved"),
        ]);
        let tool = ApprovalTool::new();
        let integration = AcpIntegration::builder()
            .name("agentkit-test")
            .approval_resolver(ClientPermissionResolver::new())
            .build()
            .unwrap();
        let permission_requests = Arc::new(AtomicUsize::new(0));

        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn({
            let integration = integration.clone();
            let factory = TestFactory {
                adapter: adapter.clone(),
                tools: Some(ToolRegistry::new().with(tool.clone())),
                require_approval: true,
            };
            async move {
                AcpHeadlessRuntime::<MockAdapter>::builder()
                    .integration(integration)
                    .agent_factory(factory)
                    .serve_transport(agent_transport)
                    .await
            }
        });

        let client = agent_client_protocol::Client
            .builder()
            .on_receive_notification(
                async move |notification: acp::SessionNotification, _cx| {
                    let _ = notification;
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                {
                    let permission_requests = Arc::clone(&permission_requests);
                    async move |request: acp::RequestPermissionRequest, responder, _cx| {
                        permission_requests.fetch_add(1, Ordering::SeqCst);
                        let option = request
                            .options
                            .iter()
                            .find(|option| option.option_id.to_string() == ALLOW_ONCE_OPTION)
                            .expect("allow option")
                            .option_id
                            .clone();
                        responder.respond(acp::RequestPermissionResponse::new(
                            acp::RequestPermissionOutcome::Selected(
                                acp::SelectedPermissionOutcome::new(option),
                            ),
                        ))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(client_transport, async move |cx| {
                cx.send_request(acp::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let cwd = std::env::current_dir()
                    .map_err(agent_client_protocol::Error::into_internal_error)?;
                let new_session = cx
                    .send_request(acp::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let response = cx
                    .send_request(acp::PromptRequest::new(
                        new_session.session_id.clone(),
                        vec![acp::ContentBlock::Text(acp::TextContent::new("use tool"))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
                cx.send_request(acp::CloseSessionRequest::new(new_session.session_id))
                    .block_task()
                    .await?;
                Ok(())
            });
        tokio::time::timeout(std::time::Duration::from_secs(5), client)
            .await
            .expect("client timed out")
            .expect("client run");

        server.abort();
        let _ = server.await;
        assert_eq!(permission_requests.load(Ordering::SeqCst), 1);
        assert_eq!(tool.call_count(), 1);
    }

    #[tokio::test]
    async fn headless_runtime_round_trips_rejection_to_client() {
        let adapter = MockAdapter::new();
        adapter.enqueue_many([
            TurnScript::tool_call(ToolCallPart::new(
                "call-1",
                "approval_tool",
                json!({ "value": 1 }),
            )),
            streamed_text("tool rejected"),
        ]);
        let tool = ApprovalTool::new();
        let integration = AcpIntegration::builder()
            .name("agentkit-test")
            .approval_resolver(ClientPermissionResolver::new())
            .build()
            .unwrap();
        let permission_requests = Arc::new(AtomicUsize::new(0));

        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn({
            let integration = integration.clone();
            let factory = TestFactory {
                adapter: adapter.clone(),
                tools: Some(ToolRegistry::new().with(tool.clone())),
                require_approval: true,
            };
            async move {
                AcpHeadlessRuntime::<MockAdapter>::builder()
                    .integration(integration)
                    .agent_factory(factory)
                    .serve_transport(agent_transport)
                    .await
            }
        });

        let client = agent_client_protocol::Client
            .builder()
            .on_receive_notification(
                async move |notification: acp::SessionNotification, _cx| {
                    let _ = notification;
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                {
                    let permission_requests = Arc::clone(&permission_requests);
                    async move |request: acp::RequestPermissionRequest, responder, _cx| {
                        permission_requests.fetch_add(1, Ordering::SeqCst);
                        let option = request
                            .options
                            .iter()
                            .find(|option| option.option_id.to_string() == REJECT_ONCE_OPTION)
                            .expect("reject option")
                            .option_id
                            .clone();
                        responder.respond(acp::RequestPermissionResponse::new(
                            acp::RequestPermissionOutcome::Selected(
                                acp::SelectedPermissionOutcome::new(option),
                            ),
                        ))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(client_transport, async move |cx| {
                cx.send_request(acp::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let cwd = std::env::current_dir()
                    .map_err(agent_client_protocol::Error::into_internal_error)?;
                let new_session = cx
                    .send_request(acp::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let response = cx
                    .send_request(acp::PromptRequest::new(
                        new_session.session_id.clone(),
                        vec![acp::ContentBlock::Text(acp::TextContent::new("use tool"))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
                cx.send_request(acp::CloseSessionRequest::new(new_session.session_id))
                    .block_task()
                    .await?;
                Ok(())
            });
        tokio::time::timeout(std::time::Duration::from_secs(5), client)
            .await
            .expect("client timed out")
            .expect("client run");

        server.abort();
        let _ = server.await;
        assert_eq!(permission_requests.load(Ordering::SeqCst), 1);
        assert_eq!(tool.call_count(), 0);
    }

    #[tokio::test]
    async fn headless_runtime_cancels_pending_permission_request() {
        let adapter = MockAdapter::new();
        adapter.enqueue_many([
            TurnScript::tool_call(ToolCallPart::new(
                "call-1",
                "approval_tool",
                json!({ "value": 1 }),
            )),
            streamed_text("tool should not run"),
        ]);
        let tool = ApprovalTool::new();
        let integration = AcpIntegration::builder()
            .name("agentkit-test")
            .approval_resolver(ClientPermissionResolver::new())
            .build()
            .unwrap();
        let permission_requests = Arc::new(AtomicUsize::new(0));
        let release_permission = Arc::new(Notify::new());

        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn({
            let integration = integration.clone();
            let factory = TestFactory {
                adapter: adapter.clone(),
                tools: Some(ToolRegistry::new().with(tool.clone())),
                require_approval: true,
            };
            async move {
                AcpHeadlessRuntime::<MockAdapter>::builder()
                    .integration(integration)
                    .agent_factory(factory)
                    .serve_transport(agent_transport)
                    .await
            }
        });

        let client = agent_client_protocol::Client
            .builder()
            .on_receive_notification(
                async move |notification: acp::SessionNotification, _cx| {
                    let _ = notification;
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                {
                    let permission_requests = Arc::clone(&permission_requests);
                    let release_permission = Arc::clone(&release_permission);
                    async move |request: acp::RequestPermissionRequest, responder, cx| {
                        let _ = request;
                        permission_requests.fetch_add(1, Ordering::SeqCst);
                        let release_permission = Arc::clone(&release_permission);
                        cx.spawn(async move {
                            release_permission.notified().await;
                            responder.respond(acp::RequestPermissionResponse::new(
                                acp::RequestPermissionOutcome::Cancelled,
                            ))
                        })?;
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(client_transport, async move |cx| {
                cx.send_request(acp::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let cwd = std::env::current_dir()
                    .map_err(agent_client_protocol::Error::into_internal_error)?;
                let new_session = cx
                    .send_request(acp::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;

                let prompt_task = tokio::spawn({
                    let cx = cx.clone();
                    let session_id = new_session.session_id.clone();
                    async move {
                        cx.send_request(acp::PromptRequest::new(
                            session_id,
                            vec![acp::ContentBlock::Text(acp::TextContent::new("use tool"))],
                        ))
                        .block_task()
                        .await
                    }
                });

                for _ in 0..100 {
                    if permission_requests.load(Ordering::SeqCst) > 0 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                assert_eq!(permission_requests.load(Ordering::SeqCst), 1);
                cx.send_notification(acp::CancelNotification::new(new_session.session_id.clone()))?;
                let response = prompt_task
                    .await
                    .map_err(agent_client_protocol::Error::into_internal_error)??;
                assert_eq!(response.stop_reason, acp::StopReason::Cancelled);
                release_permission.notify_waiters();

                cx.send_request(acp::CloseSessionRequest::new(new_session.session_id))
                    .block_task()
                    .await?;
                Ok(())
            });
        tokio::time::timeout(std::time::Duration::from_secs(5), client)
            .await
            .expect("client timed out")
            .expect("client run");

        server.abort();
        let _ = server.await;
        assert_eq!(tool.call_count(), 0);
    }

    #[tokio::test]
    async fn headless_runtime_permission_cancel_returns_cancelled() {
        let adapter = MockAdapter::new();
        adapter.enqueue_many([
            TurnScript::tool_call(ToolCallPart::new(
                "call-1",
                "approval_tool",
                json!({ "value": 1 }),
            )),
            streamed_text("tool should not run"),
        ]);
        let tool = ApprovalTool::new();
        let integration = AcpIntegration::builder()
            .name("agentkit-test")
            .approval_resolver(ClientPermissionResolver::new())
            .build()
            .unwrap();

        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn({
            let integration = integration.clone();
            let factory = TestFactory {
                adapter: adapter.clone(),
                tools: Some(ToolRegistry::new().with(tool.clone())),
                require_approval: true,
            };
            async move {
                AcpHeadlessRuntime::<MockAdapter>::builder()
                    .integration(integration)
                    .agent_factory(factory)
                    .serve_transport(agent_transport)
                    .await
            }
        });

        let client = agent_client_protocol::Client
            .builder()
            .on_receive_notification(
                async move |notification: acp::SessionNotification, _cx| {
                    let _ = notification;
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |_request: acp::RequestPermissionRequest, responder, _cx| {
                    responder.respond(acp::RequestPermissionResponse::new(
                        acp::RequestPermissionOutcome::Cancelled,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(client_transport, async move |cx| {
                cx.send_request(acp::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let cwd = std::env::current_dir()
                    .map_err(agent_client_protocol::Error::into_internal_error)?;
                let new_session = cx
                    .send_request(acp::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let response = cx
                    .send_request(acp::PromptRequest::new(
                        new_session.session_id.clone(),
                        vec![acp::ContentBlock::Text(acp::TextContent::new("use tool"))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, acp::StopReason::Cancelled);
                cx.send_request(acp::CloseSessionRequest::new(new_session.session_id))
                    .block_task()
                    .await?;
                Ok(())
            });
        tokio::time::timeout(std::time::Duration::from_secs(5), client)
            .await
            .expect("client timed out")
            .expect("client run");

        server.abort();
        let _ = server.await;
        assert_eq!(tool.call_count(), 0);
    }

    #[tokio::test]
    async fn headless_runtime_failed_turn_returns_rpc_error() {
        let adapter = MockAdapter::new();
        adapter.enqueue(failed_turn());
        let integration = AcpIntegration::builder()
            .name("agentkit-test")
            .approval_resolver(AutoApproveResolver)
            .build()
            .unwrap();

        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn({
            let integration = integration.clone();
            let factory = TestFactory {
                adapter: adapter.clone(),
                tools: None,
                require_approval: false,
            };
            async move {
                AcpHeadlessRuntime::<MockAdapter>::builder()
                    .integration(integration)
                    .agent_factory(factory)
                    .serve_transport(agent_transport)
                    .await
            }
        });

        let client = agent_client_protocol::Client.builder().connect_with(
            client_transport,
            async move |cx| {
                cx.send_request(acp::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let cwd = std::env::current_dir()
                    .map_err(agent_client_protocol::Error::into_internal_error)?;
                let new_session = cx
                    .send_request(acp::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let error = cx
                    .send_request(acp::PromptRequest::new(
                        new_session.session_id.clone(),
                        vec![acp::ContentBlock::Text(acp::TextContent::new("fail"))],
                    ))
                    .block_task()
                    .await
                    .err()
                    .expect("failed turn should return JSON-RPC error");
                assert!(
                    error
                        .to_string()
                        .contains("agent turn finished with an error")
                );
                cx.send_request(acp::CloseSessionRequest::new(new_session.session_id))
                    .block_task()
                    .await?;
                Ok(())
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), client)
            .await
            .expect("client timed out")
            .expect("client run");

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn headless_runtime_cancel_wins_over_simultaneous_approval_resolution() {
        let adapter = MockAdapter::new();
        adapter.enqueue_many([
            TurnScript::tool_call(ToolCallPart::new(
                "call-1",
                "approval_tool",
                json!({ "value": 1 }),
            )),
            streamed_text("tool should not run"),
        ]);
        let tool = ApprovalTool::new();
        let resolver = InterruptThenAllowResolver::default();
        let integration = AcpIntegration::builder()
            .name("agentkit-test")
            .approval_resolver(resolver.clone())
            .build()
            .unwrap();
        resolver.set_integration(integration.clone());

        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn({
            let integration = integration.clone();
            let factory = TestFactory {
                adapter: adapter.clone(),
                tools: Some(ToolRegistry::new().with(tool.clone())),
                require_approval: true,
            };
            async move {
                AcpHeadlessRuntime::<MockAdapter>::builder()
                    .integration(integration)
                    .agent_factory(factory)
                    .serve_transport(agent_transport)
                    .await
            }
        });

        let client = agent_client_protocol::Client
            .builder()
            .on_receive_notification(
                async move |notification: acp::SessionNotification, _cx| {
                    let _ = notification;
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, async move |cx| {
                cx.send_request(acp::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let cwd = std::env::current_dir()
                    .map_err(agent_client_protocol::Error::into_internal_error)?;
                let new_session = cx
                    .send_request(acp::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let response = cx
                    .send_request(acp::PromptRequest::new(
                        new_session.session_id.clone(),
                        vec![acp::ContentBlock::Text(acp::TextContent::new("use tool"))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, acp::StopReason::Cancelled);
                cx.send_request(acp::CloseSessionRequest::new(new_session.session_id))
                    .block_task()
                    .await?;
                Ok(())
            });
        tokio::time::timeout(std::time::Duration::from_secs(5), client)
            .await
            .expect("client timed out")
            .expect("client run");

        server.abort();
        let _ = server.await;
        assert_eq!(tool.call_count(), 0);
    }

    #[test]
    fn in_memory_approval_memory_remembers_allow_and_reject() {
        let memory = InMemoryApprovalMemory::new();
        let session = acp::SessionId::new("session-1");
        let request = ApprovalRequest::new(
            "approval-1",
            "test.approval",
            ApprovalReason::PolicyRequiresConfirmation,
            "approve test tool",
        );

        assert_eq!(memory.lookup(&session, &request), None);
        memory.remember(&session, &request, &AcpApprovalDecision::AllowAlways);
        assert_eq!(
            memory.lookup(&session, &request),
            Some(AcpApprovalDecision::AllowAlways)
        );

        memory.remember(
            &session,
            &request,
            &AcpApprovalDecision::RejectAlways {
                reason: Some("no".into()),
            },
        );
        assert_eq!(
            memory.lookup(&session, &request),
            Some(AcpApprovalDecision::RejectAlways {
                reason: Some("no".into())
            })
        );
    }

    #[test]
    fn approval_memory_is_scoped_by_session() {
        let memory = InMemoryApprovalMemory::new();
        let first_session = acp::SessionId::new("session-1");
        let second_session = acp::SessionId::new("session-2");
        let request = ApprovalRequest::new(
            "approval-1",
            "test.approval",
            ApprovalReason::PolicyRequiresConfirmation,
            "approve test tool",
        );

        memory.remember(&first_session, &request, &AcpApprovalDecision::AllowAlways);

        assert_eq!(memory.lookup(&second_session, &request), None);
    }

    #[test]
    fn patch_and_allow_is_not_remembered() {
        assert!(
            !AcpApprovalDecision::PatchAndAllow {
                input: json!({ "value": "patched" }),
            }
            .remember()
        );
    }

    #[test]
    fn approval_memory_key_uses_stable_target_not_volatile_metadata() {
        let memory = InMemoryApprovalMemory::new();
        let session = acp::SessionId::new("session-1");
        let mut first_metadata = MetadataMap::new();
        first_metadata.insert("volatile.call_nonce".into(), json!("first"));
        let first = ApprovalRequest::new(
            "approval-1",
            "filesystem.write",
            ApprovalReason::SensitivePath,
            "Write /tmp/allowed.txt",
        )
        .with_metadata(first_metadata);
        let mut second_metadata = MetadataMap::new();
        second_metadata.insert("volatile.call_nonce".into(), json!("second"));
        let second = ApprovalRequest::new(
            "approval-2",
            "filesystem.write",
            ApprovalReason::SensitivePath,
            "Write /tmp/allowed.txt",
        )
        .with_metadata(second_metadata);

        memory.remember(&session, &first, &AcpApprovalDecision::AllowAlways);

        assert_eq!(
            memory.lookup(&session, &second),
            Some(AcpApprovalDecision::AllowAlways)
        );
    }

    #[test]
    fn approval_memory_key_distinguishes_targets_without_metadata() {
        let memory = InMemoryApprovalMemory::new();
        let session = acp::SessionId::new("session-1");
        let first = ApprovalRequest::new(
            "approval-1",
            "filesystem.write",
            ApprovalReason::SensitivePath,
            "Write /tmp/one.txt",
        );
        let second = ApprovalRequest::new(
            "approval-2",
            "filesystem.write",
            ApprovalReason::SensitivePath,
            "Write /tmp/two.txt",
        );

        memory.remember(&session, &first, &AcpApprovalDecision::AllowAlways);

        assert_eq!(memory.lookup(&session, &second), None);
    }

    #[test]
    fn commit_part_preserves_other_open_part_kinds() {
        let mut part_kinds = HashMap::new();
        assert!(
            delta_to_update(
                &Delta::BeginPart {
                    part_id: PartId::new("reasoning"),
                    kind: PartKind::Reasoning,
                },
                &mut part_kinds,
            )
            .is_none()
        );
        assert!(
            delta_to_update(
                &Delta::BeginPart {
                    part_id: PartId::new("text"),
                    kind: PartKind::Text,
                },
                &mut part_kinds,
            )
            .is_none()
        );
        assert!(
            delta_to_update(
                &Delta::CommitPart {
                    part: Part::text("visible"),
                },
                &mut part_kinds,
            )
            .is_none()
        );

        let update = delta_to_update(
            &Delta::AppendText {
                part_id: PartId::new("reasoning"),
                chunk: "hidden".into(),
            },
            &mut part_kinds,
        )
        .expect("reasoning chunk should be routed");

        assert!(matches!(
            update,
            acp::SessionUpdate::AgentThoughtChunk(chunk)
                if matches!(&chunk.content, acp::ContentBlock::Text(text) if text.text == "hidden")
        ));
    }

    #[test]
    fn usage_update_carries_known_size_and_cost() {
        let mut usage =
            CoreUsage::new(TokenUsage::new(100, 25)).with_cost(CostUsage::new(0.42, "USD"));
        usage
            .metadata
            .insert("context_window".into(), json!(200_000_u64));

        let update = usage_update(&usage).expect("known context window should emit usage");

        assert_eq!(update.used, 125);
        assert_eq!(update.size, 200_000);
        assert_eq!(update.cost.as_ref().map(|cost| cost.amount), Some(0.42));
        assert_eq!(
            update.cost.as_ref().map(|cost| cost.currency.as_str()),
            Some("USD")
        );
    }

    #[test]
    fn usage_update_skips_unknown_context_window() {
        let usage = CoreUsage::new(TokenUsage::new(100, 25));

        assert_eq!(usage_update(&usage), None);
    }

    #[test]
    fn usage_update_excludes_reasoning_tokens_from_context_gauge() {
        let mut usage = CoreUsage::new(TokenUsage::new(100, 25).with_reasoning_tokens(7));
        usage
            .metadata
            .insert("context_window".into(), json!(200_000_u64));

        let update = usage_update(&usage).expect("known context window should emit usage");

        assert_eq!(update.used, 125);
    }

    #[test]
    fn usage_update_skips_when_token_counts_are_absent() {
        let mut usage = CoreUsage::default().with_cost(CostUsage::new(0.42, "USD"));
        usage
            .metadata
            .insert("context_window".into(), json!(200_000_u64));

        assert_eq!(usage_update(&usage), None);
    }

    #[test]
    fn finish_reason_error_is_rejected_by_stop_reason_mapping() {
        assert!(matches!(
            finish_reason_to_stop_reason(&FinishReason::Error),
            Err(AcpRuntimeError::Loop(_))
        ));
    }

    #[test]
    fn tool_output_parts_and_files_have_visible_content() {
        let parts = tool_output_content(&ToolOutput::parts(vec![
            Part::text("hello"),
            Part::structured(json!({ "ok": true })),
        ]))
        .expect("parts should render content");
        assert_eq!(parts.len(), 2);
        assert!(matches!(
            &parts[0],
            acp::ToolCallContent::Content(content)
                if matches!(&content.content, acp::ContentBlock::Text(text) if text.text == "hello")
        ));

        let files = tool_output_content(&ToolOutput::files(vec![
            FilePart::named("artifact.txt", DataRef::inline_text("artifact body"))
                .with_mime_type("text/plain"),
            FilePart::named("remote.txt", DataRef::uri("file:///tmp/remote.txt")),
        ]))
        .expect("files should render content");

        assert_eq!(files.len(), 2);
        assert!(matches!(
            &files[0],
            acp::ToolCallContent::Content(content)
                if matches!(&content.content, acp::ContentBlock::Resource(resource)
                    if matches!(&resource.resource, acp::EmbeddedResourceResource::TextResourceContents(text)
                        if text.text == "artifact body"))
        ));
        assert!(matches!(
            &files[1],
            acp::ToolCallContent::Content(content)
                if matches!(&content.content, acp::ContentBlock::ResourceLink(link)
                    if link.uri == "file:///tmp/remote.txt")
        ));
    }

    #[test]
    fn tool_output_non_inline_media_renders_as_resource_link() {
        let content = tool_output_content(&ToolOutput::parts(vec![Part::media(
            Modality::Image,
            "image/png",
            DataRef::uri("file:///tmp/image.png"),
        )]))
        .expect("media should render content");

        assert!(matches!(
            &content[0],
            acp::ToolCallContent::Content(content)
                if matches!(&content.content, acp::ContentBlock::ResourceLink(link)
                    if link.uri == "file:///tmp/image.png" && link.mime_type.as_deref() == Some("image/png"))
        ));
    }

    #[test]
    fn tool_output_inline_media_reencodes_non_base64_data_url() {
        let content = tool_output_content(&ToolOutput::parts(vec![Part::media(
            Modality::Image,
            "image/svg+xml",
            DataRef::inline_text("data:image/svg+xml,%3Csvg%2F%3E"),
        )]))
        .expect("media should render content");

        assert!(matches!(
            &content[0],
            acp::ToolCallContent::Content(content)
                if matches!(&content.content, acp::ContentBlock::Image(image)
                    if image.data == "PHN2Zy8+" && image.mime_type == "image/svg+xml")
        ));
    }

    #[test]
    fn tool_output_inline_media_base64_encodes_raw_inline_text() {
        let content = tool_output_content(&ToolOutput::parts(vec![Part::media(
            Modality::Image,
            "image/svg+xml",
            DataRef::inline_text("<svg/>"),
        )]))
        .expect("media should render content");

        assert!(matches!(
            &content[0],
            acp::ToolCallContent::Content(content)
                if matches!(&content.content, acp::ContentBlock::Image(image)
                    if image.data == "PHN2Zy8+" && image.mime_type == "image/svg+xml")
        ));
    }

    #[test]
    fn tool_output_inline_media_preserves_base64_data_url_payload() {
        let content = tool_output_content(&ToolOutput::parts(vec![Part::media(
            Modality::Audio,
            "audio/wav",
            DataRef::inline_text("data:audio/wav;base64,AQID"),
        )]))
        .expect("media should render content");

        assert!(matches!(
            &content[0],
            acp::ToolCallContent::Content(content)
                if matches!(&content.content, acp::ContentBlock::Audio(audio)
                    if audio.data == "AQID" && audio.mime_type == "audio/wav")
        ));
    }

    #[test]
    fn tool_output_custom_uri_is_visible() {
        let content = tool_output_content(&ToolOutput::parts(vec![Part::Custom(
            agentkit_core::CustomPart::new("artifact")
                .with_data(DataRef::uri("https://example.test/artifact")),
        )]))
        .expect("custom part should render content");

        assert!(matches!(
            &content[0],
            acp::ToolCallContent::Content(content)
                if matches!(&content.content, acp::ContentBlock::Text(text)
                    if text.text == "https://example.test/artifact")
        ));
    }

    #[test]
    fn tool_output_blob_payload_base64_encodes_raw_inline_text() {
        let content = tool_output_content(&ToolOutput::files(vec![
            FilePart::named("data.json", DataRef::inline_text(r#"{"k":1}"#))
                .with_mime_type("application/json"),
        ]))
        .expect("file should render content");

        let acp::ToolCallContent::Content(content) = &content[0] else {
            panic!("expected content");
        };
        let acp::ContentBlock::Resource(resource) = &content.content else {
            panic!("expected embedded resource");
        };
        let acp::EmbeddedResourceResource::BlobResourceContents(blob) = &resource.resource else {
            panic!("expected blob resource");
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(blob.blob.as_bytes())
            .expect("blob should be valid base64");
        assert_eq!(decoded, br#"{"k":1}"#);
    }

    #[test]
    fn tool_output_blob_payload_base64_encodes_inline_bytes() {
        let content = tool_output_content(&ToolOutput::files(vec![
            FilePart::named("data.bin", DataRef::inline_bytes([1_u8, 2, 3]))
                .with_mime_type("application/octet-stream"),
        ]))
        .expect("file should render content");

        assert!(matches!(
            &content[0],
            acp::ToolCallContent::Content(content)
                if matches!(&content.content, acp::ContentBlock::Resource(resource)
                    if matches!(&resource.resource, acp::EmbeddedResourceResource::BlobResourceContents(blob)
                        if blob.blob == "AQID"))
        ));
    }

    #[tokio::test]
    async fn patched_approval_requires_call_id() {
        let adapter = MockAdapter::new();
        let integration = AcpIntegration::builder()
            .name("agentkit-test")
            .approval_resolver(AutoApproveResolver)
            .build()
            .unwrap();
        let factory = TestFactory {
            adapter,
            tools: None,
            require_approval: false,
        };
        let ctx = AcpAgentFactoryContext {
            acp_session_id: acp::SessionId::new("session-1"),
            agentkit_session_id: AgentkitSessionId::new("session-1"),
            cwd: std::env::current_dir().unwrap(),
            additional_directories: Vec::new(),
            integration: Arc::new(integration),
            cancellation: CancellationController::new().handle(),
            metadata: MetadataMap::new(),
        };
        let mut driver = factory.start(ctx).await.unwrap();
        let request = ApprovalRequest::new(
            "approval-1",
            "test.approval",
            ApprovalReason::PolicyRequiresConfirmation,
            "approve test tool",
        );

        let error = apply_approval_decision(
            &mut driver,
            &request,
            AcpApprovalDecision::PatchAndAllow {
                input: json!({ "value": "patched" }),
            },
        )
        .expect_err("missing call id should error");
        assert!(matches!(error, AcpRuntimeError::Unsupported(_)));
    }
}
