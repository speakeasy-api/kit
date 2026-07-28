use std::env;
use std::error::Error;
use std::sync::Arc;

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::Request,
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use mcp_reference_interop::{FIXTURE_RESOURCE_URI, RecordedRequest};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{
        router::{Router as McpRouter, prompt::PromptRouter, tool::ToolRouter},
        wrapper::Parameters,
    },
    model::{
        AnnotateAble, GetPromptRequestParams, GetPromptResult, Implementation, ListPromptsResult,
        ListResourcesResult, PaginatedRequestParams, PromptMessage, PromptMessageRole, RawResource,
        ReadResourceRequestParams, ReadResourceResult, ResourceContents, ServerCapabilities,
        ServerInfo,
    },
    prompt, prompt_handler, prompt_router,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

#[derive(Clone, Default)]
struct AppState {
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerMode {
    Stateful,
    StatelessJson,
}

impl ServerMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "stateful" => Some(Self::Stateful),
            "stateless-json" => Some(Self::StatelessJson),
            _ => None,
        }
    }

    fn tool_prefix(&self) -> &'static str {
        match self {
            Self::Stateful => "rmcp-stateful",
            Self::StatelessJson => "rmcp-stateless",
        }
    }

    fn resource_text(&self) -> &'static str {
        match self {
            Self::Stateful => "rust sdk stateful fixture resource",
            Self::StatelessJson => "rust sdk stateless fixture resource",
        }
    }

    fn prompt_text(&self, name: &str) -> String {
        match self {
            Self::Stateful => format!("rust sdk stateful prompt for {name}"),
            Self::StatelessJson => format!("rust sdk stateless prompt for {name}"),
        }
    }

    fn server_name(&self) -> &'static str {
        match self {
            Self::Stateful => "agentkit-rmcp-stateful",
            Self::StatelessJson => "agentkit-rmcp-stateless-json",
        }
    }

    fn transport_config(&self) -> StreamableHttpServerConfig {
        let config = StreamableHttpServerConfig::default();
        match self {
            Self::Stateful => config,
            Self::StatelessJson => config
                .with_stateful_mode(false)
                .with_json_response(true)
                .with_sse_keep_alive(None),
        }
    }
}

#[derive(Debug, Clone)]
struct ReferenceServer {
    mode: ServerMode,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

impl ReferenceServer {
    fn new(mode: ServerMode) -> Self {
        Self {
            mode,
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct EchoRequest {
    text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct GreetingPromptRequest {
    name: String,
}

#[tool_router]
impl ReferenceServer {
    #[tool(name = "echo", description = "Echo the provided text")]
    async fn echo(&self, Parameters(EchoRequest { text }): Parameters<EchoRequest>) -> String {
        format!("{}:{text}", self.mode.tool_prefix())
    }
}

#[prompt_router]
impl ReferenceServer {
    #[prompt(name = "greeting-prompt", description = "Render a simple prompt")]
    async fn greeting_prompt(
        &self,
        Parameters(GreetingPromptRequest { name }): Parameters<GreetingPromptRequest>,
    ) -> GetPromptResult {
        GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            self.mode.prompt_text(&name),
        )])
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for ReferenceServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new(self.mode.server_name(), "1.0.0"))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult {
            resources: vec![
                RawResource::new(FIXTURE_RESOURCE_URI, "greeting-resource")
                    .with_mime_type("text/plain")
                    .with_description("Fixture greeting resource")
                    .no_annotation(),
            ],
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        if request.uri != FIXTURE_RESOURCE_URI {
            return Err(ErrorData::resource_not_found(
                format!("unknown resource: {}", request.uri),
                None,
            ));
        }

        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(self.mode.resource_text(), FIXTURE_RESOURCE_URI)
                .with_mime_type("text/plain"),
        ]))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mode = env::args()
        .nth(1)
        .as_deref()
        .and_then(ServerMode::parse)
        .unwrap_or(ServerMode::Stateful);
    let port = env::args()
        .nth(2)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(32901);

    let config = mode.transport_config();
    let state = AppState::default();
    let service: StreamableHttpService<McpRouter<ReferenceServer>, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                let server = ReferenceServer::new(mode);
                Ok(McpRouter::new(server.clone())
                    .with_tools(server.tool_router.clone())
                    .with_prompts(server.prompt_router.clone()))
            },
            Default::default(),
            config,
        );

    let router = Router::new()
        .route("/_inspect/requests", get(inspect_requests))
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            record_request,
        ))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!("READY http://127.0.0.1:{port}/mcp");
    axum::serve(listener, router).await?;

    Ok(())
}

async fn inspect_requests(State(state): State<AppState>) -> Json<Vec<RecordedRequest>> {
    Json(state.requests.lock().await.clone())
}

async fn record_request(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let body_bytes = to_bytes(body, 1024 * 1024).await.unwrap_or_default();
    let path = parts.uri.path().to_string();

    if path != "/_inspect/requests" {
        let body = String::from_utf8_lossy(&body_bytes).to_string();
        let jsonrpc_method = serde_json::from_slice::<serde_json::Value>(&body_bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            });
        state.requests.lock().await.push(RecordedRequest {
            method: parts.method.to_string(),
            path,
            headers: normalize_headers(&parts.headers),
            body,
            jsonrpc_method,
        });
    }

    next.run(Request::from_parts(parts, Body::from(body_bytes)))
        .await
}

fn normalize_headers(
    headers: &axum::http::HeaderMap,
) -> std::collections::BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect()
}
