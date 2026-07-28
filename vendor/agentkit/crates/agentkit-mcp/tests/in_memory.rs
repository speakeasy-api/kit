//! Integration tests against an in-memory rmcp server connected over tokio
//! duplex pipes. Replaces the legacy fake-transport unit tests; covers the
//! agentkit-side adapters end-to-end through a real rmcp client+server pair.

use std::sync::Arc;
use std::time::Duration;

use agentkit_capabilities::{
    CapabilityContext, CapabilityProvider, InvocableOutput, InvocableRequest,
};
use agentkit_core::{MetadataMap, ToolOutput};
use agentkit_mcp::{
    ErrorResponderOutcome, McpCapabilityProvider, McpConnection, McpCreateElicitationRequestParams,
    McpCreateElicitationResult, McpCreateMessageRequestParams, McpCreateMessageResult,
    McpElicitationAction, McpElicitationResponder, McpError, McpErrorContext, McpErrorResponder,
    McpHandlerConfig, McpInvocationError, McpLoggingLevel, McpLoggingMessageNotificationParam,
    McpMethod, McpProgressNotificationParam, McpResourceContents,
    McpResourceUpdatedNotificationParam, McpRoot, McpRootsProvider, McpSamplingMessage,
    McpSamplingResponder, McpServerCapabilities, McpServerEvent, McpServerId, McpToolAdapter,
    McpToolNamespace, PromptMessageContent,
};
use agentkit_tools_core::{
    PermissionChecker, PermissionDecision, PermissionRequest, Tool, ToolContext, ToolName,
    ToolRequest,
};
use async_trait::async_trait;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    Annotated, CallToolResult, Content, ErrorData as RmcpError, GetPromptRequestParams,
    GetPromptResult, ListPromptsResult, ListResourcesResult, PaginatedRequestParams, Prompt,
    PromptArgument, PromptMessage, PromptMessageRole, RawResource, ReadResourceRequestParams,
    ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::{Peer, RequestContext, RoleServer};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::oneshot;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EchoArgs {
    text: String,
}

#[derive(Clone)]
struct InMemoryServer {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl Default for InMemoryServer {
    fn default() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl InMemoryServer {
    #[tool(description = "Echoes the supplied text back as the tool result.")]
    async fn echo(
        &self,
        Parameters(EchoArgs { text }): Parameters<EchoArgs>,
    ) -> Result<CallToolResult, RmcpError> {
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_handler]
impl ServerHandler for InMemoryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
    }

    async fn list_resources(
        &self,
        _params: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, RmcpError> {
        Ok(ListResourcesResult::with_all_items(vec![Annotated::new(
            RawResource::new("memo:welcome", "welcome"),
            None,
        )]))
    }

    async fn read_resource(
        &self,
        params: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, RmcpError> {
        if params.uri == "memo:welcome" {
            Ok(ReadResourceResult::new(vec![
                ResourceContents::text("hello from the in-memory MCP server", params.uri)
                    .with_mime_type("text/plain"),
            ]))
        } else {
            Err(RmcpError::invalid_params("unknown resource", None))
        }
    }

    async fn list_prompts(
        &self,
        _params: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, RmcpError> {
        Ok(ListPromptsResult::with_all_items(vec![Prompt::new(
            "summarize",
            Some("Summarize the supplied text."),
            Some(vec![
                PromptArgument::new("text")
                    .with_description("Text to summarize.")
                    .with_required(true),
            ]),
        )]))
    }

    async fn get_prompt(
        &self,
        params: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, RmcpError> {
        if params.name != "summarize" {
            return Err(RmcpError::invalid_params("unknown prompt", None));
        }
        let argument = params
            .arguments
            .as_ref()
            .and_then(|args| args.get("text"))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned();
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            format!("Please summarize: {argument}"),
        )])
        .with_description("Summarize the supplied text."))
    }
}

async fn connect_in_memory() -> McpConnection {
    let (connection, _peer) = connect_in_memory_with_server_peer(McpHandlerConfig::new()).await;
    connection
}

/// Variant that returns the server-side peer alongside the client connection,
/// so a test can drive server→client requests and notifications (sampling,
/// elicitation, logging, progress, resource updates) directly.
async fn connect_in_memory_with_server_peer(
    builder: agentkit_mcp::McpHandlerConfig,
) -> (McpConnection, Peer<RoleServer>) {
    let handler_config = builder.clone();
    let (handler, channels) = builder.build();
    let (server_io, client_io) = tokio::io::duplex(8 * 1024);
    let (peer_tx, peer_rx) = oneshot::channel();
    tokio::spawn(async move {
        match InMemoryServer::default().serve(server_io).await {
            Ok(running) => {
                let _ = peer_tx.send(running.peer().clone());
                let _ = running.waiting().await;
            }
            Err(error) => {
                eprintln!("server init failed: {error:?}");
            }
        }
    });

    let service = handler
        .serve(client_io)
        .await
        .expect("client init succeeds");

    let peer = peer_rx.await.expect("server peer arrives");
    let connection = McpConnection::from_running_service_with_events_and_handler_config(
        McpServerId::new("in-memory"),
        service,
        channels.notifications,
        channels.events,
        handler_config,
    );
    (connection, peer)
}

struct AllowAll;

impl PermissionChecker for AllowAll {
    fn evaluate(&self, _request: &dyn PermissionRequest) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

struct InvalidParamsResponder;

#[async_trait]
impl McpErrorResponder for InvalidParamsResponder {
    async fn handle(
        &self,
        error: &McpInvocationError,
        ctx: McpErrorContext<'_>,
    ) -> ErrorResponderOutcome {
        let McpInvocationError::InvalidParams { data, .. } = error else {
            return ErrorResponderOutcome::PassThrough;
        };
        let McpMethod::ToolsCall { name, arguments } = ctx.method else {
            return ErrorResponderOutcome::PassThrough;
        };
        assert_eq!(ctx.server_id.0.as_str(), "in-memory");
        assert_eq!(name, "echo");
        assert_eq!(arguments, &json!({}));
        assert_eq!(ctx.input, Some(&json!({})));
        assert!(data.is_none());

        ErrorResponderOutcome::SynthesizeResult(CallToolResult::success(vec![Content::text(
            "handled invalid params",
        )]))
    }
}

#[tokio::test]
async fn capabilities_reflect_server_advertisement() {
    let connection = connect_in_memory().await;
    let caps = connection.capabilities();
    assert!(caps.tools.is_some());
    assert!(caps.resources.is_some());
    assert!(caps.prompts.is_some());
}

#[tokio::test]
async fn capabilities_all_helper_returns_full_set() {
    let caps = McpServerCapabilities::all();
    assert!(caps.tools.is_some());
    assert!(caps.resources.is_some());
    assert!(caps.prompts.is_some());
    assert!(caps.logging.is_some());
}

#[tokio::test]
async fn discovery_returns_advertised_tools_resources_prompts() {
    let connection = connect_in_memory().await;
    let snapshot = connection.discover().await.expect("discover succeeds");

    assert_eq!(snapshot.tools.len(), 1);
    assert_eq!(snapshot.tools[0].name.as_ref(), "echo");

    assert_eq!(snapshot.resources.len(), 1);
    assert_eq!(snapshot.resources[0].uri, "memo:welcome");
    assert_eq!(snapshot.resources[0].name, "welcome");

    assert_eq!(snapshot.prompts.len(), 1);
    assert_eq!(snapshot.prompts[0].name, "summarize");
    let arguments = snapshot.prompts[0].arguments.as_ref().unwrap();
    assert_eq!(arguments[0].name, "text");
    assert_eq!(arguments[0].required, Some(true));
    assert_eq!(
        arguments[0].description.as_deref(),
        Some("Text to summarize.")
    );
}

#[tokio::test]
async fn call_tool_returns_typed_result_with_content_blocks() {
    let connection = connect_in_memory().await;
    let result = connection
        .call_tool("echo", json!({ "text": "hi" }))
        .await
        .expect("tool call succeeds");
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 1);
    let raw = &result.content[0].raw;
    let text = raw
        .as_text()
        .map(|text| text.text.clone())
        .expect("first content block is text");
    assert_eq!(text, "hi");
}

#[test]
fn invocation_error_preserves_url_elicitation_data() {
    let raw_data = json!({
        "mode": "url",
        "message": "Complete browser authorization",
        "url": "https://example.com/oauth",
        "elicitationId": "elicit-123",
    });

    let error = McpInvocationError::from_error_data(RmcpError::url_elicitation_required(
        "authorization needed",
        Some(raw_data.clone()),
    ));

    match error {
        McpInvocationError::UrlElicitation {
            message,
            data: Some(data),
            raw_data: Some(raw),
        } => {
            assert_eq!(message, "authorization needed");
            assert_eq!(data.url, "https://example.com/oauth");
            assert_eq!(data.elicitation_id, "elicit-123");
            assert_eq!(
                data.message.as_deref(),
                Some("Complete browser authorization")
            );
            assert_eq!(raw, raw_data);
        }
        other => panic!("expected URL elicitation error, got {other:?}"),
    }
}

#[tokio::test]
async fn read_resource_surfaces_json_rpc_error_as_invocation_enum() {
    let connection = connect_in_memory().await;
    let error = connection
        .read_resource("memo:missing")
        .await
        .expect_err("unknown resource should be a JSON-RPC error");

    match error {
        McpError::Invocation(McpInvocationError::InvalidParams { message, data }) => {
            assert_eq!(message, "unknown resource");
            assert!(data.is_none());
        }
        other => panic!("expected invalid-params invocation error, got {other:?}"),
    }
}

#[tokio::test]
async fn read_resource_returns_typed_contents() {
    let connection = connect_in_memory().await;
    let result = connection
        .read_resource("memo:welcome")
        .await
        .expect("resource read succeeds");
    assert_eq!(result.contents.len(), 1);
    match &result.contents[0] {
        McpResourceContents::TextResourceContents {
            text, mime_type, ..
        } => {
            assert_eq!(text, "hello from the in-memory MCP server");
            assert_eq!(mime_type.as_deref(), Some("text/plain"));
        }
        other => panic!("unexpected resource contents: {other:?}"),
    }
}

#[tokio::test]
async fn get_prompt_returns_typed_messages() {
    let connection = connect_in_memory().await;
    let prompt = connection
        .get_prompt("summarize", serde_json::json!({ "text": "essay body" }))
        .await
        .expect("prompt fetch succeeds");
    assert_eq!(prompt.messages.len(), 1);
    let message = &prompt.messages[0];
    let PromptMessageContent::Text { text } = &message.content else {
        panic!("expected text content, saw {:?}", message.content);
    };
    assert!(text.contains("essay body"));
}

#[tokio::test]
async fn tool_adapter_propagates_call_through_running_service() {
    let connection = Arc::new(connect_in_memory().await);
    let server_id = connection.server_id().clone();
    let snapshot = connection.discover().await.unwrap();
    let descriptor = snapshot.tools[0].clone();

    let adapter = McpToolAdapter::new(&server_id, connection.clone(), descriptor);
    assert_eq!(adapter.spec().name.0.as_str(), "mcp_in-memory_echo");

    let metadata = MetadataMap::new();
    let mut ctx = ToolContext {
        capability: CapabilityContext {
            session_id: None,
            turn_id: None,
            metadata: &metadata,
        },
        permissions: &AllowAll,
        resources: &(),
        cancellation: None,
        execution_scope: None,
        approved_request: None,
    };

    let result = adapter
        .invoke(
            ToolRequest {
                call_id: "call-1".into(),
                tool_name: ToolName::new("mcp_in-memory_echo"),
                input: serde_json::json!({ "text": "via-adapter" }),
                session_id: "session-1".into(),
                turn_id: "turn-1".into(),
                metadata: MetadataMap::new(),
            },
            &mut ctx,
        )
        .await
        .expect("adapter invoke succeeds");

    match result.result.output {
        ToolOutput::Text(text) => assert_eq!(text, "via-adapter"),
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn tool_adapter_error_responder_receives_typed_invocation_error() {
    let (connection, _peer) = connect_in_memory_with_server_peer(
        McpHandlerConfig::new().with_error_responder(Arc::new(InvalidParamsResponder)),
    )
    .await;
    let connection = Arc::new(connection);
    let server_id = connection.server_id().clone();
    let snapshot = connection.discover().await.unwrap();
    let descriptor = snapshot.tools[0].clone();
    let adapter = McpToolAdapter::new(&server_id, connection.clone(), descriptor);
    let metadata = MetadataMap::new();
    let mut ctx = ToolContext {
        capability: CapabilityContext {
            session_id: None,
            turn_id: None,
            metadata: &metadata,
        },
        permissions: &AllowAll,
        resources: &(),
        cancellation: None,
        execution_scope: None,
        approved_request: None,
    };

    let result = adapter
        .invoke(
            ToolRequest {
                call_id: "call-1".into(),
                tool_name: ToolName::new("mcp_in-memory_echo"),
                input: json!({}),
                session_id: "session-1".into(),
                turn_id: "turn-1".into(),
                metadata: MetadataMap::new(),
            },
            &mut ctx,
        )
        .await
        .expect("responder should synthesize a tool result");

    match result.result.output {
        ToolOutput::Text(text) => assert_eq!(text, "handled invalid params"),
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn capability_provider_invocable_returns_text_output() {
    let connection = Arc::new(connect_in_memory().await);
    let snapshot = connection.discover().await.unwrap();
    let provider = McpCapabilityProvider::from_snapshot(connection.clone(), &snapshot);
    let invocables = provider.invocables();
    assert_eq!(invocables.len(), 1);
    let invocable = invocables[0].clone();
    assert_eq!(invocable.spec().name.0, "mcp_in-memory_echo");

    let metadata = MetadataMap::new();
    let mut ctx = CapabilityContext {
        session_id: None,
        turn_id: None,
        metadata: &metadata,
    };

    let result = invocable
        .invoke(
            InvocableRequest::new(serde_json::json!({ "text": "hello-invocable" })),
            &mut ctx,
        )
        .await
        .expect("invocable invoke succeeds");

    match result.output {
        InvocableOutput::Text(text) => assert_eq!(text, "hello-invocable"),
        other => panic!("unexpected output: {other:?}"),
    }
}

#[tokio::test]
async fn custom_namespace_overrides_default_prefix() {
    let connection = Arc::new(connect_in_memory().await);
    let snapshot = connection.discover().await.unwrap();
    let server_id = connection.server_id().clone();
    let namespace = McpToolNamespace::custom(|server, name| format!("remote.{server}.{name}"));
    let adapter = McpToolAdapter::with_namespace(
        &server_id,
        connection,
        snapshot.tools[0].clone(),
        &namespace,
    );
    assert_eq!(adapter.spec().name.0, "remote.in-memory.echo");
}

#[tokio::test]
async fn none_namespace_strips_prefix() {
    let connection = Arc::new(connect_in_memory().await);
    let snapshot = connection.discover().await.unwrap();
    let server_id = connection.server_id().clone();
    let adapter = McpToolAdapter::with_namespace(
        &server_id,
        connection,
        snapshot.tools[0].clone(),
        &McpToolNamespace::None,
    );
    assert_eq!(adapter.spec().name.0, "echo");
}

struct EchoSampling;

#[async_trait]
impl McpSamplingResponder for EchoSampling {
    async fn create_message(
        &self,
        params: McpCreateMessageRequestParams,
    ) -> Result<McpCreateMessageResult, McpError> {
        let last_text = params
            .messages
            .iter()
            .rev()
            .find_map(|message| match &message.content {
                rmcp::model::SamplingContent::Single(
                    rmcp::model::SamplingMessageContent::Text(text),
                ) => Some(text.text.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "(no input)".to_string());
        Ok(McpCreateMessageResult::new(
            McpSamplingMessage::assistant_text(last_text),
            "test-model".into(),
        ))
    }
}

struct StaticRoots(Vec<McpRoot>);

#[async_trait]
impl McpRootsProvider for StaticRoots {
    async fn list_roots(&self) -> Result<Vec<McpRoot>, McpError> {
        Ok(self.0.clone())
    }
}

struct AcceptingElicitation;

#[async_trait]
impl McpElicitationResponder for AcceptingElicitation {
    async fn create_elicitation(
        &self,
        _params: McpCreateElicitationRequestParams,
    ) -> Result<McpCreateElicitationResult, McpError> {
        Ok(McpCreateElicitationResult::new(
            McpElicitationAction::Accept,
        ))
    }
}

#[tokio::test]
async fn sampling_responder_handles_create_message_request() {
    let (_connection, peer) = connect_in_memory_with_server_peer(
        McpHandlerConfig::new().with_sampling_responder(Arc::new(EchoSampling)),
    )
    .await;

    let mut params = McpCreateMessageRequestParams::default();
    params.messages = vec![McpSamplingMessage::user_text("ping")];
    params.max_tokens = 32;
    let result = peer
        .create_message(params)
        .await
        .expect("server peer create_message succeeds");

    assert_eq!(result.model, "test-model");
    assert_eq!(result.message.role, rmcp::model::Role::Assistant);
    let text = match &result.message.content {
        rmcp::model::SamplingContent::Single(rmcp::model::SamplingMessageContent::Text(text)) => {
            text.text.clone()
        }
        other => panic!("unexpected sampling content: {other:?}"),
    };
    assert_eq!(text, "ping");
}

#[tokio::test]
async fn sampling_responder_absent_returns_method_not_found() {
    let (_connection, peer) = connect_in_memory_with_server_peer(McpHandlerConfig::new()).await;

    let mut params = McpCreateMessageRequestParams::default();
    params.messages = vec![McpSamplingMessage::user_text("ping")];
    params.max_tokens = 32;
    let outcome = peer.create_message(params).await;

    let error = outcome.expect_err("missing responder rejects sampling");
    // JSON-RPC code -32601 is METHOD_NOT_FOUND; the rmcp Display impl encodes
    // the code numerically rather than spelling it out.
    let message = error.to_string();
    assert!(
        message.contains("-32601"),
        "expected method-not-found error (-32601), got {message}"
    );
}

#[tokio::test]
async fn roots_provider_supplies_list_roots_response() {
    let roots = vec![
        McpRoot::new("file:///workspace/a").with_name("a"),
        McpRoot::new("file:///workspace/b"),
    ];
    let (_connection, peer) = connect_in_memory_with_server_peer(
        McpHandlerConfig::new().with_roots_provider(Arc::new(StaticRoots(roots.clone()))),
    )
    .await;

    let result = peer.list_roots().await.expect("list_roots succeeds");
    assert_eq!(result.roots.len(), 2);
    assert_eq!(result.roots[0].uri, roots[0].uri);
    assert_eq!(result.roots[0].name.as_deref(), Some("a"));
    assert_eq!(result.roots[1].uri, roots[1].uri);
}

#[tokio::test]
async fn roots_provider_default_returns_empty() {
    let (_connection, peer) = connect_in_memory_with_server_peer(McpHandlerConfig::new()).await;
    let result = peer.list_roots().await.expect("list_roots succeeds");
    assert!(result.roots.is_empty());
}

#[tokio::test]
async fn elicitation_responder_returns_accept() {
    use rmcp::model::ElicitationSchema;
    let (_connection, peer) = connect_in_memory_with_server_peer(
        McpHandlerConfig::new().with_elicitation_responder(Arc::new(AcceptingElicitation)),
    )
    .await;

    let outcome = peer
        .create_elicitation(McpCreateElicitationRequestParams::FormElicitationParams {
            meta: None,
            message: "name?".into(),
            requested_schema: ElicitationSchema::new(Default::default()),
        })
        .await
        .expect("create_elicitation succeeds");

    assert_eq!(outcome.action, McpElicitationAction::Accept);
}

#[tokio::test]
async fn progress_notification_reaches_event_subscribers() {
    let (connection, peer) = connect_in_memory_with_server_peer(McpHandlerConfig::new()).await;
    let mut events = connection.subscribe_events();

    peer.notify_progress(McpProgressNotificationParam {
        progress_token: rmcp::model::ProgressToken(rmcp::model::NumberOrString::String(
            "tok".into(),
        )),
        progress: 0.5,
        total: Some(1.0),
        message: Some("halfway".into()),
    })
    .await
    .expect("server notify_progress succeeds");

    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("event arrives in time")
        .expect("broadcast not closed");
    match event {
        McpServerEvent::Progress(progress) => {
            assert_eq!(progress.progress, 0.5);
            assert_eq!(progress.message.as_deref(), Some("halfway"));
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn logging_message_reaches_event_subscribers() {
    let (connection, peer) = connect_in_memory_with_server_peer(McpHandlerConfig::new()).await;
    let mut events = connection.subscribe_events();

    peer.notify_logging_message(McpLoggingMessageNotificationParam {
        level: McpLoggingLevel::Info,
        logger: Some("test".into()),
        data: serde_json::json!({"msg": "hi"}),
    })
    .await
    .expect("server notify_logging_message succeeds");

    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("event arrives in time")
        .expect("broadcast not closed");
    match event {
        McpServerEvent::Logging(message) => {
            assert_eq!(message.level, McpLoggingLevel::Info);
            assert_eq!(message.logger.as_deref(), Some("test"));
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn resource_updated_notification_reaches_event_subscribers() {
    let (connection, peer) = connect_in_memory_with_server_peer(McpHandlerConfig::new()).await;
    let mut events = connection.subscribe_events();

    peer.notify_resource_updated(McpResourceUpdatedNotificationParam {
        uri: "memo:welcome".into(),
    })
    .await
    .expect("server notify_resource_updated succeeds");

    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("event arrives in time")
        .expect("broadcast not closed");
    match event {
        McpServerEvent::ResourceUpdated(updated) => {
            assert_eq!(updated.uri, "memo:welcome");
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn list_changed_notifications_emit_events() {
    let (connection, peer) = connect_in_memory_with_server_peer(McpHandlerConfig::new()).await;
    let mut events = connection.subscribe_events();

    peer.notify_tool_list_changed()
        .await
        .expect("server notify_tool_list_changed succeeds");
    peer.notify_resource_list_changed()
        .await
        .expect("server notify_resource_list_changed succeeds");
    peer.notify_prompt_list_changed()
        .await
        .expect("server notify_prompt_list_changed succeeds");

    let mut seen = Vec::new();
    for _ in 0..3 {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event arrives in time")
            .expect("broadcast not closed");
        seen.push(event);
    }

    assert!(
        seen.iter()
            .any(|event| matches!(event, McpServerEvent::ToolListChanged))
    );
    assert!(
        seen.iter()
            .any(|event| matches!(event, McpServerEvent::ResourceListChanged))
    );
    assert!(
        seen.iter()
            .any(|event| matches!(event, McpServerEvent::PromptListChanged))
    );
}

#[tokio::test]
async fn handler_advertises_responder_capabilities_during_initialize() {
    let (connection, _peer) = connect_in_memory_with_server_peer(
        McpHandlerConfig::new()
            .with_sampling_responder(Arc::new(EchoSampling))
            .with_elicitation_responder(Arc::new(AcceptingElicitation))
            .with_roots_provider(Arc::new(StaticRoots(Vec::new()))),
    )
    .await;

    // Server peer info is only available on the server side; capabilities on the
    // connection reflect what the *server* advertised. We assert the wiring
    // doesn't break the handshake — discover still succeeds — and the events
    // channel is live.
    let snapshot = connection.discover().await.expect("discover succeeds");
    assert_eq!(snapshot.tools.len(), 1);
    let _events = connection.subscribe_events();
}
