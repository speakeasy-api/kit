//! Model Context Protocol integration for agentkit, built on top of [`rmcp`].
//!
//! This crate exposes:
//!
//! - [`McpServerConfig`] / [`McpTransportBinding`] / [`StdioTransportConfig`] /
//!   [`StreamableHttpTransportConfig`] — declarative transport configuration.
//! - [`McpConnection`] — a live, single-server connection wrapping
//!   [`rmcp::service::RunningService`].
//! - [`McpServerManager`] — multi-server lifecycle, discovery, catalog diffing,
//!   and auth replay.
//! - [`McpServerHandle`], [`McpToolExecutor`], [`McpToolAdapter`],
//!   [`McpResourceHandle`], [`McpPromptHandle`],
//!   [`McpCapabilityProvider`] — bridges into the agentkit `Tool` / capabilities
//!   systems.
//!
//! Wire-protocol types (`CallToolResult`, `ReadResourceResult`, `Content`,
//! `ToolAnnotations`, `Prompt`, sampling/elicitation/roots payloads, …) are
//! re-exported from [`rmcp::model`] directly — there is no parallel
//! agentkit-side vocabulary. As `rmcp` tracks new MCP spec revisions, those
//! types and their fields propagate into agentkit unchanged.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use agentkit_capabilities::{
    CapabilityContext, CapabilityError, CapabilityProvider, Invocable, PromptContents,
    PromptDescriptor, PromptId, PromptProvider, ResourceContents, ResourceDescriptor, ResourceId,
    ResourceProvider,
};
use agentkit_core::{
    DataRef, Item, ItemKind, MediaPart, MetadataMap, Modality, Part, TextPart, ToolOutput,
    ToolResultPart,
};
use agentkit_tools_core::{
    AllowAllPermissions, CatalogReader, CatalogWriter, PermissionChecker, Tool, ToolAnnotations,
    ToolCapabilityProvider, ToolContext, ToolError, ToolName, ToolRegistry, ToolRequest,
    ToolResult, ToolSpec, dynamic_catalog,
};
use async_trait::async_trait;
use futures_util::future::{join_all, try_join_all};
use futures_util::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::handler::client::ClientHandler;
use rmcp::model as rmcp_model;
use rmcp::service::{ClientInitializeError, Peer, RoleClient, RunningService, ServiceError};
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, InsufficientScopeError, StreamableHttpClient as RmcpStreamableHttpClient,
    StreamableHttpClientTransportConfig as RmcpStreamableHttpClientTransportConfig,
    StreamableHttpError, StreamableHttpPostResponse,
};
use rmcp::transport::{
    ConfigureCommandExt, DynamicTransportError, StreamableHttpClientTransport, TokioChildProcess,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sse_stream::{Error as SseError, Sse};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, mpsc};

/// Re-exports of the rmcp wire-protocol types this crate now surfaces directly
/// instead of wrapping. Pull these in to pattern-match on tool annotations,
/// content blocks, structured tool output, embedded resources, sampling /
/// elicitation requests, progress and log notifications, etc.
pub use rmcp::model::{
    Annotations as McpAnnotations, AudioContent, CallToolResult,
    CancelledNotificationParam as McpCancelledNotificationParam,
    ClientCapabilities as McpClientCapabilities, Content,
    CreateElicitationRequestParams as McpCreateElicitationRequestParams,
    CreateElicitationResult as McpCreateElicitationResult,
    CreateMessageRequestParams as McpCreateMessageRequestParams,
    CreateMessageResult as McpCreateMessageResult, ElicitationAction as McpElicitationAction,
    ElicitationCapability as McpElicitationCapability, EmbeddedResource,
    FormElicitationCapability as McpFormElicitationCapability, GetPromptResult, ImageContent,
    Implementation as McpImplementation, ListRootsResult as McpListRootsResult,
    LoggingLevel as McpLoggingLevel,
    LoggingMessageNotificationParam as McpLoggingMessageNotificationParam,
    ProgressNotificationParam as McpProgressNotificationParam, Prompt as McpPrompt, PromptArgument,
    PromptMessage, PromptMessageContent, PromptMessageRole, RawAudioContent, RawContent,
    RawEmbeddedResource, RawImageContent, RawResource as McpRawResource, RawTextContent,
    ReadResourceResult, Resource as McpResource, ResourceContents as McpResourceContents,
    ResourceUpdatedNotificationParam as McpResourceUpdatedNotificationParam, Root as McpRoot,
    RootsCapabilities as McpRootsCapabilities, SamplingCapability as McpSamplingCapability,
    SamplingMessage as McpSamplingMessage, SetLevelRequestParams as McpSetLevelRequestParams,
    TextContent, Tool as McpTool, ToolAnnotations as McpToolAnnotations,
    UrlElicitationCapability as McpUrlElicitationCapability,
};

/// Re-export of the JSON-RPC client→server envelope handed to
/// [`McpHttpClient::post_message`].
pub use rmcp::model::ClientJsonRpcMessage;

/// Re-exports of the rmcp Streamable HTTP transport types used by
/// [`McpHttpClient`] implementations.
pub use rmcp::transport::streamable_http_client::{
    StreamableHttpError as McpStreamableHttpError,
    StreamableHttpPostResponse as McpStreamableHttpPostResponse,
};

/// Re-export of the SSE event/error types referenced by [`McpHttpClient::get_stream`].
pub use sse_stream::{Error as McpSseError, Sse as McpSse};

/// Alias for [`McpTool`].
pub type McpToolDescriptor = McpTool;
/// Alias for [`McpResource`].
pub type McpResourceDescriptor = McpResource;
/// Alias for [`McpPrompt`].
pub type McpPromptDescriptor = McpPrompt;

/// An auth challenge raised by an MCP server during a tool call, resource
/// read, prompt fetch, or connection handshake.
///
/// Hosts handle these via an [`McpAuthResponder`] registered on
/// [`McpHandlerConfig::with_auth_responder`]. The responder is invoked
/// inline by [`McpToolAdapter::invoke`] (and equivalent paths in
/// [`McpServerManager`]) — auth never crosses the executor boundary as a
/// loop interrupt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthRequest {
    /// Unique identifier for this auth challenge.
    pub id: String,
    /// Name of the authentication provider (e.g. `"github"`, `"google"`).
    pub provider: String,
    /// The MCP operation that triggered the auth requirement.
    pub operation: AuthOperation,
    /// Provider-specific challenge data (e.g. OAuth URLs, scopes).
    pub challenge: MetadataMap,
}

impl AuthRequest {
    /// Convenience: returns the MCP server id this challenge targets, if any.
    pub fn server_id(&self) -> Option<&str> {
        self.operation.server_id()
    }
}

/// The MCP operation that triggered an [`AuthRequest`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthOperation {
    /// Connecting to an MCP server.
    McpConnect {
        server_id: String,
        metadata: MetadataMap,
    },
    /// Invoking a tool on an MCP server.
    McpToolCall {
        server_id: String,
        tool_name: String,
        input: Value,
        metadata: MetadataMap,
    },
    /// Reading a resource from an MCP server.
    McpResourceRead {
        server_id: String,
        resource_id: String,
        metadata: MetadataMap,
    },
    /// Fetching a prompt from an MCP server.
    McpPromptGet {
        server_id: String,
        prompt_id: String,
        args: Value,
        metadata: MetadataMap,
    },
    /// Any other MCP method that requires auth (resource subscribe/unsubscribe,
    /// logging level changes, future protocol additions). The typed variants
    /// above cover the common cases; this catch-all preserves the method name
    /// and JSON params verbatim for hosts that need to render or log them.
    McpOther {
        server_id: String,
        method: String,
        params: Value,
        metadata: MetadataMap,
    },
}

impl AuthOperation {
    /// Returns the MCP server ID this operation targets.
    pub fn server_id(&self) -> Option<&str> {
        match self {
            Self::McpConnect { server_id, .. }
            | Self::McpToolCall { server_id, .. }
            | Self::McpResourceRead { server_id, .. }
            | Self::McpPromptGet { server_id, .. }
            | Self::McpOther { server_id, .. } => Some(server_id.as_str()),
        }
    }
}

/// Outcome of an [`AuthRequest`] after the host's [`McpAuthResponder`] runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthResolution {
    /// The host obtained credentials.
    Provided {
        request: AuthRequest,
        credentials: MetadataMap,
    },
    /// The host cancelled the auth flow.
    Cancelled { request: AuthRequest },
}

impl AuthResolution {
    /// Builds a successful auth resolution.
    pub fn provided(request: AuthRequest, credentials: MetadataMap) -> Self {
        Self::Provided {
            request,
            credentials,
        }
    }

    /// Builds a cancelled auth resolution.
    pub fn cancelled(request: AuthRequest) -> Self {
        Self::Cancelled { request }
    }

    /// Returns the underlying [`AuthRequest`] regardless of variant.
    pub fn request(&self) -> &AuthRequest {
        match self {
            Self::Provided { request, .. } | Self::Cancelled { request } => request,
        }
    }
}

/// Host-supplied resolver for MCP auth challenges.
///
/// Install one via [`McpHandlerConfig::with_auth_responder`]. When an MCP
/// server returns an auth challenge during a tool call, resource read, or
/// prompt fetch, the adapter invokes [`McpAuthResponder::resolve`] inline,
/// applies the resulting credentials to the [`McpConnection`], and retries
/// the original operation. Auth never surfaces as a loop interrupt.
///
/// Hosts that want to interleave the auth UI with the loop's main thread
/// implement a thin channel-bridging responder (responder sends the
/// challenge to the UI thread on a `mpsc::Sender`, awaits a `oneshot`
/// reply with the resolution).
#[async_trait]
pub trait McpAuthResponder: Send + Sync + 'static {
    async fn resolve(&self, request: AuthRequest) -> Result<AuthResolution, McpError>;
}

/// Typed view of a JSON-RPC error returned by an MCP server for an invoked
/// method.
///
/// Surfaced by [`McpError::Invocation`] so callers can branch on the
/// underlying error code without re-parsing strings. The variants cover
/// every rmcp [`rmcp::model::ErrorCode`] constant defined at the time of
/// writing; anything else (custom server codes, future protocol additions)
/// lands in [`Self::Other`] with the original code preserved.
///
/// For the URL elicitation case ([`rmcp::model::ErrorCode::URL_ELICITATION_REQUIRED`])
/// the `data` payload is best-effort parsed into [`UrlElicitationData`].
/// When the server's payload does not match the documented shape the typed
/// `data` slot is `None` but `raw_data` always preserves the original
/// [`serde_json::Value`] so callers can fall back to ad-hoc inspection.
#[derive(Debug, Clone, thiserror::Error)]
pub enum McpInvocationError {
    /// JSON-RPC error `-32042` (URL elicitation required).
    #[error("url elicitation required: {message}")]
    UrlElicitation {
        /// Human-readable message from the server.
        message: String,
        /// Typed view of the server's `data` payload, when it matched the
        /// documented URL elicitation shape.
        data: Option<UrlElicitationData>,
        /// The original `data` value, preserved verbatim.
        raw_data: Option<serde_json::Value>,
    },
    /// JSON-RPC error `-32600` (invalid request).
    #[error("invalid request: {message}")]
    InvalidRequest {
        message: String,
        data: Option<serde_json::Value>,
    },
    /// JSON-RPC error `-32601` (method not found).
    #[error("method not found: {message}")]
    MethodNotFound {
        message: String,
        data: Option<serde_json::Value>,
    },
    /// JSON-RPC error `-32602` (invalid params).
    #[error("invalid params: {message}")]
    InvalidParams {
        message: String,
        data: Option<serde_json::Value>,
    },
    /// JSON-RPC error `-32603` (internal error).
    #[error("internal error: {message}")]
    InternalError {
        message: String,
        data: Option<serde_json::Value>,
    },
    /// JSON-RPC error `-32700` (parse error).
    #[error("parse error: {message}")]
    ParseError {
        message: String,
        data: Option<serde_json::Value>,
    },
    /// JSON-RPC error `-32002` (resource not found).
    #[error("resource not found: {message}")]
    ResourceNotFound {
        message: String,
        data: Option<serde_json::Value>,
    },
    /// Forward-compat for custom server codes and any future rmcp additions
    /// not yet recognized by this crate.
    #[error("mcp error code {code}: {message}")]
    Other {
        code: i32,
        message: String,
        data: Option<serde_json::Value>,
    },
}

/// Typed payload for the URL elicitation error case.
///
/// Mirrors the shape of [`rmcp::model::CreateElicitationRequestParams::UrlElicitationParams`]
/// (camelCase on the wire). Server messages that include extra fields are
/// accepted; missing required fields make typed parsing fail and the
/// surrounding [`McpInvocationError::UrlElicitation`] preserves the raw
/// payload instead.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UrlElicitationData {
    /// The URL where the user can complete the elicitation.
    pub url: String,
    /// The server-issued identifier for this elicitation request.
    pub elicitation_id: String,
    /// Optional human-readable message that accompanied the payload.
    #[serde(default)]
    pub message: Option<String>,
}

impl McpInvocationError {
    /// Lifts an rmcp wire error into the typed enum. Infallible: well-known
    /// codes attempt a typed `data` parse and degrade to a raw-only view
    /// when parsing fails; unrecognized codes land in [`Self::Other`].
    pub fn from_error_data(err: rmcp::model::ErrorData) -> Self {
        let rmcp::model::ErrorData {
            code,
            message,
            data,
        } = err;
        let message = message.into_owned();
        match code {
            rmcp::model::ErrorCode::URL_ELICITATION_REQUIRED => {
                let typed = data.as_ref().and_then(|value| {
                    serde_json::from_value::<UrlElicitationData>(value.clone()).ok()
                });
                Self::UrlElicitation {
                    message,
                    data: typed,
                    raw_data: data,
                }
            }
            rmcp::model::ErrorCode::INVALID_REQUEST => Self::InvalidRequest { message, data },
            rmcp::model::ErrorCode::METHOD_NOT_FOUND => Self::MethodNotFound { message, data },
            rmcp::model::ErrorCode::INVALID_PARAMS => Self::InvalidParams { message, data },
            rmcp::model::ErrorCode::INTERNAL_ERROR => Self::InternalError { message, data },
            rmcp::model::ErrorCode::PARSE_ERROR => Self::ParseError { message, data },
            rmcp::model::ErrorCode::RESOURCE_NOT_FOUND => Self::ResourceNotFound { message, data },
            other => Self::Other {
                code: other.0,
                message,
                data,
            },
        }
    }

    /// Returns the underlying JSON-RPC error code.
    pub fn code(&self) -> i32 {
        match self {
            Self::UrlElicitation { .. } => rmcp::model::ErrorCode::URL_ELICITATION_REQUIRED.0,
            Self::InvalidRequest { .. } => rmcp::model::ErrorCode::INVALID_REQUEST.0,
            Self::MethodNotFound { .. } => rmcp::model::ErrorCode::METHOD_NOT_FOUND.0,
            Self::InvalidParams { .. } => rmcp::model::ErrorCode::INVALID_PARAMS.0,
            Self::InternalError { .. } => rmcp::model::ErrorCode::INTERNAL_ERROR.0,
            Self::ParseError { .. } => rmcp::model::ErrorCode::PARSE_ERROR.0,
            Self::ResourceNotFound { .. } => rmcp::model::ErrorCode::RESOURCE_NOT_FOUND.0,
            Self::Other { code, .. } => *code,
        }
    }
}

/// Userland hook invoked when an MCP server returns a JSON-RPC error for an
/// invoked method.
///
/// Lets the host translate well-known errors (e.g. URL elicitation
/// challenges) into a synthesized tool result the agent can render —
/// without agentkit-mcp baking in any specific UX or response policy.
///
/// Install one via [`McpHandlerConfig::with_error_responder`]. When set,
/// [`McpToolAdapter::invoke`] forwards every JSON-RPC error to the
/// responder before falling back to [`ToolError::ExecutionFailed`]; the
/// responder returns either a synthesized [`CallToolResult`] (treated as a
/// successful call so `structured_content` flows through
/// [`ToolOutput::Structured`]) or [`ErrorResponderOutcome::PassThrough`] to
/// preserve the default failure path.
#[async_trait]
pub trait McpErrorResponder: Send + Sync + 'static {
    /// Inspects an invocation error and decides whether to synthesize a
    /// replacement [`CallToolResult`] or propagate the error.
    async fn handle(
        &self,
        error: &McpInvocationError,
        ctx: McpErrorContext<'_>,
    ) -> ErrorResponderOutcome;
}

/// Context describing which server / method / input produced the
/// invocation error currently being inspected by [`McpErrorResponder::handle`].
pub struct McpErrorContext<'a> {
    /// The server that returned the error.
    pub server_id: &'a McpServerId,
    /// The MCP method that was invoked.
    pub method: &'a McpMethod,
    /// The input payload supplied to the invocation, when available.
    pub input: Option<&'a serde_json::Value>,
}

/// Decision returned by an [`McpErrorResponder`].
pub enum ErrorResponderOutcome {
    /// Replace the error with a synthesized successful response. The
    /// returned [`CallToolResult`] flows through agentkit-mcp's normal
    /// tool-result conversion: `structured_content` becomes
    /// [`ToolOutput::Structured`], `content` becomes text / media parts,
    /// and `is_error` is honoured.
    SynthesizeResult(CallToolResult),
    /// Defer to default behavior; the invocation error continues to surface
    /// as [`ToolError::ExecutionFailed`].
    PassThrough,
}

/// Unique identifier for a registered MCP server.
///
/// Each MCP server in a [`McpServerManager`] is addressed by its `McpServerId`.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct McpServerId(pub String);

impl McpServerId {
    /// Creates a new server identifier from any string-like value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for McpServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Configuration for an MCP server that communicates over standard I/O.
///
/// The specified command is spawned as a child process; rmcp drives the
/// JSON-RPC framing over its stdin/stdout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StdioTransportConfig {
    /// The executable to launch (e.g. `"npx"`, `"python"`, `"node"`).
    pub command: String,
    /// Command-line arguments passed to the executable.
    pub args: Vec<String>,
    /// Additional environment variables set for the child process.
    pub env: Vec<(String, String)>,
    /// Optional working directory for the child process.
    pub cwd: Option<std::path::PathBuf>,
}

impl StdioTransportConfig {
    /// Creates a new stdio transport configuration for the given command.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
        }
    }

    /// Appends a command-line argument. Returns `self` for chaining.
    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Adds an environment variable for the child process. Returns `self` for chaining.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Sets the working directory for the child process. Returns `self` for chaining.
    pub fn with_cwd(mut self, cwd: impl Into<std::path::PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
}

/// Configuration for an MCP server that communicates over the MCP Streamable HTTP transport.
#[derive(Clone, Default)]
pub struct StreamableHttpTransportConfig {
    /// The MCP endpoint URL to connect to.
    pub url: String,
    /// Static bearer token sent as an HTTP `Authorization: Bearer ...` header.
    ///
    /// Ignored when [`Self::http_client`] is set, since the custom client owns
    /// authorization for every request.
    pub bearer_token: Option<String>,
    /// Static custom HTTP headers sent with every Streamable HTTP request.
    ///
    /// Ignored when [`Self::http_client`] is set.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Optional caller-supplied HTTP client.
    ///
    /// When `Some`, agentkit-mcp routes every Streamable HTTP request through
    /// the provided implementation instead of rmcp's default reqwest client.
    /// This is the seam to inject dynamic bearers, request signing, retry
    /// middleware, custom TLS, and so on. See [`McpHttpClient`].
    pub http_client: Option<Arc<dyn McpHttpClient>>,
}

impl fmt::Debug for StreamableHttpTransportConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamableHttpTransportConfig")
            .field("url", &self.url)
            .field(
                "bearer_token",
                &self.bearer_token.as_deref().map(|_| "<redacted>"),
            )
            .field("headers", &self.headers)
            .field(
                "http_client",
                &self.http_client.as_ref().map(|_| "<custom>"),
            )
            .finish()
    }
}

impl StreamableHttpTransportConfig {
    /// Creates a new Streamable HTTP transport configuration for the given MCP endpoint.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            bearer_token: None,
            headers: Vec::new(),
            http_client: None,
        }
    }

    /// Sets a static bearer token for Streamable HTTP authorization.
    ///
    /// Ignored when a custom [`McpHttpClient`] is installed via
    /// [`Self::with_http_client`].
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    /// Installs a caller-supplied HTTP client for every Streamable HTTP
    /// request issued by this transport.
    ///
    /// This is the only seam capable of producing per-request dynamic state
    /// (rotating bearers, request signing, distributed-tracing headers).
    /// rmcp's default reqwest path is bypassed entirely when this is set, so
    /// implementations are responsible for forwarding `auth_header` /
    /// `custom_headers` if they want the static config to keep applying.
    pub fn with_http_client(mut self, client: Arc<dyn McpHttpClient>) -> Self {
        self.http_client = Some(client);
        self
    }

    /// Adds a static HTTP header for every Streamable HTTP request.
    ///
    /// Reserved MCP session and protocol headers are still managed by RMCP.
    /// Ignored when a custom [`McpHttpClient`] is installed.
    pub fn with_header<N, V>(mut self, name: N, value: V) -> Result<Self, McpError>
    where
        N: TryInto<HeaderName>,
        N::Error: fmt::Display,
        V: TryInto<HeaderValue>,
        V::Error: fmt::Display,
    {
        let name = name
            .try_into()
            .map_err(|error| McpError::Transport(format!("invalid HTTP header name: {error}")))?;
        let value = value
            .try_into()
            .map_err(|error| McpError::Transport(format!("invalid HTTP header value: {error}")))?;
        self.headers.push((name, value));
        Ok(self)
    }
}

/// Type alias for the SSE stream returned by [`McpHttpClient::get_stream`].
pub type McpSseStream = BoxStream<'static, Result<Sse, SseError>>;

/// Pluggable HTTP transport for the MCP Streamable HTTP client.
///
/// Mirrors [`rmcp::transport::streamable_http_client::StreamableHttpClient`]
/// but is dyn-compatible (boxed via `async_trait`) so the configuration can
/// store an `Arc<dyn McpHttpClient>` without genericizing every type that
/// flows through [`McpServerConfig`] / [`McpTransportBinding`].
///
/// The associated error type is fixed to [`reqwest::Error`] so that
/// agentkit-mcp's auth-challenge detection (which downcasts to
/// [`StreamableHttpError<reqwest::Error>`]) keeps working — implementations
/// that wrap a non-reqwest backend should map their failures into a
/// `reqwest::Error` before returning.
///
/// All three methods are invoked by rmcp's worker on every protocol op.
/// `auth_header` and `custom_headers` carry the values resolved from
/// [`StreamableHttpTransportConfig`] at connection time; implementations are
/// free to ignore them and inject their own per-call values (e.g. a fresh
/// bearer pulled from a runtime registry).
#[async_trait]
pub trait McpHttpClient: Send + Sync + 'static {
    /// POSTs a single client→server JSON-RPC message. The response carries
    /// either a JSON body or an SSE stream depending on what the server
    /// negotiates.
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<reqwest::Error>>;

    /// Tears down a server-issued session (HTTP DELETE).
    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<reqwest::Error>>;

    /// Opens a server→client SSE stream (HTTP GET) for push notifications and
    /// reconnect resumes.
    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<McpSseStream, StreamableHttpError<reqwest::Error>>;
}

/// Internal newtype that adapts an `Arc<dyn McpHttpClient>` to rmcp's
/// generic, non-dyn-compatible [`RmcpStreamableHttpClient`] trait.
#[derive(Clone)]
struct DynHttpClient(Arc<dyn McpHttpClient>);

impl RmcpStreamableHttpClient for DynHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<reqwest::Error>> {
        self.0
            .post_message(uri, message, session_id, auth_header, custom_headers)
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<reqwest::Error>> {
        self.0
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<McpSseStream, StreamableHttpError<reqwest::Error>> {
        self.0
            .get_stream(uri, session_id, last_event_id, auth_header, custom_headers)
            .await
    }
}

/// Selects which transport an MCP server should use.
#[derive(Clone, Debug)]
pub enum McpTransportBinding {
    /// Communicate over the child process's stdin/stdout.
    Stdio(StdioTransportConfig),
    /// Communicate over the MCP Streamable HTTP transport.
    StreamableHttp(StreamableHttpTransportConfig),
}

/// Full configuration for a single MCP server.
#[derive(Clone, Debug)]
pub struct McpServerConfig {
    /// Unique identifier for this server.
    pub id: McpServerId,
    /// Transport binding that determines how communication happens.
    pub transport: McpTransportBinding,
    /// Arbitrary metadata attached to this server configuration.
    pub metadata: MetadataMap,
}

impl McpServerConfig {
    /// Creates a new server configuration with the given identifier and transport.
    pub fn new(id: impl Into<String>, transport: McpTransportBinding) -> Self {
        Self {
            id: McpServerId::new(id),
            transport,
            metadata: MetadataMap::new(),
        }
    }

    /// Creates a stdio-backed server configuration.
    pub fn stdio(id: impl Into<String>, command: impl Into<String>) -> Self {
        Self::new(
            id,
            McpTransportBinding::Stdio(StdioTransportConfig::new(command)),
        )
    }

    /// Creates a Streamable HTTP-backed server configuration.
    pub fn streamable_http(id: impl Into<String>, url: impl Into<String>) -> Self {
        Self::new(
            id,
            McpTransportBinding::StreamableHttp(StreamableHttpTransportConfig::new(url)),
        )
    }

    /// Replaces the configuration metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

type CustomNamespace = Arc<dyn Fn(&McpServerId, &str) -> String + Send + Sync>;

/// Strategy used to derive the agentkit-side tool name for an MCP tool.
///
/// The default (`Default`) preserves agentkit's historical
/// `mcp_<server>_<tool>` shape so that names satisfy provider validators
/// that only allow `[a-zA-Z0-9_-]` (e.g. Anthropic on Vertex). Use
/// [`McpToolNamespace::None`] when the calling provider already namespaces
/// remote tools, or [`McpToolNamespace::Custom`] for a bespoke scheme.
#[derive(Clone, Default)]
pub enum McpToolNamespace {
    /// Format names as `mcp_<server>_<tool>`.
    #[default]
    Default,
    /// Use the raw MCP tool name with no prefix at all.
    None,
    /// Apply a caller-supplied function for full control.
    Custom(CustomNamespace),
}

impl fmt::Debug for McpToolNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("McpToolNamespace::Default"),
            Self::None => f.write_str("McpToolNamespace::None"),
            Self::Custom(_) => f.write_str("McpToolNamespace::Custom(<fn>)"),
        }
    }
}

impl McpToolNamespace {
    /// Builds a custom namespace from a closure.
    pub fn custom(f: impl Fn(&McpServerId, &str) -> String + Send + Sync + 'static) -> Self {
        Self::Custom(Arc::new(f))
    }

    /// Applies the namespace strategy to produce the agentkit tool name.
    pub fn apply(&self, server_id: &McpServerId, tool_name: &str) -> String {
        match self {
            Self::Default => format!("mcp_{server_id}_{tool_name}"),
            Self::None => tool_name.to_string(),
            Self::Custom(f) => f(server_id, tool_name),
        }
    }

    /// Recovers the raw MCP tool name from an agentkit-side name. Returns
    /// `None` for [`Self::Custom`] (no general inverse) or when the name
    /// doesn't match the expected shape.
    pub fn unapply(&self, server_id: &McpServerId, agentkit_name: &str) -> Option<String> {
        match self {
            Self::Default => agentkit_name
                .strip_prefix(&format!("mcp_{server_id}_"))
                .map(str::to_string),
            Self::None => Some(agentkit_name.to_string()),
            Self::Custom(_) => None,
        }
    }
}

/// A snapshot of all capabilities discovered from a single MCP server.
///
/// Tools, resources, and prompts are stored as raw rmcp wire types
/// ([`McpTool`], [`McpResource`], [`McpPrompt`]) so that consumers see the
/// full typed surface — `Tool::annotations`, `Tool::output_schema`,
/// `Tool::execution`, `Tool::icons`; `Resource::title` / `mime_type` /
/// `size`; `Prompt::arguments` (with the typed `required` flag and per-arg
/// `description`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpDiscoverySnapshot {
    /// The server this snapshot was taken from.
    pub server_id: McpServerId,
    /// Tools advertised by the server.
    pub tools: Vec<McpTool>,
    /// Resources advertised by the server.
    pub resources: Vec<McpResource>,
    /// Prompts advertised by the server.
    pub prompts: Vec<McpPrompt>,
    /// Arbitrary metadata attached to this snapshot.
    pub metadata: MetadataMap,
}

/// Catalog and lifecycle events emitted by [`McpServerManager`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpCatalogEvent {
    /// A server connected and completed initial discovery.
    ServerConnected { server_id: McpServerId },
    /// A server disconnected.
    ServerDisconnected { server_id: McpServerId },
    /// The server's tool list changed.
    ToolsChanged {
        server_id: McpServerId,
        added: Vec<String>,
        removed: Vec<String>,
        changed: Vec<String>,
    },
    /// The server's resource list changed.
    ResourcesChanged {
        server_id: McpServerId,
        added: Vec<String>,
        removed: Vec<String>,
        changed: Vec<String>,
    },
    /// The server's prompt list changed.
    PromptsChanged {
        server_id: McpServerId,
        added: Vec<String>,
        removed: Vec<String>,
        changed: Vec<String>,
    },
    /// Authentication state changed for a server.
    AuthChanged { server_id: McpServerId },
    /// A catalog refresh failed.
    RefreshFailed {
        server_id: McpServerId,
        message: String,
    },
}

/// Capabilities advertised by an MCP server during the `initialize` handshake.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerCapabilities {
    /// Advertised `tools` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    /// Advertised `resources` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
    /// Advertised `prompts` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<PromptsCapability>,
    /// Advertised `logging` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<LoggingCapability>,
}

impl McpServerCapabilities {
    /// Returns a capabilities struct with every top-level capability
    /// advertised. Useful for tests.
    pub fn all() -> Self {
        Self {
            tools: Some(ToolsCapability::default()),
            resources: Some(ResourcesCapability::default()),
            prompts: Some(PromptsCapability::default()),
            logging: Some(LoggingCapability::default()),
        }
    }
}

/// Tools sub-capability flags from the MCP `initialize` response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    /// Server emits `notifications/tools/list_changed` when the catalog changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

/// Resources sub-capability flags from the MCP `initialize` response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesCapability {
    /// Server supports `resources/subscribe`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribe: Option<bool>,
    /// Server emits `notifications/resources/list_changed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

/// Prompts sub-capability flags from the MCP `initialize` response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptsCapability {
    /// Server emits `notifications/prompts/list_changed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

/// Logging sub-capability. Spec reserves the key with no defined sub-fields yet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggingCapability {}

/// Server-originated catalog notifications observed by [`McpClientHandler`].
///
/// Drained by [`McpConnection`] inside
/// [`McpServerManager::refresh_changed_catalogs`] to trigger re-discovery of
/// the affected capability lists. For richer push-style consumption (progress,
/// logging, resource updates, cancellation), subscribe via
/// [`McpConnection::subscribe_events`] and pattern-match on
/// [`McpServerEvent`].
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Debug)]
pub enum McpServerNotification {
    /// Server announced `notifications/tools/list_changed`.
    ToolsChanged,
    /// Server announced `notifications/resources/list_changed`.
    ResourcesChanged,
    /// Server announced `notifications/prompts/list_changed`.
    PromptsChanged,
}

/// Server-pushed events broadcast to every [`McpConnection::subscribe_events`]
/// receiver.
///
/// Covers the rmcp client-handler notification surface that does not feed the
/// catalog refresh path: progress, logging, resource updates, cancellation,
/// plus list-changed announcements (also delivered over the legacy
/// [`McpServerNotification`] channel).
#[derive(Clone, Debug)]
pub enum McpServerEvent {
    /// `notifications/progress` from the server, scoped to a
    /// `progress_token` issued in a previous request.
    Progress(McpProgressNotificationParam),
    /// `notifications/message` (server log emission). Drives the optional
    /// log-level negotiation initiated by [`McpConnection::set_logging_level`].
    Logging(McpLoggingMessageNotificationParam),
    /// `notifications/resources/updated` for a resource the client previously
    /// subscribed to via [`McpConnection::subscribe_resource`].
    ResourceUpdated(McpResourceUpdatedNotificationParam),
    /// `notifications/tools/list_changed`.
    ToolListChanged,
    /// `notifications/resources/list_changed`.
    ResourceListChanged,
    /// `notifications/prompts/list_changed`.
    PromptListChanged,
    /// `notifications/cancelled` from the server, requesting cancellation of
    /// an in-flight client request.
    Cancelled(McpCancelledNotificationParam),
}

/// Pluggable handler invoked when an MCP server issues `sampling/createMessage`.
///
/// Install one via [`McpHandlerConfig::with_sampling_responder`] to expose
/// the host application's LLM as a sampling target for connected MCP servers.
#[async_trait]
pub trait McpSamplingResponder: Send + Sync + 'static {
    /// Produces a sampled completion in response to a server-initiated
    /// `sampling/createMessage` request.
    async fn create_message(
        &self,
        params: McpCreateMessageRequestParams,
    ) -> Result<McpCreateMessageResult, McpError>;
}

/// Pluggable handler invoked when an MCP server issues `elicitation/create`.
///
/// Install one via [`McpHandlerConfig::with_elicitation_responder`] to drive
/// the host application's user-input UI.
#[async_trait]
pub trait McpElicitationResponder: Send + Sync + 'static {
    /// Returns the user's response to a server-initiated elicitation request.
    async fn create_elicitation(
        &self,
        params: McpCreateElicitationRequestParams,
    ) -> Result<McpCreateElicitationResult, McpError>;
}

/// Pluggable handler invoked when an MCP server issues `roots/list`.
///
/// Install one via [`McpHandlerConfig::with_roots_provider`] to surface
/// workspace roots that scope the server's filesystem access.
#[async_trait]
pub trait McpRootsProvider: Send + Sync + 'static {
    /// Returns the roots the server should consider in scope.
    async fn list_roots(&self) -> Result<Vec<McpRoot>, McpError>;
}

/// Default broadcast capacity for [`McpServerEvent`] subscribers.
const DEFAULT_EVENTS_CAPACITY: usize = 128;

/// Channels paired with an [`McpClientHandler`] returned by
/// [`McpHandlerConfig::build`].
///
/// `notifications` is the legacy mpsc receiver consumed by the catalog refresh
/// path inside [`McpServerManager::refresh_changed_catalogs`]. `events` is the
/// broadcast sender that surfaces every [`McpServerEvent`] — clone it once and
/// pass it into [`McpConnection::from_running_service_with_events`] when
/// adopting an externally constructed [`rmcp::service::RunningService`]. If the
/// adopted connection also needs adapter-time hooks from the same
/// [`McpHandlerConfig`], use
/// [`McpConnection::from_running_service_with_events_and_handler_config`].
pub struct McpClientChannels {
    /// Legacy mpsc receiver for catalog list-changed announcements.
    pub notifications: mpsc::UnboundedReceiver<McpServerNotification>,
    /// Broadcast sender that forwards every [`McpServerEvent`] to subscribers.
    pub events: broadcast::Sender<McpServerEvent>,
}

/// rmcp [`ClientHandler`] used by [`McpConnection`].
///
/// You only need to construct this directly if you're wiring rmcp transports
/// that [`McpTransportBinding`] does not cover (in-memory pipes, websockets,
/// custom IO). Build one via [`McpHandlerConfig::build`], then pair the
/// resulting service with [`McpConnection::from_running_service`],
/// [`McpConnection::from_running_service_with_events`], or
/// [`McpConnection::from_running_service_with_events_and_handler_config`] when
/// the connection must preserve adapter-time hooks from the config.
#[derive(Clone)]
pub struct McpClientHandler {
    info: rmcp_model::ClientInfo,
    notifications: mpsc::UnboundedSender<McpServerNotification>,
    events: broadcast::Sender<McpServerEvent>,
    sampling: Option<Arc<dyn McpSamplingResponder>>,
    elicitation: Option<Arc<dyn McpElicitationResponder>>,
    roots: Option<Arc<dyn McpRootsProvider>>,
}

impl ClientHandler for McpClientHandler {
    fn create_message(
        &self,
        params: rmcp_model::CreateMessageRequestParams,
        _context: rmcp::service::RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<rmcp_model::CreateMessageResult, rmcp_model::ErrorData>>
    + rmcp::service::MaybeSendFuture
    + '_ {
        let responder = self.sampling.clone();
        async move {
            match responder {
                Some(responder) => responder.create_message(params).await.map_err(Into::into),
                None => Err(rmcp_model::ErrorData::method_not_found::<
                    rmcp_model::CreateMessageRequestMethod,
                >()),
            }
        }
    }

    fn list_roots(
        &self,
        _context: rmcp::service::RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<rmcp_model::ListRootsResult, rmcp_model::ErrorData>>
    + rmcp::service::MaybeSendFuture
    + '_ {
        let provider = self.roots.clone();
        async move {
            match provider {
                Some(provider) => provider
                    .list_roots()
                    .await
                    .map(McpListRootsResult::new)
                    .map_err(Into::into),
                None => Ok(McpListRootsResult::default()),
            }
        }
    }

    fn create_elicitation(
        &self,
        params: rmcp_model::CreateElicitationRequestParams,
        _context: rmcp::service::RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<rmcp_model::CreateElicitationResult, rmcp_model::ErrorData>>
    + rmcp::service::MaybeSendFuture
    + '_ {
        let responder = self.elicitation.clone();
        async move {
            match responder {
                Some(responder) => responder
                    .create_elicitation(params)
                    .await
                    .map_err(Into::into),
                None => Ok(McpCreateElicitationResult::new(
                    McpElicitationAction::Decline,
                )),
            }
        }
    }

    fn on_progress(
        &self,
        params: rmcp_model::ProgressNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
        let _ = self.events.send(McpServerEvent::Progress(params));
        std::future::ready(())
    }

    fn on_logging_message(
        &self,
        params: rmcp_model::LoggingMessageNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
        let _ = self.events.send(McpServerEvent::Logging(params));
        std::future::ready(())
    }

    fn on_resource_updated(
        &self,
        params: rmcp_model::ResourceUpdatedNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
        let _ = self.events.send(McpServerEvent::ResourceUpdated(params));
        std::future::ready(())
    }

    fn on_cancelled(
        &self,
        params: rmcp_model::CancelledNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
        let _ = self.events.send(McpServerEvent::Cancelled(params));
        std::future::ready(())
    }

    fn on_tool_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
        let _ = self.notifications.send(McpServerNotification::ToolsChanged);
        let _ = self.events.send(McpServerEvent::ToolListChanged);
        std::future::ready(())
    }

    fn on_resource_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
        let _ = self
            .notifications
            .send(McpServerNotification::ResourcesChanged);
        let _ = self.events.send(McpServerEvent::ResourceListChanged);
        std::future::ready(())
    }

    fn on_prompt_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
        let _ = self
            .notifications
            .send(McpServerNotification::PromptsChanged);
        let _ = self.events.send(McpServerEvent::PromptListChanged);
        std::future::ready(())
    }

    fn get_info(&self) -> rmcp_model::ClientInfo {
        self.info.clone()
    }
}

impl From<McpError> for rmcp_model::ErrorData {
    fn from(error: McpError) -> Self {
        rmcp_model::ErrorData::internal_error(error.to_string(), None)
    }
}

type RmcpClientService = RunningService<RoleClient, McpClientHandler>;

/// Configuration applied to every [`McpClientHandler`] this crate builds on
/// behalf of a connection or [`McpServerManager`].
///
/// Holds the optional sampling / elicitation / roots responders plus the
/// broadcast capacity for [`McpServerEvent`] subscribers. Pass an instance to
/// [`McpConnection::connect_with_handler`] to drive a single connection, or
/// install one on the manager via
/// [`McpServerManager::with_handler_config`] / per-trait builders.
#[derive(Clone, Default)]
pub struct McpHandlerConfig {
    /// Responder for server-initiated `sampling/createMessage` requests.
    pub sampling: Option<Arc<dyn McpSamplingResponder>>,
    /// Responder for server-initiated `elicitation/create` requests.
    pub elicitation: Option<Arc<dyn McpElicitationResponder>>,
    /// Provider for `roots/list`.
    pub roots: Option<Arc<dyn McpRootsProvider>>,
    /// Resolver for auth challenges raised during MCP operations. When
    /// installed, [`McpToolAdapter::invoke`] (and other operation paths)
    /// invoke the responder inline on auth challenges and retry — auth
    /// never surfaces as a loop interrupt.
    pub auth: Option<Arc<dyn McpAuthResponder>>,
    /// Handler invoked when an MCP server returns a JSON-RPC error for an
    /// invoked tool. When installed, [`McpToolAdapter::invoke`] forwards
    /// the typed [`McpInvocationError`] to the responder before falling
    /// back to [`ToolError::ExecutionFailed`]; the responder may synthesize
    /// a [`CallToolResult`] (the agent sees a successful tool call) or
    /// pass the error through unchanged.
    pub error_responder: Option<Arc<dyn McpErrorResponder>>,
    /// Broadcast capacity for the [`McpServerEvent`] channel. Defaults to
    /// `DEFAULT_EVENTS_CAPACITY` when `None`.
    pub events_capacity: Option<usize>,
}

impl McpHandlerConfig {
    /// Returns an empty handler config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the sampling responder.
    pub fn with_sampling_responder(mut self, responder: Arc<dyn McpSamplingResponder>) -> Self {
        self.sampling = Some(responder);
        self
    }

    /// Sets the elicitation responder.
    pub fn with_elicitation_responder(
        mut self,
        responder: Arc<dyn McpElicitationResponder>,
    ) -> Self {
        self.elicitation = Some(responder);
        self
    }

    /// Sets the roots provider.
    pub fn with_roots_provider(mut self, provider: Arc<dyn McpRootsProvider>) -> Self {
        self.roots = Some(provider);
        self
    }

    /// Sets the auth responder.
    pub fn with_auth_responder(mut self, responder: Arc<dyn McpAuthResponder>) -> Self {
        self.auth = Some(responder);
        self
    }

    /// Sets the invocation-error responder.
    pub fn with_error_responder(mut self, responder: Arc<dyn McpErrorResponder>) -> Self {
        self.error_responder = Some(responder);
        self
    }

    /// Sets the broadcast capacity for [`McpServerEvent`] subscribers.
    pub fn with_events_capacity(mut self, capacity: usize) -> Self {
        self.events_capacity = Some(capacity);
        self
    }

    /// Builds a handler together with a fresh [`McpClientChannels`] pair —
    /// the notification receiver and a new broadcast sender for
    /// [`McpServerEvent`].
    pub fn build(&self) -> (McpClientHandler, McpClientChannels) {
        self.build_inner(None)
    }

    /// Builds a handler that publishes [`McpServerEvent`] into the provided
    /// broadcast sender. Use this when adopting an externally constructed
    /// rmcp service via [`McpConnection::from_running_service_with_events`]
    /// so subscribers see the same stream.
    pub fn build_with(
        &self,
        events: broadcast::Sender<McpServerEvent>,
    ) -> (McpClientHandler, McpClientChannels) {
        self.build_inner(Some(events))
    }

    fn build_inner(
        &self,
        events: Option<broadcast::Sender<McpServerEvent>>,
    ) -> (McpClientHandler, McpClientChannels) {
        let (notifications_tx, notifications_rx) = mpsc::unbounded_channel();
        let events_tx = events.unwrap_or_else(|| {
            let capacity = self.events_capacity.unwrap_or(DEFAULT_EVENTS_CAPACITY);
            let (tx, _) = broadcast::channel(capacity);
            tx
        });

        let mut capabilities = rmcp_model::ClientCapabilities::default();
        if self.sampling.is_some() {
            capabilities.sampling = Some(McpSamplingCapability::default());
        }
        if self.elicitation.is_some() {
            capabilities.elicitation = Some(McpElicitationCapability {
                form: Some(McpFormElicitationCapability::default()),
                url: None,
            });
        }
        if self.roots.is_some() {
            capabilities.roots = Some(McpRootsCapabilities::default());
        }

        let handler = McpClientHandler {
            info: rmcp_model::ClientInfo::new(
                capabilities,
                rmcp_model::Implementation::new("agentkit-mcp", env!("CARGO_PKG_VERSION"))
                    .with_title("agentkit MCP client"),
            )
            .with_protocol_version(rmcp_model::ProtocolVersion::LATEST),
            notifications: notifications_tx,
            events: events_tx.clone(),
            sampling: self.sampling.clone(),
            elicitation: self.elicitation.clone(),
            roots: self.roots.clone(),
        };

        (
            handler,
            McpClientChannels {
                notifications: notifications_rx,
                events: events_tx,
            },
        )
    }
}

/// A live connection to a single MCP server, wrapping an
/// [`rmcp::service::RunningService`].
pub struct McpConnection {
    server_id: McpServerId,
    config: Option<McpServerConfig>,
    inner: Mutex<RmcpClientService>,
    peer: RwLock<Peer<RoleClient>>,
    auth: Mutex<Option<MetadataMap>>,
    notifications: Mutex<mpsc::UnboundedReceiver<McpServerNotification>>,
    events: broadcast::Sender<McpServerEvent>,
    handler_config: McpHandlerConfig,
    capabilities: McpServerCapabilities,
}

/// The result of replaying an MCP operation after auth resolution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum McpOperationResult {
    /// The server was successfully (re)connected; contains the discovery snapshot.
    Connected(McpDiscoverySnapshot),
    /// A tool call completed; contains the typed rmcp [`CallToolResult`].
    Tool(CallToolResult),
    /// A resource was read successfully.
    Resource(ReadResourceResult),
    /// A prompt was retrieved successfully.
    Prompt(GetPromptResult),
}

impl McpConnection {
    /// Connects to an MCP server, performs the rmcp `initialize` handshake,
    /// and returns a ready-to-use connection. No sampling / elicitation /
    /// roots responders are wired; use [`Self::connect_with_handler`] when
    /// the server may issue those requests.
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        Self::connect_with_auth(config, None, McpHandlerConfig::default()).await
    }

    /// Connects to an MCP server with a fully configured [`McpHandlerConfig`].
    pub async fn connect_with_handler(
        config: &McpServerConfig,
        handler_config: McpHandlerConfig,
    ) -> Result<Self, McpError> {
        Self::connect_with_auth(config, None, handler_config).await
    }

    async fn connect_with_auth(
        config: &McpServerConfig,
        auth: Option<&MetadataMap>,
        handler_config: McpHandlerConfig,
    ) -> Result<Self, McpError> {
        let (handler, channels) = handler_config.build();
        let McpClientChannels {
            notifications: notification_rx,
            events: events_tx,
        } = channels;
        let (service, capabilities) = match &config.transport {
            McpTransportBinding::Stdio(binding) => {
                connect_rmcp_stdio(config, binding, handler).await?
            }
            McpTransportBinding::StreamableHttp(binding) => {
                connect_rmcp_streamable_http(config, binding, auth, handler).await?
            }
        };

        let peer = service.peer().clone();
        Ok(Self {
            server_id: config.id.clone(),
            config: Some(config.clone()),
            inner: Mutex::new(service),
            peer: RwLock::new(peer),
            auth: Mutex::new(auth.cloned()),
            notifications: Mutex::new(notification_rx),
            events: events_tx,
            handler_config,
            capabilities,
        })
    }

    /// Adopts an externally constructed [`rmcp::service::RunningService`] as
    /// an [`McpConnection`].
    ///
    /// Use this when you need a transport rmcp supports but
    /// [`McpTransportBinding`] does not (in-memory pipes for tests, websockets,
    /// custom IO). Pair the service with the notification receiver returned by
    /// [`McpHandlerConfig::build`] so list-change notifications stay
    /// observable.
    ///
    /// The connection has no [`McpServerConfig`] attached, so reconnect-on-auth
    /// is unavailable; [`resolve_auth`](Self::resolve_auth) only updates stored
    /// credentials in this mode. Server-pushed events from the underlying
    /// handler are *not* forwarded to subscribers — use
    /// [`Self::from_running_service_with_events`] paired with the broadcast
    /// sender from [`McpClientChannels`] when you need event delivery.
    pub fn from_running_service(
        server_id: impl Into<McpServerId>,
        service: RmcpClientService,
        notifications: mpsc::UnboundedReceiver<McpServerNotification>,
    ) -> Self {
        let (events_tx, _) = broadcast::channel(DEFAULT_EVENTS_CAPACITY);
        Self::from_running_service_with_events(server_id, service, notifications, events_tx)
    }

    /// Variant of [`Self::from_running_service`] that wires the broadcast
    /// sender returned by [`McpHandlerConfig::build`] (or [`build_with`])
    /// so [`Self::subscribe_events`] receivers observe the same stream the
    /// handler is publishing into.
    ///
    /// [`build_with`]: McpHandlerConfig::build_with
    pub fn from_running_service_with_events(
        server_id: impl Into<McpServerId>,
        service: RmcpClientService,
        notifications: mpsc::UnboundedReceiver<McpServerNotification>,
        events: broadcast::Sender<McpServerEvent>,
    ) -> Self {
        Self::from_running_service_with_events_and_handler_config(
            server_id,
            service,
            notifications,
            events,
            McpHandlerConfig::default(),
        )
    }

    /// Variant of [`Self::from_running_service_with_events`] that also
    /// preserves the handler config used to build the adopted service.
    ///
    /// Use this when the connection needs to reach client-side hooks that are
    /// not carried by the rmcp handler itself, such as [`McpAuthResponder`] and
    /// [`McpErrorResponder`] during [`McpToolAdapter`] invocation.
    pub fn from_running_service_with_events_and_handler_config(
        server_id: impl Into<McpServerId>,
        service: RmcpClientService,
        notifications: mpsc::UnboundedReceiver<McpServerNotification>,
        events: broadcast::Sender<McpServerEvent>,
        handler_config: McpHandlerConfig,
    ) -> Self {
        let capabilities = service
            .peer_info()
            .map(|info| rmcp_server_capabilities_to_agentkit(&info.capabilities))
            .unwrap_or_default();
        let peer = service.peer().clone();
        Self {
            server_id: server_id.into(),
            config: None,
            inner: Mutex::new(service),
            peer: RwLock::new(peer),
            auth: Mutex::new(None),
            notifications: Mutex::new(notifications),
            events,
            handler_config,
            capabilities,
        }
    }

    async fn reconnect_inner(&self, auth: Option<&MetadataMap>) -> Result<(), McpError> {
        let Some(config) = self.config.clone() else {
            return Ok(());
        };
        let (handler, channels) = self.handler_config.build_with(self.events.clone());
        let McpClientChannels {
            notifications: notification_rx,
            ..
        } = channels;
        let (service, _capabilities) = match &config.transport {
            McpTransportBinding::Stdio(binding) => {
                connect_rmcp_stdio(&config, binding, handler).await?
            }
            McpTransportBinding::StreamableHttp(binding) => {
                connect_rmcp_streamable_http(&config, binding, auth, handler).await?
            }
        };
        let new_peer = service.peer().clone();
        *self.notifications.lock().await = notification_rx;
        *self.inner.lock().await = service;
        *self.peer.write().expect("MCP peer lock poisoned") = new_peer;
        Ok(())
    }

    fn peer(&self) -> Peer<RoleClient> {
        self.peer.read().expect("MCP peer lock poisoned").clone()
    }

    /// Returns the [`McpServerId`] for this connection.
    pub fn server_id(&self) -> &McpServerId {
        &self.server_id
    }

    /// Returns the capabilities advertised by the server during `initialize`.
    pub fn capabilities(&self) -> &McpServerCapabilities {
        &self.capabilities
    }

    /// Returns the [`McpHandlerConfig`] this connection was built with.
    /// Used by [`McpToolAdapter`] to reach the registered
    /// [`McpAuthResponder`] when an auth challenge surfaces.
    pub fn handler_config(&self) -> &McpHandlerConfig {
        &self.handler_config
    }

    /// Subscribes to the per-connection [`McpServerEvent`] broadcast.
    ///
    /// Receivers buffer up to `events_capacity` (configured via
    /// [`McpHandlerConfig::with_events_capacity`], defaults to
    /// `DEFAULT_EVENTS_CAPACITY`) before slow consumers are signalled with
    /// [`broadcast::error::RecvError::Lagged`]. Catalog `*ListChanged` events
    /// are also delivered through the legacy [`McpServerNotification`]
    /// receiver consumed by [`McpServerManager::refresh_changed_catalogs`].
    pub fn subscribe_events(&self) -> broadcast::Receiver<McpServerEvent> {
        self.events.subscribe()
    }

    /// Subscribes to `notifications/resources/updated` for the given URI.
    ///
    /// Updates surface as [`McpServerEvent::ResourceUpdated`] on every
    /// receiver returned by [`Self::subscribe_events`].
    pub async fn subscribe_resource(&self, uri: impl Into<String>) -> Result<(), McpError> {
        let uri = uri.into();
        self.peer()
            .subscribe(rmcp_model::SubscribeRequestParams::new(uri.clone()))
            .await
            .map_err(|error| {
                rmcp_operation_error(
                    &self.server_id,
                    McpMethod::ResourcesSubscribe { uri },
                    error,
                )
            })
    }

    /// Cancels a previous [`Self::subscribe_resource`] subscription.
    pub async fn unsubscribe_resource(&self, uri: impl Into<String>) -> Result<(), McpError> {
        let uri = uri.into();
        self.peer()
            .unsubscribe(rmcp_model::UnsubscribeRequestParams::new(uri.clone()))
            .await
            .map_err(|error| {
                rmcp_operation_error(
                    &self.server_id,
                    McpMethod::ResourcesUnsubscribe { uri },
                    error,
                )
            })
    }

    /// Negotiates the minimum severity the server should emit through
    /// `notifications/message`. Surfaced as [`McpServerEvent::Logging`].
    pub async fn set_logging_level(&self, level: McpLoggingLevel) -> Result<(), McpError> {
        self.peer()
            .set_level(rmcp_model::SetLevelRequestParams::new(level))
            .await
            .map_err(|error| {
                rmcp_operation_error(
                    &self.server_id,
                    McpMethod::LoggingSetLevel {
                        level: format!("{level:?}"),
                    },
                    error,
                )
            })
    }

    /// Sends a `notifications/cancelled` to the server, asking it to stop
    /// processing a previously issued request.
    pub async fn notify_cancelled(
        &self,
        params: McpCancelledNotificationParam,
    ) -> Result<(), McpError> {
        self.peer()
            .notify_cancelled(params)
            .await
            .map_err(rmcp_service_error)
    }

    /// Notifies the server that the client's roots list has changed; servers
    /// may respond by re-issuing `roots/list`.
    pub async fn notify_roots_list_changed(&self) -> Result<(), McpError> {
        self.peer()
            .notify_roots_list_changed()
            .await
            .map_err(rmcp_service_error)
    }

    /// Gracefully closes the underlying rmcp service.
    ///
    /// For Streamable HTTP this drives the rmcp transport to issue a `DELETE`
    /// against the negotiated session, releasing server-side state.
    pub async fn close(&self) -> Result<(), McpError> {
        let mut inner = self.inner.lock().await;
        inner
            .close()
            .await
            .map(|_| ())
            .map_err(|error| McpError::Transport(format!("rmcp service close failed: {error}")))
    }

    /// Stores or clears authentication credentials and, when configured to do
    /// so via [`McpServerConfig`], reconnects to apply them.
    pub async fn resolve_auth(&self, resolution: AuthResolution) -> Result<(), McpError> {
        let mut auth_slot = self.auth.lock().await;
        match resolution {
            AuthResolution::Provided { credentials, .. } => {
                *auth_slot = Some(credentials);
            }
            AuthResolution::Cancelled { .. } => {
                *auth_slot = None;
            }
        }
        let snapshot = auth_slot.clone();
        drop(auth_slot);
        // Only reconnect if we have a config to reconnect with. Without one
        // (e.g. constructed via [`from_running_service`]) the auth is stored
        // but not pushed to the live transport.
        if self.config.is_some() {
            self.reconnect_inner(snapshot.as_ref()).await?;
        }
        Ok(())
    }

    /// Discovers tools, resources, and prompts that the server advertised.
    pub async fn discover(&self) -> Result<McpDiscoverySnapshot, McpError> {
        let tools = async {
            match self.capabilities.tools {
                Some(_) => self.list_tools().await,
                None => Ok(Vec::new()),
            }
        };
        let resources = async {
            match self.capabilities.resources {
                Some(_) => self.list_resources().await,
                None => Ok(Vec::new()),
            }
        };
        let prompts = async {
            match self.capabilities.prompts {
                Some(_) => self.list_prompts().await,
                None => Ok(Vec::new()),
            }
        };
        let (tools, resources, prompts) = tokio::try_join!(tools, resources, prompts)?;
        Ok(McpDiscoverySnapshot {
            server_id: self.server_id.clone(),
            tools,
            resources,
            prompts,
            metadata: MetadataMap::new(),
        })
    }

    async fn drain_notifications(&self) -> Vec<McpServerNotification> {
        let mut notifications = self.notifications.lock().await;
        let mut drained = Vec::new();
        while let Ok(notification) = notifications.try_recv() {
            drained.push(notification);
        }
        drained
    }

    /// Lists all tools advertised by the connected MCP server.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        self.peer()
            .list_all_tools()
            .await
            .map_err(rmcp_service_error)
    }

    /// Lists all resources advertised by the connected MCP server.
    pub async fn list_resources(&self) -> Result<Vec<McpResource>, McpError> {
        self.peer()
            .list_all_resources()
            .await
            .map_err(rmcp_service_error)
    }

    /// Lists all prompts advertised by the connected MCP server.
    pub async fn list_prompts(&self) -> Result<Vec<McpPrompt>, McpError> {
        self.peer()
            .list_all_prompts()
            .await
            .map_err(rmcp_service_error)
    }

    /// Invokes a tool on the MCP server.
    ///
    /// Returns the typed [`CallToolResult`] — the [`Vec<Content>`] block list,
    /// the optional `structured_content` field, and the `is_error` flag are
    /// all preserved. Adapters convert this into agentkit
    /// [`ToolOutput`]/[`InvocableOutput`] at the boundary.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpError> {
        let arguments_for_auth = arguments.clone();
        let mut params = rmcp_model::CallToolRequestParams::new(name.to_string());
        if !arguments.is_null() {
            params =
                params.with_arguments(value_to_json_object(arguments, "tools/call arguments")?);
        }
        let name_owned = name.to_string();

        // Wire-call span so tool latency on `agent.execute_tool` can be
        // broken down into MCP round-trip vs dispatch overhead. Parents
        // under the loop's execute_tool span via the tracing hierarchy.
        let span = tracing::info_span!(
            "mcp.call_tool",
            "otel.name" = %format!("mcp.call_tool {name}"),
            "mcp.server.id" = %self.server_id,
            "mcp.tool.name" = %name,
            "error.type" = tracing::field::Empty,
        );
        use tracing::Instrument;
        let result = self.peer().call_tool(params).instrument(span.clone()).await;
        match result {
            Ok(result) => {
                if result.is_error == Some(true) {
                    span.record("error.type", "tool_error");
                }
                Ok(result)
            }
            Err(error) => {
                span.record("error.type", "mcp_error");
                Err(rmcp_operation_error(
                    &self.server_id,
                    McpMethod::ToolsCall {
                        name: name_owned,
                        arguments: arguments_for_auth,
                    },
                    error,
                ))
            }
        }
    }

    /// Reads a resource from the MCP server by URI.
    ///
    /// Returns the typed [`ReadResourceResult`] — the full
    /// [`Vec<McpResourceContents>`] is preserved (text vs blob, mime types,
    /// metadata). Use [`McpResourceHandle`] for the agentkit
    /// [`ResourceProvider`] view that collapses to a single inline `DataRef`.
    pub async fn read_resource(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        let uri_owned = uri.to_string();
        self.peer()
            .read_resource(rmcp_model::ReadResourceRequestParams::new(uri))
            .await
            .map_err(|error| {
                rmcp_operation_error(
                    &self.server_id,
                    McpMethod::ResourcesRead { uri: uri_owned },
                    error,
                )
            })
    }

    /// Retrieves a prompt from the MCP server, rendering it with the given
    /// arguments.
    ///
    /// Returns the typed [`GetPromptResult`] — message role and content
    /// blocks (text/image/audio/embedded resource) are preserved. Use
    /// [`McpPromptHandle`] for the collapsed agentkit [`PromptProvider`]
    /// view.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<GetPromptResult, McpError> {
        let arguments_for_auth = arguments.clone();
        let name_owned = name.to_string();
        let mut params = rmcp_model::GetPromptRequestParams::new(name);
        if !arguments.is_null() {
            params =
                params.with_arguments(value_to_json_object(arguments, "prompts/get arguments")?);
        }
        self.peer().get_prompt(params).await.map_err(|error| {
            rmcp_operation_error(
                &self.server_id,
                McpMethod::PromptsGet {
                    name: name_owned,
                    arguments: arguments_for_auth,
                },
                error,
            )
        })
    }
}

async fn connect_rmcp_stdio(
    config: &McpServerConfig,
    binding: &StdioTransportConfig,
    handler: McpClientHandler,
) -> Result<(RmcpClientService, McpServerCapabilities), McpError> {
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(&binding.command).configure(|command| {
            command.args(&binding.args);
            if let Some(cwd) = &binding.cwd {
                command.current_dir(cwd);
            }
            for (key, value) in &binding.env {
                command.env(key, value);
            }
        }),
    )
    .map_err(McpError::Io)?;

    let service = handler
        .serve(transport)
        .await
        .map_err(|error| rmcp_initialize_error(config, error))?;
    let capabilities = service
        .peer_info()
        .map(|info| rmcp_server_capabilities_to_agentkit(&info.capabilities))
        .unwrap_or_default();

    Ok((service, capabilities))
}

async fn connect_rmcp_streamable_http(
    config: &McpServerConfig,
    binding: &StreamableHttpTransportConfig,
    auth: Option<&MetadataMap>,
    handler: McpClientHandler,
) -> Result<(RmcpClientService, McpServerCapabilities), McpError> {
    let auth_header = auth
        .and_then(bearer_token_from_metadata)
        .or_else(|| binding.bearer_token.clone());
    let mut rmcp_config = RmcpStreamableHttpClientTransportConfig::with_uri(binding.url.clone());
    if let Some(auth_header) = auth_header {
        rmcp_config = rmcp_config.auth_header(auth_header);
    }
    rmcp_config = rmcp_config.custom_headers(binding.headers.iter().cloned().collect());

    let result = match binding.http_client.as_ref() {
        Some(client) => {
            let transport = StreamableHttpClientTransport::with_client(
                DynHttpClient(client.clone()),
                rmcp_config,
            );
            handler.serve(transport).await
        }
        None => {
            let transport = StreamableHttpClientTransport::from_config(rmcp_config);
            handler.serve(transport).await
        }
    };
    let service = result.map_err(|error| rmcp_initialize_error(config, error))?;
    let capabilities = service
        .peer_info()
        .map(|info| rmcp_server_capabilities_to_agentkit(&info.capabilities))
        .unwrap_or_default();

    Ok((service, capabilities))
}

/// Adapter exposing a single MCP resource as a [`ResourceProvider`].
pub struct McpResourceHandle {
    connection: Arc<McpConnection>,
    descriptor: ResourceDescriptor,
}

#[async_trait]
impl ResourceProvider for McpResourceHandle {
    async fn list_resources(&self) -> Result<Vec<ResourceDescriptor>, CapabilityError> {
        Ok(vec![self.descriptor.clone()])
    }

    async fn read_resource(
        &self,
        id: &ResourceId,
        _ctx: &mut CapabilityContext<'_>,
    ) -> Result<ResourceContents, CapabilityError> {
        let result = self
            .connection
            .read_resource(&id.0)
            .await
            .map_err(|error| match error {
                McpError::AuthRequired(request) => {
                    CapabilityError::Unavailable(format!("auth required: {:?}", request))
                }
                other => CapabilityError::ExecutionFailed(other.to_string()),
            })?;
        read_resource_result_to_capabilities(result)
            .map_err(|error| CapabilityError::ExecutionFailed(error.to_string()))
    }
}

/// Adapter exposing a single MCP prompt as a [`PromptProvider`].
pub struct McpPromptHandle {
    connection: Arc<McpConnection>,
    descriptor: PromptDescriptor,
}

#[async_trait]
impl PromptProvider for McpPromptHandle {
    async fn list_prompts(&self) -> Result<Vec<PromptDescriptor>, CapabilityError> {
        Ok(vec![self.descriptor.clone()])
    }

    async fn get_prompt(
        &self,
        id: &PromptId,
        args: Value,
        _ctx: &mut CapabilityContext<'_>,
    ) -> Result<PromptContents, CapabilityError> {
        let result =
            self.connection
                .get_prompt(&id.0, args)
                .await
                .map_err(|error| match error {
                    McpError::AuthRequired(request) => {
                        CapabilityError::Unavailable(format!("auth required: {:?}", request))
                    }
                    other => CapabilityError::ExecutionFailed(other.to_string()),
                })?;
        Ok(get_prompt_result_to_capabilities(result))
    }
}

/// A [`CapabilityProvider`] that surfaces MCP tools, resources, and prompts.
///
/// The tool side is built by wrapping [`McpToolAdapter`]s in
/// [`agentkit_tools_core::ToolInvocableAdapter`], so the same
/// permission-check + adapter-spec plumbing the rest of agentkit uses also
/// applies to MCP tools — this crate no longer ships its own
/// `McpInvocable`.
pub struct McpCapabilityProvider {
    invocables: Vec<Arc<dyn Invocable>>,
    resources: Vec<Arc<dyn ResourceProvider>>,
    prompts: Vec<Arc<dyn PromptProvider>>,
}

impl McpCapabilityProvider {
    /// Builds a capability provider from an existing connection and snapshot,
    /// using the [`McpToolNamespace::Default`] tool naming strategy.
    pub fn from_snapshot(connection: Arc<McpConnection>, snapshot: &McpDiscoverySnapshot) -> Self {
        Self::from_snapshot_with_namespace(connection, snapshot, &McpToolNamespace::Default)
    }

    /// Builds a capability provider with a custom tool naming strategy.
    pub fn from_snapshot_with_namespace(
        connection: Arc<McpConnection>,
        snapshot: &McpDiscoverySnapshot,
        namespace: &McpToolNamespace,
    ) -> Self {
        let server_id = connection.server_id().clone();
        let registry =
            snapshot
                .tools
                .iter()
                .cloned()
                .fold(ToolRegistry::new(), |registry, tool| {
                    registry.with(McpToolAdapter::with_namespace(
                        &server_id,
                        connection.clone(),
                        tool,
                        namespace,
                    ))
                });
        let permissions: Arc<dyn PermissionChecker> = Arc::new(AllowAllPermissions);
        let resources_arc: Arc<dyn agentkit_tools_core::ToolResources> = Arc::new(());
        let invocables =
            ToolCapabilityProvider::from_registry(&registry, permissions, resources_arc)
                .invocables();

        let resources = snapshot
            .resources
            .iter()
            .cloned()
            .map(|resource| {
                Arc::new(McpResourceHandle {
                    connection: connection.clone(),
                    descriptor: resource_descriptor_from_rmcp(resource),
                }) as Arc<dyn ResourceProvider>
            })
            .collect();

        let prompts = snapshot
            .prompts
            .iter()
            .cloned()
            .map(|prompt| {
                Arc::new(McpPromptHandle {
                    connection: connection.clone(),
                    descriptor: prompt_descriptor_from_rmcp(prompt),
                }) as Arc<dyn PromptProvider>
            })
            .collect();

        Self {
            invocables,
            resources,
            prompts,
        }
    }

    /// Merges multiple capability providers into one.
    pub fn merge<I>(providers: I) -> Self
    where
        I: IntoIterator<Item = Self>,
    {
        let mut invocables = Vec::new();
        let mut resources = Vec::new();
        let mut prompts = Vec::new();

        for provider in providers {
            invocables.extend(provider.invocables);
            resources.extend(provider.resources);
            prompts.extend(provider.prompts);
        }

        Self {
            invocables,
            resources,
            prompts,
        }
    }

    /// Connects to an MCP server, performs discovery, and builds a provider.
    pub async fn connect(
        config: &McpServerConfig,
    ) -> Result<(Arc<McpConnection>, Self, McpDiscoverySnapshot), McpError> {
        let connection = Arc::new(McpConnection::connect(config).await?);
        let snapshot = connection.discover().await?;
        let provider = Self::from_snapshot(connection.clone(), &snapshot);

        Ok((connection, provider, snapshot))
    }
}

impl CapabilityProvider for McpCapabilityProvider {
    fn invocables(&self) -> Vec<Arc<dyn Invocable>> {
        self.invocables.clone()
    }

    fn resources(&self) -> Vec<Arc<dyn ResourceProvider>> {
        self.resources.clone()
    }

    fn prompts(&self) -> Vec<Arc<dyn PromptProvider>> {
        self.prompts.clone()
    }
}

/// A connected MCP server together with its configuration and snapshot.
#[derive(Clone)]
pub struct McpServerHandle {
    config: McpServerConfig,
    connection: Arc<McpConnection>,
    snapshot: McpDiscoverySnapshot,
    namespace: McpToolNamespace,
}

impl McpServerHandle {
    /// Returns the original configuration used to connect this server.
    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    /// Returns the server's unique identifier.
    pub fn server_id(&self) -> &McpServerId {
        self.connection.server_id()
    }

    /// Returns a shared reference to the underlying [`McpConnection`].
    pub fn connection(&self) -> Arc<McpConnection> {
        self.connection.clone()
    }

    /// Returns the discovery snapshot captured when the server was connected.
    pub fn snapshot(&self) -> &McpDiscoverySnapshot {
        &self.snapshot
    }

    /// Returns the tool naming strategy in effect for this server.
    pub fn namespace(&self) -> &McpToolNamespace {
        &self.namespace
    }

    /// Returns the agentkit-namespaced name of every tool this server
    /// advertised at discovery time, without building adapters or cloning
    /// schemas. Cheaper than [`Self::tool_registry`] when only the names
    /// are needed.
    pub fn tool_names(&self) -> Vec<ToolName> {
        self.snapshot
            .tools
            .iter()
            .map(|tool| ToolName::new(self.namespace.apply(self.server_id(), &tool.name)))
            .collect()
    }

    /// Builds a [`ToolRegistry`] containing an [`McpToolAdapter`] for each tool.
    pub fn tool_registry(&self) -> ToolRegistry {
        self.snapshot
            .tools
            .iter()
            .cloned()
            .fold(ToolRegistry::new(), |registry, tool| {
                registry.with(McpToolAdapter::with_namespace(
                    self.server_id(),
                    self.connection.clone(),
                    tool,
                    &self.namespace,
                ))
            })
    }

    /// Builds an [`McpCapabilityProvider`] from this server's snapshot.
    pub fn capability_provider(&self) -> McpCapabilityProvider {
        McpCapabilityProvider::from_snapshot_with_namespace(
            self.connection.clone(),
            &self.snapshot,
            &self.namespace,
        )
    }
}

/// Connection failure for one server returned by
/// [`McpServerManager::connect_all_settled`].
#[derive(Debug)]
pub struct McpServerConnectionError {
    /// The registered server that failed to connect or complete discovery.
    pub server_id: McpServerId,
    /// The underlying connection or discovery error for this server.
    pub error: McpError,
}

/// Best-effort outcome returned by [`McpServerManager::connect_all_settled`].
///
/// Successful connections are installed into the manager and its tool catalog.
/// Failed entries leave any existing connection for that server untouched.
#[must_use = "inspect `failed` before ignoring the settled MCP connection result"]
pub struct McpConnectAllSettled {
    /// Handles for servers that connected and completed discovery.
    pub connected: Vec<McpServerHandle>,
    /// Per-server failures for servers that did not connect or discover.
    pub failed: Vec<McpServerConnectionError>,
}

impl McpConnectAllSettled {
    /// Returns `true` when every registered server connected successfully.
    pub fn all_connected(&self) -> bool {
        self.failed.is_empty()
    }

    /// Returns `true` when at least one server failed to connect.
    pub fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }

    /// Borrows the successful connection handles.
    pub fn connected(&self) -> &[McpServerHandle] {
        &self.connected
    }

    /// Borrows the per-server connection failures.
    pub fn failed(&self) -> &[McpServerConnectionError] {
        &self.failed
    }

    /// Consumes the result into successful handles and failures.
    pub fn into_parts(self) -> (Vec<McpServerHandle>, Vec<McpServerConnectionError>) {
        (self.connected, self.failed)
    }
}

impl fmt::Debug for McpConnectAllSettled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let connected = self
            .connected
            .iter()
            .map(|handle| handle.server_id())
            .collect::<Vec<_>>();
        f.debug_struct("McpConnectAllSettled")
            .field("connected", &connected)
            .field("failed", &self.failed)
            .finish()
    }
}

/// Per-server lifecycle options used by [`McpServerManager`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpServerOptions {
    /// Maximum time allowed to establish a server connection — transport
    /// setup and the MCP initialize handshake — and complete initial
    /// discovery (`tools/list`, `resources/list`, and `prompts/list`).
    /// Refresh discovery is bounded by the same duration.
    pub connect_timeout: Option<Duration>,
}

impl McpServerOptions {
    /// Creates default server options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the connect timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }
}

/// Manages the lifecycle of one or more MCP servers.
pub struct McpServerManager {
    configs: BTreeMap<McpServerId, McpServerConfig>,
    options: BTreeMap<McpServerId, McpServerOptions>,
    connections: BTreeMap<McpServerId, McpServerHandle>,
    auth: BTreeMap<McpServerId, MetadataMap>,
    catalog_tx: broadcast::Sender<McpCatalogEvent>,
    namespace: McpToolNamespace,
    handler_config: McpHandlerConfig,
    catalog_writer: CatalogWriter,
    /// Agentkit-namespaced tool names this manager has registered for each
    /// connected server. Used to perform surgical writes against the
    /// [`CatalogWriter`] on connect/disconnect/refresh without rebuilding
    /// the whole catalog.
    server_tools: BTreeMap<McpServerId, BTreeSet<ToolName>>,
}

impl Default for McpServerManager {
    fn default() -> Self {
        let (catalog_tx, _) = broadcast::channel(128);
        let (catalog_writer, _) = dynamic_catalog("mcp");
        Self {
            configs: BTreeMap::new(),
            options: BTreeMap::new(),
            connections: BTreeMap::new(),
            auth: BTreeMap::new(),
            catalog_tx,
            namespace: McpToolNamespace::Default,
            handler_config: McpHandlerConfig::default(),
            catalog_writer,
            server_tools: BTreeMap::new(),
        }
    }
}

impl McpServerManager {
    /// Creates an empty server manager with no registered servers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the tool naming strategy for every adapter built by this manager.
    pub fn with_namespace(mut self, namespace: McpToolNamespace) -> Self {
        self.namespace = namespace;
        self
    }

    /// Replaces the tool naming strategy in place.
    pub fn set_namespace(&mut self, namespace: McpToolNamespace) -> &mut Self {
        self.namespace = namespace;
        self
    }

    /// Returns the active tool naming strategy.
    pub fn namespace(&self) -> &McpToolNamespace {
        &self.namespace
    }

    /// Replaces the [`McpHandlerConfig`] applied to every connection this
    /// manager opens.
    pub fn with_handler_config(mut self, handler_config: McpHandlerConfig) -> Self {
        self.handler_config = handler_config;
        self
    }

    /// Sets the [`McpHandlerConfig`] in place.
    pub fn set_handler_config(&mut self, handler_config: McpHandlerConfig) -> &mut Self {
        self.handler_config = handler_config;
        self
    }

    /// Returns the active [`McpHandlerConfig`].
    pub fn handler_config(&self) -> &McpHandlerConfig {
        &self.handler_config
    }

    /// Registers a server configuration. Returns `self` for chaining.
    pub fn with_server(mut self, config: McpServerConfig) -> Self {
        self.register_server(config);
        self
    }

    /// Registers a server configuration with lifecycle options. Returns
    /// `self` for chaining.
    pub fn with_server_options(
        mut self,
        config: McpServerConfig,
        options: McpServerOptions,
    ) -> Self {
        self.register_server_with_options(config, options);
        self
    }

    /// Registers a server configuration by mutable reference.
    pub fn register_server(&mut self, config: McpServerConfig) -> &mut Self {
        let id = config.id.clone();
        self.configs.insert(id.clone(), config);
        self.options.entry(id).or_default();
        self
    }

    /// Registers a server configuration and lifecycle options by mutable
    /// reference.
    pub fn register_server_with_options(
        &mut self,
        config: McpServerConfig,
        options: McpServerOptions,
    ) -> &mut Self {
        let id = config.id.clone();
        self.configs.insert(id.clone(), config);
        self.options.insert(id, options);
        self
    }

    /// Returns the handle for a connected server, or `None` if not connected.
    pub fn connected_server(&self, server_id: &McpServerId) -> Option<&McpServerHandle> {
        self.connections.get(server_id)
    }

    /// Returns handles for all currently connected servers.
    pub fn connected_servers(&self) -> Vec<&McpServerHandle> {
        self.connections.values().collect()
    }

    /// Subscribes to MCP catalog and lifecycle events.
    pub fn subscribe_catalog_events(&self) -> broadcast::Receiver<McpCatalogEvent> {
        self.catalog_tx.subscribe()
    }

    fn emit_catalog_event(&self, event: McpCatalogEvent) {
        let _ = self.catalog_tx.send(event);
    }

    async fn discover_with_options(
        connection: &McpConnection,
        options: &McpServerOptions,
    ) -> Result<McpDiscoverySnapshot, McpError> {
        match options.connect_timeout {
            Some(timeout) => tokio::time::timeout(timeout, connection.discover())
                .await
                .map_err(|_| McpError::Timeout {
                    operation: "discover",
                    duration: timeout,
                })?,
            None => connection.discover().await,
        }
    }

    async fn connect_and_discover(
        config: &McpServerConfig,
        auth: Option<&MetadataMap>,
        handler_config: McpHandlerConfig,
        options: &McpServerOptions,
    ) -> Result<(Arc<McpConnection>, McpDiscoverySnapshot), McpError> {
        let connect = async {
            let connection =
                Arc::new(McpConnection::connect_with_auth(config, auth, handler_config).await?);
            let snapshot = connection.discover().await?;
            Ok((connection, snapshot))
        };
        match options.connect_timeout {
            Some(timeout) => {
                tokio::time::timeout(timeout, connect)
                    .await
                    .map_err(|_| McpError::Timeout {
                        operation: "connect",
                        duration: timeout,
                    })?
            }
            None => connect.await,
        }
    }

    /// Connects a single registered server by its identifier.
    pub async fn connect_server(
        &mut self,
        server_id: &McpServerId,
    ) -> Result<McpServerHandle, McpError> {
        let config = self
            .configs
            .get(server_id)
            .cloned()
            .ok_or_else(|| McpError::UnknownServer(server_id.to_string()))?;
        let options = self.options.get(server_id).cloned().unwrap_or_default();
        let (connection, snapshot) = Self::connect_and_discover(
            &config,
            self.auth.get(server_id),
            self.handler_config.clone(),
            &options,
        )
        .await?;
        let handle = McpServerHandle {
            config,
            connection,
            snapshot,
            namespace: self.namespace.clone(),
        };
        self.install_connection(server_id.clone(), handle.clone())
            .await;
        self.register_server_tools(server_id, &handle.snapshot);
        self.emit_catalog_event(McpCatalogEvent::ServerConnected {
            server_id: server_id.clone(),
        });
        Ok(handle)
    }

    /// Installs a freshly connected handle, gracefully closing any previous
    /// connection it replaces so transports (and, for Streamable HTTP,
    /// server-side sessions) are not leaked. Close failures on the replaced
    /// connection are ignored — it is being discarded either way.
    async fn install_connection(&mut self, server_id: McpServerId, handle: McpServerHandle) {
        if let Some(previous) = self.connections.insert(server_id, handle) {
            let _ = previous.connection.close().await;
        }
    }

    /// Connects all registered servers concurrently.
    pub async fn connect_all(&mut self) -> Result<Vec<McpServerHandle>, McpError> {
        let plans: Vec<(
            McpServerId,
            McpServerConfig,
            McpServerOptions,
            Option<MetadataMap>,
        )> = self
            .configs
            .iter()
            .map(|(id, cfg)| {
                (
                    id.clone(),
                    cfg.clone(),
                    self.options.get(id).cloned().unwrap_or_default(),
                    self.auth.get(id).cloned(),
                )
            })
            .collect();
        let handler_config = self.handler_config.clone();
        let namespace = self.namespace.clone();

        let futures = plans.into_iter().map(|(server_id, config, options, auth)| {
            let handler_config = handler_config.clone();
            let namespace = namespace.clone();
            async move {
                let (connection, snapshot) =
                    Self::connect_and_discover(&config, auth.as_ref(), handler_config, &options)
                        .await?;
                Ok::<(McpServerId, McpServerHandle), McpError>((
                    server_id,
                    McpServerHandle {
                        config,
                        connection,
                        snapshot,
                        namespace,
                    },
                ))
            }
        });

        let results = try_join_all(futures).await?;
        let mut handles = Vec::with_capacity(results.len());
        let mut connected: Vec<(McpServerId, McpDiscoverySnapshot)> =
            Vec::with_capacity(results.len());
        for (server_id, handle) in results {
            connected.push((server_id.clone(), handle.snapshot.clone()));
            self.install_connection(server_id, handle.clone()).await;
            handles.push(handle);
        }
        for (server_id, snapshot) in &connected {
            self.register_server_tools(server_id, snapshot);
        }
        for (server_id, _) in connected {
            self.emit_catalog_event(McpCatalogEvent::ServerConnected { server_id });
        }
        Ok(handles)
    }

    /// Connects all registered servers concurrently and waits for every
    /// connection attempt to settle.
    ///
    /// Unlike [`Self::connect_all`], this method does not fail fast. Every
    /// unconnected server is attempted in parallel; successful connections
    /// are installed into the manager and tool catalog, while each failed
    /// connection is returned with its [`McpServerId`] and [`McpError`].
    ///
    /// Already-connected servers are not re-handshaken: their existing
    /// handles are left untouched and returned as connected.
    pub async fn connect_all_settled(&mut self) -> McpConnectAllSettled {
        let server_ids: Vec<McpServerId> = self.configs.keys().cloned().collect();
        self.connect_servers_settled(server_ids).await
    }

    /// Connects the given registered servers concurrently and waits for
    /// every connection attempt to settle.
    ///
    /// The settled semantics match [`Self::connect_all_settled`], scoped to
    /// `server_ids`. The call is idempotent per server:
    ///
    /// - already-connected servers are not re-handshaken; their existing
    ///   handles are returned as connected,
    /// - unregistered identifiers settle as failures with
    ///   [`McpError::UnknownServer`],
    /// - duplicate identifiers are attempted once.
    ///
    /// This is the building block for lazy, on-demand connection strategies:
    /// connect exactly the servers a turn needs, in parallel, without
    /// touching (or resurrecting) anything else the manager knows about.
    pub async fn connect_servers_settled<I, T>(&mut self, server_ids: I) -> McpConnectAllSettled
    where
        I: IntoIterator<Item = T>,
        T: Into<McpServerId>,
    {
        let mut connected = Vec::new();
        let mut failures = Vec::new();
        let mut seen = BTreeSet::new();
        let mut plans: Vec<(
            McpServerId,
            McpServerConfig,
            McpServerOptions,
            Option<MetadataMap>,
        )> = Vec::new();

        for server_id in server_ids {
            let server_id: McpServerId = server_id.into();
            if !seen.insert(server_id.clone()) {
                continue;
            }
            if let Some(existing) = self.connections.get(&server_id) {
                connected.push(existing.clone());
                continue;
            }
            let Some(config) = self.configs.get(&server_id).cloned() else {
                failures.push(McpServerConnectionError {
                    error: McpError::UnknownServer(server_id.to_string()),
                    server_id,
                });
                continue;
            };
            plans.push((
                server_id.clone(),
                config,
                self.options.get(&server_id).cloned().unwrap_or_default(),
                self.auth.get(&server_id).cloned(),
            ));
        }

        let handler_config = self.handler_config.clone();
        let namespace = self.namespace.clone();

        let futures = plans.into_iter().map(|(server_id, config, options, auth)| {
            let handler_config = handler_config.clone();
            let namespace = namespace.clone();
            async move {
                let result = async {
                    let (connection, snapshot) = Self::connect_and_discover(
                        &config,
                        auth.as_ref(),
                        handler_config,
                        &options,
                    )
                    .await?;
                    Ok::<McpServerHandle, McpError>(McpServerHandle {
                        config,
                        connection,
                        snapshot,
                        namespace,
                    })
                }
                .await;
                (server_id, result)
            }
        });

        let results = join_all(futures).await;
        let mut connected_snapshots = Vec::new();

        for (server_id, result) in results {
            match result {
                Ok(handle) => {
                    connected_snapshots.push((server_id.clone(), handle.snapshot.clone()));
                    self.install_connection(server_id, handle.clone()).await;
                    connected.push(handle);
                }
                Err(error) => {
                    failures.push(McpServerConnectionError { server_id, error });
                }
            }
        }

        for (server_id, snapshot) in &connected_snapshots {
            self.register_server_tools(server_id, snapshot);
        }
        for (server_id, _) in connected_snapshots {
            self.emit_catalog_event(McpCatalogEvent::ServerConnected { server_id });
        }

        McpConnectAllSettled {
            connected,
            failed: failures,
        }
    }

    /// Re-discovers capabilities for a connected server.
    pub async fn refresh_server(
        &mut self,
        server_id: &McpServerId,
    ) -> Result<McpDiscoverySnapshot, McpError> {
        let handle = self
            .connections
            .get_mut(server_id)
            .ok_or_else(|| McpError::UnknownServer(server_id.to_string()))?;
        let options = self.options.get(server_id).cloned().unwrap_or_default();
        let previous = handle.snapshot.clone();
        let snapshot = match Self::discover_with_options(&handle.connection, &options).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.emit_catalog_event(McpCatalogEvent::RefreshFailed {
                    server_id: server_id.clone(),
                    message: error.to_string(),
                });
                return Err(error);
            }
        };
        handle.snapshot = snapshot.clone();
        let events = diff_discovery_snapshots(server_id, &previous, &snapshot);
        if !events.is_empty() {
            self.apply_catalog_events(server_id, &snapshot, &events);
            for event in events {
                self.emit_catalog_event(event);
            }
        }
        Ok(snapshot)
    }

    /// Processes pending server list-change notifications.
    pub async fn refresh_changed_catalogs(&mut self) -> Result<Vec<McpCatalogEvent>, McpError> {
        let server_ids = self.connections.keys().cloned().collect::<Vec<_>>();
        let mut emitted = Vec::new();

        for server_id in server_ids {
            let Some(connection) = self
                .connections
                .get(&server_id)
                .map(McpServerHandle::connection)
            else {
                continue;
            };
            let notifications = connection.drain_notifications().await;
            if notifications.is_empty() {
                continue;
            }

            let handle = self
                .connections
                .get_mut(&server_id)
                .ok_or_else(|| McpError::UnknownServer(server_id.to_string()))?;
            let options = self.options.get(&server_id).cloned().unwrap_or_default();
            let previous = handle.snapshot.clone();
            let snapshot = match Self::discover_with_options(&handle.connection, &options).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let event = McpCatalogEvent::RefreshFailed {
                        server_id: server_id.clone(),
                        message: error.to_string(),
                    };
                    self.emit_catalog_event(event.clone());
                    emitted.push(event);
                    return Err(error);
                }
            };
            handle.snapshot = snapshot.clone();
            let events = diff_discovery_snapshots(&server_id, &previous, &snapshot);
            if !events.is_empty() {
                self.apply_catalog_events(&server_id, &snapshot, &events);
                for event in events {
                    self.emit_catalog_event(event.clone());
                    emitted.push(event);
                }
            }
        }

        Ok(emitted)
    }

    /// Disconnects a server and removes it from active connections.
    ///
    /// The server's tools are always removed from the catalog and
    /// [`McpCatalogEvent::ServerDisconnected`] is always emitted, even when
    /// closing the underlying connection fails — the returned error is
    /// advisory. After this method returns the manager no longer tracks the
    /// connection, and the server (whose config stays registered) can be
    /// reconnected via [`Self::connect_server`].
    pub async fn disconnect_server(&mut self, server_id: &McpServerId) -> Result<(), McpError> {
        let Some(handle) = self.connections.remove(server_id) else {
            return Err(McpError::UnknownServer(server_id.to_string()));
        };
        let close_result = handle.connection.close().await;
        self.unregister_server_tools(server_id);
        self.emit_catalog_event(McpCatalogEvent::ServerDisconnected {
            server_id: server_id.clone(),
        });
        close_result
    }

    /// Removes a server's registration entirely: its config, options, and
    /// stored credentials. If the server is currently connected it is
    /// disconnected first (tools unregistered,
    /// [`McpCatalogEvent::ServerDisconnected`] emitted), and any close error
    /// is returned as advisory after the removal has completed.
    ///
    /// Unlike [`Self::disconnect_server`], the server cannot be reconnected
    /// afterwards without registering it again, and bulk operations such as
    /// [`Self::connect_all_settled`] no longer attempt it.
    pub async fn unregister_server(&mut self, server_id: &McpServerId) -> Result<(), McpError> {
        let was_registered = self.configs.remove(server_id).is_some();
        self.options.remove(server_id);
        self.auth.remove(server_id);
        match self.disconnect_server(server_id).await {
            // Registered but not connected: nothing to close.
            Err(McpError::UnknownServer(_)) if was_registered => Ok(()),
            result => result,
        }
    }

    /// Stores or clears authentication credentials for a server.
    pub async fn resolve_auth(&mut self, resolution: AuthResolution) -> Result<(), McpError> {
        let server_id = resolution
            .request()
            .server_id()
            .ok_or_else(|| McpError::AuthResolution("auth resolution missing server id".into()))?;
        let server_id = McpServerId::new(server_id);
        match &resolution {
            AuthResolution::Provided { credentials, .. } => {
                self.auth.insert(server_id.clone(), credentials.clone());
            }
            AuthResolution::Cancelled { .. } => {
                self.auth.remove(&server_id);
            }
        }

        if let Some(handle) = self.connections.get(&server_id) {
            handle.connection.resolve_auth(resolution).await?;
        } else if !self.configs.contains_key(&server_id) {
            return Err(McpError::UnknownServer(server_id.to_string()));
        }
        self.emit_catalog_event(McpCatalogEvent::AuthChanged { server_id });
        Ok(())
    }

    /// Builds a one-shot snapshot [`ToolRegistry`] of every tool across all
    /// connected servers. Use [`source`](Self::source) instead when wiring
    /// the manager into an [`agentkit_loop::Agent`] so tool catalog changes
    /// flow through automatically.
    pub fn tool_registry(&self) -> ToolRegistry {
        self.connections
            .values()
            .fold(ToolRegistry::new(), |mut registry, handle| {
                for tool in handle.snapshot.tools.iter().cloned() {
                    registry.register(McpToolAdapter::with_namespace(
                        handle.server_id(),
                        handle.connection.clone(),
                        tool,
                        &self.namespace,
                    ));
                }
                registry
            })
    }

    /// Returns the manager's federated [`CatalogReader`].
    ///
    /// The manager keeps an internal `CatalogWriter` in sync with every
    /// connect, disconnect, and catalog refresh; the returned reader sees
    /// the added/removed/changed tool sets via
    /// [`ToolSource::drain_catalog_events`]. Pass it to
    /// [`agentkit_loop::AgentBuilder::tools`] alongside any frozen native
    /// [`ToolRegistry`].
    ///
    /// Each call returns a fresh reader subscription — events emitted before
    /// this call are not replayed. Call once at agent setup time and reuse.
    pub fn source(&self) -> CatalogReader {
        self.catalog_writer.reader()
    }

    /// Surgically updates the tool catalog from the diff events produced
    /// by [`diff_discovery_snapshots`]. Only [`McpCatalogEvent::ToolsChanged`]
    /// affects the catalog — resource and prompt diffs are observed by the
    /// caller via the broadcast stream and don't touch tool state.
    fn apply_catalog_events(
        &mut self,
        server_id: &McpServerId,
        snapshot: &McpDiscoverySnapshot,
        events: &[McpCatalogEvent],
    ) {
        for event in events {
            if let McpCatalogEvent::ToolsChanged {
                added,
                removed,
                changed,
                ..
            } = event
            {
                self.apply_server_tool_diff(server_id, snapshot, added, removed, changed);
            }
        }
    }

    /// Registers every tool from a freshly-discovered snapshot, recording
    /// the agentkit-namespaced names so [`Self::unregister_server_tools`]
    /// can later remove exactly this set.
    fn register_server_tools(&mut self, server_id: &McpServerId, snapshot: &McpDiscoverySnapshot) {
        let connection = match self.connections.get(server_id) {
            Some(handle) => handle.connection.clone(),
            None => return,
        };
        let previous = self.server_tools.remove(server_id).unwrap_or_default();
        let mut names = BTreeSet::new();
        for tool in &snapshot.tools {
            let adapter = McpToolAdapter::with_namespace(
                server_id,
                connection.clone(),
                tool.clone(),
                &self.namespace,
            );
            names.insert(adapter.spec().name.clone());
            self.catalog_writer.upsert(Arc::new(adapter));
        }
        for stale in previous.difference(&names) {
            self.catalog_writer.remove(stale);
        }
        self.server_tools.insert(server_id.clone(), names);
    }

    /// Removes every agentkit-namespaced tool previously registered for
    /// `server_id`. No-op if the server was never registered.
    fn unregister_server_tools(&mut self, server_id: &McpServerId) {
        let Some(names) = self.server_tools.remove(server_id) else {
            return;
        };
        for name in names {
            self.catalog_writer.remove(&name);
        }
    }

    /// Applies a per-tool diff (in raw MCP names) against the current
    /// catalog: removes are pruned, adds and changes are upserted from the
    /// fresh snapshot. Updates the per-server name index accordingly.
    fn apply_server_tool_diff(
        &mut self,
        server_id: &McpServerId,
        snapshot: &McpDiscoverySnapshot,
        added: &[String],
        removed: &[String],
        changed: &[String],
    ) {
        let connection = match self.connections.get(server_id) {
            Some(handle) => handle.connection.clone(),
            None => return,
        };
        let names = self.server_tools.entry(server_id.clone()).or_default();

        for raw_name in removed {
            let agentkit_name = ToolName::new(self.namespace.apply(server_id, raw_name));
            if names.remove(&agentkit_name) {
                self.catalog_writer.remove(&agentkit_name);
            }
        }

        let upsert_one = |raw_name: &str| -> Option<(ToolName, McpToolAdapter)> {
            let tool = snapshot
                .tools
                .iter()
                .find(|tool| tool.name.as_ref() == raw_name)?
                .clone();
            let adapter = McpToolAdapter::with_namespace(
                server_id,
                connection.clone(),
                tool,
                &self.namespace,
            );
            Some((adapter.spec().name.clone(), adapter))
        };

        for raw_name in added.iter().chain(changed.iter()) {
            if let Some((agentkit_name, adapter)) = upsert_one(raw_name) {
                names.insert(agentkit_name);
                self.catalog_writer.upsert(Arc::new(adapter));
            }
        }
    }

    /// Builds a combined [`McpCapabilityProvider`] from all connected servers.
    pub fn capability_provider(&self) -> McpCapabilityProvider {
        McpCapabilityProvider::merge(
            self.connections
                .values()
                .map(McpServerHandle::capability_provider),
        )
    }
}

fn diff_discovery_snapshots(
    server_id: &McpServerId,
    previous: &McpDiscoverySnapshot,
    current: &McpDiscoverySnapshot,
) -> Vec<McpCatalogEvent> {
    let mut events = Vec::new();
    let (added, removed, changed) = diff_named_items(
        previous.tools.iter().map(|item| (item.name.as_ref(), item)),
        current.tools.iter().map(|item| (item.name.as_ref(), item)),
    );
    if !added.is_empty() || !removed.is_empty() || !changed.is_empty() {
        events.push(McpCatalogEvent::ToolsChanged {
            server_id: server_id.clone(),
            added,
            removed,
            changed,
        });
    }

    let (added, removed, changed) = diff_named_items(
        previous
            .resources
            .iter()
            .map(|item| (item.uri.as_str(), item)),
        current
            .resources
            .iter()
            .map(|item| (item.uri.as_str(), item)),
    );
    if !added.is_empty() || !removed.is_empty() || !changed.is_empty() {
        events.push(McpCatalogEvent::ResourcesChanged {
            server_id: server_id.clone(),
            added,
            removed,
            changed,
        });
    }

    let (added, removed, changed) = diff_named_items(
        previous
            .prompts
            .iter()
            .map(|item| (item.name.as_str(), item)),
        current
            .prompts
            .iter()
            .map(|item| (item.name.as_str(), item)),
    );
    if !added.is_empty() || !removed.is_empty() || !changed.is_empty() {
        events.push(McpCatalogEvent::PromptsChanged {
            server_id: server_id.clone(),
            added,
            removed,
            changed,
        });
    }

    events
}

/// Merge-walks two name-keyed sequences and produces added/removed/changed
/// name lists. Each side is sorted in place; no intermediate maps are
/// allocated. Names are cloned only at output time.
fn diff_named_items<'a, T>(
    previous: impl IntoIterator<Item = (&'a str, &'a T)>,
    current: impl IntoIterator<Item = (&'a str, &'a T)>,
) -> (Vec<String>, Vec<String>, Vec<String>)
where
    T: PartialEq + 'a,
{
    let mut prev: Vec<(&str, &T)> = previous.into_iter().collect();
    let mut curr: Vec<(&str, &T)> = current.into_iter().collect();
    prev.sort_unstable_by_key(|(name, _)| *name);
    curr.sort_unstable_by_key(|(name, _)| *name);

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < prev.len() && j < curr.len() {
        match prev[i].0.cmp(curr[j].0) {
            std::cmp::Ordering::Less => {
                removed.push(prev[i].0.to_string());
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                added.push(curr[j].0.to_string());
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                if prev[i].1 != curr[j].1 {
                    changed.push(curr[j].0.to_string());
                }
                i += 1;
                j += 1;
            }
        }
    }
    while i < prev.len() {
        removed.push(prev[i].0.to_string());
        i += 1;
    }
    while j < curr.len() {
        added.push(curr[j].0.to_string());
        j += 1;
    }

    (added, removed, changed)
}

/// Adapter exposing an MCP tool as an agentkit [`Tool`].
pub struct McpToolAdapter {
    tool_name: String,
    connection: Arc<McpConnection>,
    spec: ToolSpec,
}

impl McpToolAdapter {
    /// Creates a new tool adapter for the given MCP tool, using the
    /// [`McpToolNamespace::Default`] naming strategy.
    pub fn new(server_id: &McpServerId, connection: Arc<McpConnection>, tool: McpTool) -> Self {
        Self::with_namespace(server_id, connection, tool, &McpToolNamespace::Default)
    }

    /// Creates a new tool adapter with a custom name-namespacing strategy.
    pub fn with_namespace(
        server_id: &McpServerId,
        connection: Arc<McpConnection>,
        tool: McpTool,
        namespace: &McpToolNamespace,
    ) -> Self {
        let spec = tool_spec_from_tool(server_id, &tool, namespace);
        Self {
            tool_name: tool.name.into_owned(),
            connection,
            spec,
        }
    }

    async fn handle_invocation_error(
        &self,
        err: McpInvocationError,
        input: &Value,
    ) -> Result<CallToolResult, ToolError> {
        let Some(responder) = self.connection.handler_config().error_responder.clone() else {
            return Err(tool_error_from_invocation(err));
        };
        let method = McpMethod::ToolsCall {
            name: self.tool_name.clone(),
            arguments: input.clone(),
        };
        let ctx = McpErrorContext {
            server_id: self.connection.server_id(),
            method: &method,
            input: Some(input),
        };
        match responder.handle(&err, ctx).await {
            ErrorResponderOutcome::SynthesizeResult(result) => Ok(result),
            ErrorResponderOutcome::PassThrough => Err(tool_error_from_invocation(err)),
        }
    }
}

/// Maps a JSON-RPC invocation error that no [`McpErrorResponder`]
/// synthesized into the [`ToolError`] vocabulary. Request-side faults —
/// invalid params, invalid request, parse errors — surface as
/// [`ToolError::InvalidInput`] so the model is told to fix its arguments
/// rather than that the tool broke; server-side and unrecognized codes stay
/// [`ToolError::ExecutionFailed`].
fn tool_error_from_invocation(error: McpInvocationError) -> ToolError {
    match &error {
        McpInvocationError::InvalidParams { .. }
        | McpInvocationError::InvalidRequest { .. }
        | McpInvocationError::ParseError { .. } => ToolError::InvalidInput(error.to_string()),
        _ => ToolError::ExecutionFailed(error.to_string()),
    }
}

/// Maps a non-invocation [`McpError`] onto the [`ToolError`] vocabulary.
///
/// I/O, transport, and timeout failures mean the connection itself is
/// unhealthy rather than that the call was invalid, so they surface as
/// [`ToolError::Unavailable`] — callers can match on the variant to trigger
/// reconnects without string-inspection. JSON-RPC application errors and
/// everything else remain [`ToolError::ExecutionFailed`].
fn tool_error_from_mcp(error: McpError) -> ToolError {
    match &error {
        McpError::Io(_) | McpError::Transport(_) | McpError::Timeout { .. } => {
            ToolError::Unavailable(error.to_string())
        }
        _ => ToolError::ExecutionFailed(error.to_string()),
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input = request.input;
        let result = match self
            .connection
            .call_tool(&self.tool_name, input.clone())
            .await
        {
            Ok(result) => result,
            Err(McpError::AuthRequired(auth_request)) => {
                let responder = self
                    .connection
                    .handler_config()
                    .auth
                    .clone()
                    .ok_or_else(|| {
                        ToolError::ExecutionFailed(
                            "MCP server requires auth but no McpAuthResponder is registered".into(),
                        )
                    })?;
                let resolution = responder.resolve(*auth_request).await.map_err(|error| {
                    ToolError::ExecutionFailed(format!("auth responder failed: {error}"))
                })?;
                match &resolution {
                    AuthResolution::Provided { .. } => {
                        self.connection
                            .resolve_auth(resolution.clone())
                            .await
                            .map_err(|error| {
                                ToolError::ExecutionFailed(format!(
                                    "applying auth resolution failed: {error}"
                                ))
                            })?;
                    }
                    AuthResolution::Cancelled { .. } => {
                        return Err(ToolError::ExecutionFailed(
                            "user cancelled MCP auth flow".into(),
                        ));
                    }
                }
                match self
                    .connection
                    .call_tool(&self.tool_name, input.clone())
                    .await
                {
                    Ok(result) => result,
                    Err(McpError::AuthRequired(req)) => {
                        return Err(ToolError::ExecutionFailed(format!(
                            "MCP auth challenge unresolved after retry: {}",
                            req.id
                        )));
                    }
                    Err(McpError::Invocation(err)) => {
                        self.handle_invocation_error(err, &input).await?
                    }
                    Err(other) => return Err(tool_error_from_mcp(other)),
                }
            }
            Err(McpError::Invocation(err)) => self.handle_invocation_error(err, &input).await?,
            Err(other) => return Err(tool_error_from_mcp(other)),
        };

        let is_error = result.is_error.unwrap_or(false);
        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: call_tool_result_to_tool_output(result),
                is_error,
                metadata: MetadataMap::new(),
            },
            duration: None,
            metadata: MetadataMap::new(),
        })
    }
}

fn rmcp_server_capabilities_to_agentkit(
    capabilities: &rmcp_model::ServerCapabilities,
) -> McpServerCapabilities {
    McpServerCapabilities {
        tools: capabilities.tools.as_ref().map(|tools| ToolsCapability {
            list_changed: tools.list_changed,
        }),
        resources: capabilities
            .resources
            .as_ref()
            .map(|resources| ResourcesCapability {
                subscribe: resources.subscribe,
                list_changed: resources.list_changed,
            }),
        prompts: capabilities
            .prompts
            .as_ref()
            .map(|prompts| PromptsCapability {
                list_changed: prompts.list_changed,
            }),
        logging: capabilities.logging.as_ref().map(|_| LoggingCapability {}),
    }
}

fn tool_spec_from_tool(
    server_id: &McpServerId,
    tool: &McpTool,
    namespace: &McpToolNamespace,
) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(namespace.apply(server_id, &tool.name)),
        description: tool
            .description
            .as_ref()
            .map(|d| d.to_string())
            .unwrap_or_else(|| tool.name.to_string()),
        input_schema: Value::Object((*tool.input_schema).clone()),
        output_schema: tool
            .output_schema
            .as_ref()
            .map(|schema| Value::Object((**schema).clone())),
        annotations: tool_annotations_from_rmcp(tool.annotations.as_ref()),
        metadata: MetadataMap::new(),
    }
}

fn tool_annotations_from_rmcp(annotations: Option<&McpToolAnnotations>) -> ToolAnnotations {
    let Some(annotations) = annotations else {
        return ToolAnnotations::default();
    };
    // rmcp expresses each hint as `Option<bool>` (advisory; absent means
    // unspecified). agentkit collapses absent → false. Tools that need to
    // distinguish "absent" from "false" should inspect the underlying
    // `McpTool::annotations` directly via the snapshot. MCP has no
    // `needs_approval` hint, so leave it unset and let the loop's permission
    // policy drive approval.
    ToolAnnotations {
        read_only_hint: annotations.read_only_hint.unwrap_or(false),
        destructive_hint: annotations.destructive_hint.unwrap_or(false),
        idempotent_hint: annotations.idempotent_hint.unwrap_or(false),
        needs_approval_hint: false,
        supports_streaming_hint: false,
    }
}

fn resource_descriptor_from_rmcp(resource: McpResource) -> ResourceDescriptor {
    let raw = resource.raw;
    ResourceDescriptor {
        id: ResourceId::new(raw.uri),
        name: raw.name,
        description: raw.description,
        mime_type: raw.mime_type,
        metadata: MetadataMap::new(),
    }
}

fn prompt_descriptor_from_rmcp(prompt: McpPrompt) -> PromptDescriptor {
    let arguments = prompt.arguments.unwrap_or_default();
    let mut required = Vec::new();
    let properties = arguments
        .into_iter()
        .map(|argument| {
            let mut schema = serde_json::Map::new();
            schema.insert("type".into(), Value::String("string".into()));
            if let Some(description) = argument.description {
                schema.insert("description".into(), Value::String(description));
            }
            if argument.required.unwrap_or(false) {
                required.push(Value::String(argument.name.clone()));
            }
            (argument.name, Value::Object(schema))
        })
        .collect::<serde_json::Map<String, Value>>();
    let mut input_schema = serde_json::Map::new();
    input_schema.insert("type".into(), Value::String("object".into()));
    input_schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        input_schema.insert("required".into(), Value::Array(required));
    }

    PromptDescriptor {
        id: PromptId::new(prompt.name.clone()),
        name: prompt.name,
        description: prompt.description,
        input_schema: Value::Object(input_schema),
        metadata: MetadataMap::new(),
    }
}

fn read_resource_result_to_capabilities(
    result: ReadResourceResult,
) -> Result<ResourceContents, McpError> {
    let content = result
        .contents
        .into_iter()
        .next()
        .ok_or_else(|| McpError::Protocol("resources/read returned no contents".into()))?;
    Ok(resource_contents_to_capabilities(content))
}

fn resource_contents_to_capabilities(content: McpResourceContents) -> ResourceContents {
    let mut metadata = MetadataMap::new();
    let data = match content {
        McpResourceContents::TextResourceContents {
            text, mime_type, ..
        } => {
            if let Some(mime) = mime_type {
                metadata.insert("mime_type".into(), Value::String(mime));
            }
            DataRef::InlineText(text)
        }
        McpResourceContents::BlobResourceContents {
            blob,
            mime_type,
            uri,
            ..
        } => {
            if let Some(mime) = mime_type {
                metadata.insert("mime_type".into(), Value::String(mime));
            }
            metadata.insert("uri".into(), Value::String(uri));
            // rmcp delivers blobs as base64-encoded text on the wire.
            DataRef::InlineText(blob)
        }
    };
    ResourceContents { data, metadata }
}

fn get_prompt_result_to_capabilities(result: GetPromptResult) -> PromptContents {
    let items = result
        .messages
        .into_iter()
        .map(prompt_message_to_item)
        .collect();
    let mut metadata = MetadataMap::new();
    if let Some(description) = result.description {
        metadata.insert("description".into(), Value::String(description));
    }
    PromptContents { items, metadata }
}

fn prompt_message_to_item(message: PromptMessage) -> Item {
    let kind = match message.role {
        PromptMessageRole::Assistant => ItemKind::Assistant,
        PromptMessageRole::User => ItemKind::User,
    };
    Item {
        id: None,
        kind,
        parts: vec![prompt_message_content_to_part(message.content)],
        metadata: MetadataMap::new(),
        usage: None,
        finish_reason: None,
        created_at: None,
    }
}

fn prompt_message_content_to_part(content: PromptMessageContent) -> Part {
    match content {
        PromptMessageContent::Text { text } => Part::Text(TextPart::new(text)),
        PromptMessageContent::Image { image } => Part::Media(MediaPart::new(
            Modality::Image,
            image.mime_type.clone(),
            DataRef::InlineText(image.data.clone()),
        )),
        PromptMessageContent::Resource { resource } => {
            let agentkit_resource = resource_contents_to_capabilities(resource.resource.clone());
            agentkit_part_from_resource(agentkit_resource)
        }
        PromptMessageContent::ResourceLink { link } => Part::Text(TextPart::new(link.uri.clone())),
    }
}

fn agentkit_part_from_resource(resource: ResourceContents) -> Part {
    let mime = resource
        .metadata
        .get("mime_type")
        .and_then(Value::as_str)
        .unwrap_or("text/plain")
        .to_string();
    Part::Media(MediaPart::new(Modality::Binary, mime, resource.data))
}

fn call_tool_result_to_tool_output(result: CallToolResult) -> ToolOutput {
    if let Some(structured) = result.structured_content {
        return ToolOutput::Structured(structured);
    }
    let parts = call_tool_content_to_parts(result.content);
    if parts.iter().all(|part| matches!(part, Part::Text(_))) {
        let text = parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        ToolOutput::Text(text)
    } else {
        ToolOutput::Parts(parts)
    }
}

fn call_tool_content_to_parts(contents: Vec<Content>) -> Vec<Part> {
    contents.into_iter().map(content_to_part).collect()
}

fn content_to_part(content: Content) -> Part {
    match content.raw {
        RawContent::Text(text) => Part::Text(TextPart::new(text.text)),
        RawContent::Image(image) => Part::Media(MediaPart::new(
            Modality::Image,
            image.mime_type,
            DataRef::InlineText(image.data),
        )),
        RawContent::Audio(audio) => Part::Media(MediaPart::new(
            Modality::Audio,
            audio.mime_type,
            DataRef::InlineText(audio.data),
        )),
        RawContent::Resource(embedded) => {
            agentkit_part_from_resource(resource_contents_to_capabilities(embedded.resource))
        }
        RawContent::ResourceLink(link) => Part::Text(TextPart::new(link.uri)),
    }
}

fn value_to_json_object(value: Value, context: &str) -> Result<rmcp_model::JsonObject, McpError> {
    match value {
        Value::Object(object) => Ok(object),
        Value::Null => Ok(serde_json::Map::new()),
        other => Err(McpError::Protocol(format!(
            "{context} must be a JSON object, got {other}"
        ))),
    }
}

fn bearer_token_from_metadata(metadata: &MetadataMap) -> Option<String> {
    ["bearer_token", "access_token", "token", "api_key"]
        .into_iter()
        .find_map(|key| metadata.get(key).and_then(Value::as_str).map(str::to_owned))
}

fn rmcp_initialize_error(config: &McpServerConfig, error: ClientInitializeError) -> McpError {
    if let Some(signal) = match &error {
        ClientInitializeError::TransportError { error: dyn_err, .. } => {
            transport_auth_signal(dyn_err)
        }
        _ => None,
    } {
        return McpError::AuthRequired(Box::new(auth_request_from_signal(
            &config.id,
            McpMethod::Initialize,
            signal,
            &error.to_string(),
        )));
    }
    McpError::Transport(error.to_string())
}

fn rmcp_service_error(error: ServiceError) -> McpError {
    service_error_to_mcp_error(error)
}

fn rmcp_operation_error(
    server_id: &McpServerId,
    method: McpMethod,
    error: ServiceError,
) -> McpError {
    if let Some(signal) = service_auth_signal(&error) {
        return McpError::AuthRequired(Box::new(auth_request_from_signal(
            server_id,
            method,
            signal,
            &error.to_string(),
        )));
    }
    service_error_to_mcp_error(error)
}

fn service_error_to_mcp_error(error: ServiceError) -> McpError {
    match error {
        ServiceError::McpError(data) => {
            McpError::Invocation(McpInvocationError::from_error_data(data))
        }
        other => McpError::Transport(other.to_string()),
    }
}

#[derive(Debug)]
enum AuthSignal {
    Required {
        www_authenticate: Option<String>,
    },
    InsufficientScope {
        www_authenticate: Option<String>,
        required_scope: Option<String>,
    },
}

fn service_auth_signal(error: &ServiceError) -> Option<AuthSignal> {
    match error {
        ServiceError::TransportSend(dyn_err) => transport_auth_signal(dyn_err),
        _ => None,
    }
}

fn transport_auth_signal(error: &DynamicTransportError) -> Option<AuthSignal> {
    let inner = error
        .error
        .downcast_ref::<StreamableHttpError<reqwest::Error>>()?;
    match inner {
        StreamableHttpError::AuthRequired(AuthRequiredError {
            www_authenticate_header,
            ..
        }) => Some(AuthSignal::Required {
            www_authenticate: Some(www_authenticate_header.clone()),
        }),
        StreamableHttpError::InsufficientScope(InsufficientScopeError {
            www_authenticate_header,
            required_scope,
            ..
        }) => Some(AuthSignal::InsufficientScope {
            www_authenticate: Some(www_authenticate_header.clone()),
            required_scope: required_scope.clone(),
        }),
        _ => None,
    }
}

fn auth_request_from_signal(
    server_id: &McpServerId,
    method: McpMethod,
    signal: AuthSignal,
    message: &str,
) -> AuthRequest {
    let method_name = method.method_name();
    let mut challenge = MetadataMap::new();
    challenge.insert("server_id".into(), Value::String(server_id.to_string()));
    challenge.insert("method".into(), Value::String(method_name.into()));
    challenge.insert("message".into(), Value::String(message.into()));
    challenge.insert("flow_kind".into(), Value::String("http_bearer".into()));
    match signal {
        AuthSignal::Required { www_authenticate } => {
            if let Some(header) = www_authenticate {
                challenge.insert("www_authenticate".into(), Value::String(header));
            }
        }
        AuthSignal::InsufficientScope {
            www_authenticate,
            required_scope,
        } => {
            challenge.insert("insufficient_scope".into(), Value::Bool(true));
            if let Some(header) = www_authenticate {
                challenge.insert("www_authenticate".into(), Value::String(header));
            }
            if let Some(scope) = required_scope {
                challenge.insert("required_scope".into(), Value::String(scope));
            }
        }
    }
    AuthRequest {
        id: format!("mcp:{}:{}", server_id, method_name),
        provider: format!("mcp.{}", server_id),
        operation: method.into_auth_operation(server_id),
        challenge,
    }
}

/// Typed dispatch for MCP requests that may surface auth or invocation
/// errors. Each peer call constructs the matching variant;
/// [`auth_request_from_signal`] converts to a public [`AuthOperation`]
/// (typed for the four common cases, [`AuthOperation::McpOther`] for the
/// long tail). The same value is also exposed to [`McpErrorResponder`]
/// implementations via [`McpErrorContext::method`].
#[derive(Debug, Clone)]
pub enum McpMethod {
    /// `initialize` — the MCP handshake.
    Initialize,
    /// `tools/call`.
    ToolsCall {
        /// The raw MCP tool name (no agentkit namespacing).
        name: String,
        /// The arguments object as sent to the server.
        arguments: Value,
    },
    /// `resources/read`.
    ResourcesRead {
        /// Resource URI being read.
        uri: String,
    },
    /// `resources/subscribe`.
    ResourcesSubscribe {
        /// Resource URI being subscribed to.
        uri: String,
    },
    /// `resources/unsubscribe`.
    ResourcesUnsubscribe {
        /// Resource URI being unsubscribed from.
        uri: String,
    },
    /// `prompts/get`.
    PromptsGet {
        /// The raw MCP prompt name.
        name: String,
        /// Arguments forwarded to the prompt.
        arguments: Value,
    },
    /// `logging/setLevel`.
    LoggingSetLevel {
        /// Negotiated minimum log severity, formatted for diagnostics.
        level: String,
    },
}

impl McpMethod {
    /// Returns the JSON-RPC method name (e.g. `"tools/call"`).
    pub fn method_name(&self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::ToolsCall { .. } => "tools/call",
            Self::ResourcesRead { .. } => "resources/read",
            Self::ResourcesSubscribe { .. } => "resources/subscribe",
            Self::ResourcesUnsubscribe { .. } => "resources/unsubscribe",
            Self::PromptsGet { .. } => "prompts/get",
            Self::LoggingSetLevel { .. } => "logging/setLevel",
        }
    }

    fn into_auth_operation(self, server_id: &McpServerId) -> AuthOperation {
        let server = server_id.to_string();
        match self {
            Self::Initialize => AuthOperation::McpConnect {
                server_id: server,
                metadata: MetadataMap::new(),
            },
            Self::ToolsCall { name, arguments } => AuthOperation::McpToolCall {
                server_id: server,
                tool_name: name,
                input: arguments,
                metadata: MetadataMap::new(),
            },
            Self::ResourcesRead { uri } => AuthOperation::McpResourceRead {
                server_id: server,
                resource_id: uri,
                metadata: MetadataMap::new(),
            },
            Self::PromptsGet { name, arguments } => AuthOperation::McpPromptGet {
                server_id: server,
                prompt_id: name,
                args: arguments,
                metadata: MetadataMap::new(),
            },
            other @ (Self::ResourcesSubscribe { .. }
            | Self::ResourcesUnsubscribe { .. }
            | Self::LoggingSetLevel { .. }) => {
                let method = other.method_name().to_string();
                AuthOperation::McpOther {
                    server_id: server,
                    method,
                    params: other.into_params_json(),
                    metadata: MetadataMap::new(),
                }
            }
        }
    }

    fn into_params_json(self) -> Value {
        match self {
            Self::Initialize => json!({}),
            Self::ToolsCall { name, arguments } => json!({ "name": name, "arguments": arguments }),
            Self::ResourcesRead { uri } => json!({ "uri": uri }),
            Self::ResourcesSubscribe { uri } => json!({ "uri": uri }),
            Self::ResourcesUnsubscribe { uri } => json!({ "uri": uri }),
            Self::PromptsGet { name, arguments } => {
                json!({ "name": name, "arguments": arguments })
            }
            Self::LoggingSetLevel { level } => json!({ "level": level }),
        }
    }
}

/// Errors produced by MCP transport, protocol, and lifecycle operations.
#[derive(Debug, Error)]
pub enum McpError {
    /// An underlying I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON serialization or deserialization error.
    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
    /// A transport-level error.
    #[error("transport error: {0}")]
    Transport(String),
    /// A manager lifecycle operation exceeded its configured timeout.
    #[error("{operation} timed out after {duration:?}")]
    Timeout {
        /// Operation that timed out.
        operation: &'static str,
        /// Configured timeout duration.
        duration: Duration,
    },
    /// An MCP protocol violation.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The server requires authentication before the operation can proceed.
    #[error("MCP auth required: {0:?}")]
    AuthRequired(Box<AuthRequest>),
    /// An error occurred while resolving or replaying authentication.
    #[error("auth resolution error: {0}")]
    AuthResolution(String),
    /// The MCP server returned a JSON-RPC error for the invoked method.
    #[error("invocation error: {0}")]
    Invocation(McpInvocationError),
    /// The referenced server ID is not registered in the [`McpServerManager`].
    #[error("unknown MCP server: {0}")]
    UnknownServer(String),
}

impl From<&str> for McpServerId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for McpServerId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod error_mapping_tests {
    use super::*;

    #[test]
    fn transport_class_errors_map_to_unavailable() {
        for error in [
            McpError::Transport("connection reset".into()),
            McpError::Timeout {
                operation: "tools/call",
                duration: Duration::from_secs(5),
            },
            McpError::Io(std::io::Error::other("broken pipe")),
        ] {
            assert!(
                matches!(tool_error_from_mcp(error), ToolError::Unavailable(_)),
                "transport-class errors must surface as Unavailable"
            );
        }
    }

    #[test]
    fn non_transport_errors_stay_execution_failed() {
        for error in [
            McpError::Protocol("bad frame".into()),
            McpError::Invocation(McpInvocationError::InternalError {
                message: "server exploded".into(),
                data: None,
            }),
        ] {
            assert!(matches!(
                tool_error_from_mcp(error),
                ToolError::ExecutionFailed(_)
            ));
        }
    }

    #[test]
    fn request_side_invocation_errors_map_to_invalid_input() {
        for error in [
            McpInvocationError::InvalidParams {
                message: "missing field `id`".into(),
                data: None,
            },
            McpInvocationError::InvalidRequest {
                message: "not a request".into(),
                data: None,
            },
            McpInvocationError::ParseError {
                message: "bad json".into(),
                data: None,
            },
        ] {
            assert!(matches!(
                tool_error_from_invocation(error),
                ToolError::InvalidInput(_)
            ));
        }
    }

    #[test]
    fn server_side_invocation_errors_stay_execution_failed() {
        for error in [
            McpInvocationError::InternalError {
                message: "boom".into(),
                data: None,
            },
            McpInvocationError::MethodNotFound {
                message: "tools/call".into(),
                data: None,
            },
            McpInvocationError::Other {
                code: -32099,
                message: "custom".into(),
                data: None,
            },
        ] {
            assert!(matches!(
                tool_error_from_invocation(error),
                ToolError::ExecutionFailed(_)
            ));
        }
    }
}
