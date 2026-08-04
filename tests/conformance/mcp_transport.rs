use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use agentkit_mcp::{
    ClientJsonRpcMessage, McpConnection, McpCreateElicitationRequestParams,
    McpCreateElicitationResult, McpCreateMessageRequestParams, McpCreateMessageResult,
    McpElicitationAction, McpElicitationResponder, McpError, McpHandlerConfig, McpHttpClient,
    McpListRootsResult, McpResponderRequestContext, McpRoot, McpRootsProvider, McpSamplingMessage,
    McpSamplingResponder, McpServerConfig, McpServerId, McpSse, McpSseStream,
    McpStreamableHttpError, McpStreamableHttpPostResponse, McpTransportBinding,
    PINNED_PROTOCOL_VERSION, StreamableHttpTransportConfig,
};
use futures_util::{StreamExt, stream};
use http::{HeaderName, HeaderValue};
use rmcp::{
    RoleClient,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use kit::protocols::mcp::transport::PROTOCOL_REVISION;

#[derive(Clone)]
struct WireLog(Arc<Mutex<Vec<Value>>>);

impl WireLog {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    fn push<T: serde::Serialize>(&self, message: &T) {
        self.0
            .lock()
            .unwrap()
            .push(serde_json::to_value(message).unwrap());
    }

    fn methods(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|message| message.get("method").and_then(Value::as_str))
            .map(str::to_owned)
            .collect()
    }

    fn initialize_capabilities(&self) -> Value {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|message| message.get("method").and_then(Value::as_str) == Some("initialize"))
            .and_then(|message| message.pointer("/params/capabilities"))
            .cloned()
            .expect("initialize capabilities recorded")
    }

    fn response_ids(&self) -> Vec<Value> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message.get("method").is_none() && message.get("result").is_some())
            .filter_map(|message| message.get("id").cloned())
            .collect()
    }
}

struct EmptyRoots;

#[async_trait::async_trait]
impl McpRootsProvider for EmptyRoots {
    async fn list_roots(
        &self,
        _context: McpResponderRequestContext,
    ) -> Result<Vec<McpRoot>, McpError> {
        Ok(Vec::new())
    }
}

fn track_response<T: serde::Serialize>(context: &McpResponderRequestContext, result: &T) {
    context
        .on_delivery(
            context
                .callback_delivery_token("transport-test", [1; 32], [2; 32])
                .unwrap(),
            result,
            || true,
            |_, delivered| assert!(delivered),
        )
        .unwrap();
}

struct TrackedResponders;

#[async_trait::async_trait]
impl McpSamplingResponder for TrackedResponders {
    async fn create_message(
        &self,
        _params: McpCreateMessageRequestParams,
        context: McpResponderRequestContext,
    ) -> Result<McpCreateMessageResult, McpError> {
        let result = McpCreateMessageResult::new(
            McpSamplingMessage::assistant_text("sampled"),
            "test-model".to_owned(),
        );
        track_response(&context, &result);
        Ok(result)
    }
}

struct GuardedRoots {
    authority: Arc<AtomicBool>,
    delivery: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl McpRootsProvider for GuardedRoots {
    async fn list_roots(
        &self,
        context: McpResponderRequestContext,
    ) -> Result<Vec<McpRoot>, McpError> {
        let roots = vec![McpRoot::new("file:///workspace")];
        let result = McpListRootsResult::new(roots.clone());
        let authority = Arc::clone(&self.authority);
        let delivery = Arc::clone(&self.delivery);
        context.on_delivery(
            context.callback_delivery_token("guarded-roots", [1; 32], [2; 32])?,
            &result,
            move || authority.load(Ordering::Acquire),
            move |_, delivered| {
                delivery.store(if delivered { 1 } else { 2 }, Ordering::Release);
            },
        )?;
        Ok(roots)
    }
}

#[async_trait::async_trait]
impl McpElicitationResponder for TrackedResponders {
    async fn create_elicitation(
        &self,
        _params: McpCreateElicitationRequestParams,
        context: McpResponderRequestContext,
    ) -> Result<McpCreateElicitationResult, McpError> {
        let result = McpCreateElicitationResult::new(McpElicitationAction::Decline);
        track_response(&context, &result);
        Ok(result)
    }
}

#[async_trait::async_trait]
impl McpRootsProvider for TrackedResponders {
    async fn list_roots(
        &self,
        context: McpResponderRequestContext,
    ) -> Result<Vec<McpRoot>, McpError> {
        let roots = vec![McpRoot::new("file:///workspace")];
        track_response(&context, &McpListRootsResult::new(roots.clone()));
        Ok(roots)
    }
}

fn tracked_handler() -> McpHandlerConfig {
    let responders = Arc::new(TrackedResponders);
    McpHandlerConfig::new()
        .with_sampling_responder(responders.clone())
        .with_elicitation_responder(responders.clone())
        .with_roots_provider(responders)
}

fn callback_requests() -> [Value; 3] {
    let sampling = McpCreateMessageRequestParams::new(vec![McpSamplingMessage::user_text("hi")], 8);
    [
        json!({
            "jsonrpc":"2.0", "id":101, "method":"sampling/createMessage",
            "params": serde_json::to_value(sampling).unwrap()
        }),
        json!({
            "jsonrpc":"2.0", "id":102, "method":"elicitation/create",
            "params":{"message":"Name","requestedSchema":{"type":"object","properties":{}}}
        }),
        json!({"jsonrpc":"2.0", "id":103, "method":"roots/list", "params":{}}),
    ]
}

struct MemoryTransport {
    version: &'static str,
    log: WireLog,
    fail_responses: bool,
    tx: mpsc::UnboundedSender<RxJsonRpcMessage<RoleClient>>,
    rx: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleClient>>,
}

impl MemoryTransport {
    fn new(version: &'static str, log: WireLog) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            version,
            log,
            fail_responses: false,
            tx,
            rx,
        }
    }

    fn sender(&self) -> mpsc::UnboundedSender<RxJsonRpcMessage<RoleClient>> {
        self.tx.clone()
    }

    fn with_failed_responses(mut self) -> Self {
        self.fail_responses = true;
        self
    }
}

impl Transport<RoleClient> for MemoryTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let version = self.version;
        let log = self.log.clone();
        let fail_responses = self.fail_responses;
        let tx = self.tx.clone();
        async move {
            log.push(&item);
            let request = serde_json::to_value(&item).map_err(std::io::Error::other)?;
            if request.get("method").is_none() && request.get("result").is_some() {
                if !agentkit_mcp::has_responder_delivery_permit(&item) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "methodless response lacked responder permit",
                    ));
                }
                if fail_responses {
                    return Err(std::io::Error::other("response send failed"));
                }
                return Ok(());
            }
            let Some(id) = request.get("id").cloned() else {
                return Ok(());
            };
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "memory", "version": "1"}
                }),
                "tools/list" => json!({"tools": []}),
                _ => json!({}),
            };
            let response = serde_json::from_value(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }))
            .map_err(std::io::Error::other)?;
            tx.send(response)
                .map_err(|_| std::io::Error::other("memory transport closed"))
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.rx.recv().await
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum InitEncoding {
    Json,
    Sse,
}

struct MemoryHttpClient {
    version: &'static str,
    encoding: InitEncoding,
    log: WireLog,
    deletes: AtomicUsize,
    open_session: AtomicBool,
    auth_initialize: bool,
    expire_tools: bool,
    fail_responses: bool,
    callback_rx: Mutex<Option<mpsc::UnboundedReceiver<McpSse>>>,
}

impl MemoryHttpClient {
    fn new(version: &'static str, encoding: InitEncoding, log: WireLog) -> Self {
        Self {
            version,
            encoding,
            log,
            deletes: AtomicUsize::new(0),
            open_session: AtomicBool::new(false),
            auth_initialize: false,
            expire_tools: false,
            fail_responses: false,
            callback_rx: Mutex::new(None),
        }
    }

    fn with_auth_initialize(mut self) -> Self {
        self.auth_initialize = true;
        self
    }

    fn with_expired_tools(mut self) -> Self {
        self.expire_tools = true;
        self
    }

    fn with_failed_responses(mut self) -> Self {
        self.fail_responses = true;
        self
    }

    fn with_callback_stream(mut self) -> (Self, mpsc::UnboundedSender<McpSse>) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.callback_rx = Mutex::new(Some(rx));
        (self, tx)
    }

    fn response(&self, request: &Value, result: Value) -> Value {
        json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
    }
}

#[async_trait::async_trait]
impl McpHttpClient for MemoryHttpClient {
    async fn post_message(
        &self,
        _uri: Arc<str>,
        message: ClientJsonRpcMessage,
        _session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<McpStreamableHttpPostResponse, McpStreamableHttpError<reqwest::Error>> {
        self.log.push(&message);
        let request = serde_json::to_value(&message)?;
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if method.is_empty() && request.get("result").is_some() {
            if !agentkit_mcp::has_responder_delivery_permit(&message) {
                return Err(McpStreamableHttpError::UnexpectedServerResponse(
                    "methodless response lacked responder permit".into(),
                ));
            }
            if self.fail_responses {
                return Err(McpStreamableHttpError::UnexpectedServerResponse(
                    "response send failed".into(),
                ));
            }
            return Ok(McpStreamableHttpPostResponse::Accepted);
        }
        if method == "initialize" && self.auth_initialize {
            return Err(McpStreamableHttpError::AuthRequired(
                rmcp::transport::streamable_http_client::AuthRequiredError::new(
                    "Bearer scope=\"mcp.connect\"".to_owned(),
                ),
            ));
        }
        if method == "tools/list" && self.expire_tools {
            return Err(McpStreamableHttpError::SessionExpired);
        }
        if request.get("id").is_none() {
            return Ok(McpStreamableHttpPostResponse::Accepted);
        }
        let result = match method {
            "initialize" => {
                let mut result = json!({
                    "protocolVersion": self.version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "memory-http", "version": "1"}
                });
                if self.version == "missing" {
                    result.as_object_mut().unwrap().remove("protocolVersion");
                }
                result
            }
            "tools/list" => match request.pointer("/params/cursor").and_then(Value::as_str) {
                None => json!({
                    "tools":[{"name":"first", "inputSchema":{"type":"object"}}],
                    "nextCursor":"page-2"
                }),
                Some("page-2") => json!({
                    "tools":[{"name":"second", "inputSchema":{"type":"object"}}]
                }),
                Some(_) => json!({"tools":[]}),
            },
            _ => json!({}),
        };
        let response = self.response(&request, result);
        if method == "initialize" {
            self.open_session.store(true, Ordering::Release);
        }
        match (method, self.encoding) {
            ("initialize", InitEncoding::Sse) => {
                let event = McpSse {
                    data: Some(response.to_string()),
                    ..McpSse::default()
                };
                Ok(McpStreamableHttpPostResponse::Sse(
                    stream::once(async move { Ok(event) }).boxed(),
                    Some("session-1".to_owned()),
                ))
            }
            _ => Ok(McpStreamableHttpPostResponse::Json(
                serde_json::from_value(response)?,
                (method == "initialize").then(|| "session-1".to_owned()),
            )),
        }
    }

    async fn delete_session(
        &self,
        _uri: Arc<str>,
        _session_id: Arc<str>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        self.open_session.store(false, Ordering::Release);
        Ok(())
    }

    async fn get_stream(
        &self,
        _uri: Arc<str>,
        _session_id: Arc<str>,
        _last_event_id: Option<String>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<McpSseStream, McpStreamableHttpError<reqwest::Error>> {
        let receiver = self.callback_rx.lock().unwrap().take();
        let Some(receiver) = receiver else {
            return Err(McpStreamableHttpError::ServerDoesNotSupportSse);
        };
        Ok(stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|event| (Ok(event), receiver))
        })
        .boxed())
    }

    async fn close_open_sessions(&self) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
        if self.open_session.swap(false, Ordering::AcqRel) {
            self.deletes.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

fn http_config(client: Arc<MemoryHttpClient>) -> McpServerConfig {
    McpServerConfig::new(
        "behavioral-http",
        McpTransportBinding::StreamableHttp(
            StreamableHttpTransportConfig::new("https://mcp.invalid").with_http_client(client),
        ),
    )
}

fn assert_typed_refusal(error: McpError) {
    assert!(matches!(
        error,
        McpError::UnsupportedProtocolVersion {
            expected,
            ..
        } if expected == PINNED_PROTOCOL_VERSION
    ));
}

async fn wait_for_callback_responses(log: &WireLog) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if log.response_ids().len() == 3 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("callback responses reached transport");
    assert_eq!(log.response_ids(), [json!(101), json!(102), json!(103)]);
}

async fn wait_for_delivery(delivery: &AtomicUsize) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while delivery.load(Ordering::Acquire) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("callback delivery settled");
}

#[tokio::test]
async fn both_authorized_transports_refuse_before_initialized_or_discovery() {
    assert_eq!(PROTOCOL_REVISION, PINNED_PROTOCOL_VERSION);

    let stdio_log = WireLog::new();
    let error = match McpConnection::connect_kit_authorized_transport(
        McpServerId::new("behavioral-stdio"),
        MemoryTransport::new("2025-06-18", stdio_log.clone()),
        McpHandlerConfig::new(),
    )
    .await
    {
        Ok(_) => panic!("old stdio revision connected"),
        Err(error) => error,
    };
    assert_typed_refusal(error);
    assert_eq!(stdio_log.methods(), ["initialize"]);

    for version in ["2025-06-18", "missing"] {
        for encoding in [InitEncoding::Json, InitEncoding::Sse] {
            let log = WireLog::new();
            let client = Arc::new(MemoryHttpClient::new(version, encoding, log.clone()));
            let error = match McpConnection::connect_authorized_http(
                &http_config(Arc::clone(&client)),
                McpHandlerConfig::new(),
            )
            .await
            {
                Ok(_) => panic!("unsupported HTTP revision connected"),
                Err(error) => error,
            };
            assert_typed_refusal(error);
            assert_eq!(log.methods(), ["initialize"]);
            assert_eq!(client.deletes.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn stdio_and_http_advertise_only_the_installed_roots_responder() {
    let handler = McpHandlerConfig::new().with_roots_provider(Arc::new(EmptyRoots));
    let stdio_log = WireLog::new();
    let stdio = McpConnection::connect_kit_authorized_transport(
        McpServerId::new("roots-stdio"),
        MemoryTransport::new("2025-11-25", stdio_log.clone()),
        handler.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        stdio_log.initialize_capabilities(),
        json!({"roots":{"listChanged":false}})
    );
    stdio.close().await.unwrap();

    let http_log = WireLog::new();
    let client = Arc::new(MemoryHttpClient::new(
        "2025-11-25",
        InitEncoding::Json,
        http_log.clone(),
    ));
    let http = McpConnection::connect_authorized_http(&http_config(client), handler)
        .await
        .unwrap();
    assert_eq!(
        http_log.initialize_capabilities(),
        json!({"roots":{"listChanged":false}})
    );
    http.close().await.unwrap();
}

#[tokio::test]
async fn stdio_sampling_elicitation_and_roots_responses_require_delivery_permits() {
    let log = WireLog::new();
    let transport = MemoryTransport::new("2025-11-25", log.clone());
    let peer = transport.sender();
    let connection = McpConnection::connect_kit_authorized_transport(
        McpServerId::new("callback-stdio"),
        transport,
        tracked_handler(),
    )
    .await
    .unwrap();
    for request in callback_requests() {
        peer.send(serde_json::from_value(request).unwrap()).unwrap();
    }
    wait_for_callback_responses(&log).await;
    connection.close().await.unwrap();
}

#[tokio::test]
async fn http_sampling_elicitation_and_roots_responses_require_delivery_permits() {
    let log = WireLog::new();
    let (client, peer) =
        MemoryHttpClient::new("2025-11-25", InitEncoding::Json, log.clone()).with_callback_stream();
    let connection =
        McpConnection::connect_authorized_http(&http_config(Arc::new(client)), tracked_handler())
            .await
            .unwrap();
    for request in callback_requests() {
        peer.send(McpSse {
            data: Some(request.to_string()),
            ..McpSse::default()
        })
        .unwrap();
    }
    wait_for_callback_responses(&log).await;
    connection.close().await.unwrap();
}

#[tokio::test]
async fn stdio_and_http_roots_authority_is_rechecked_before_methodless_send() {
    for http in [false, true] {
        let log = WireLog::new();
        let authority = Arc::new(AtomicBool::new(false));
        let delivery = Arc::new(AtomicUsize::new(0));
        let handler = McpHandlerConfig::new().with_roots_provider(Arc::new(GuardedRoots {
            authority,
            delivery: Arc::clone(&delivery),
        }));
        let request = json!({"jsonrpc":"2.0", "id":103, "method":"roots/list", "params":{}});
        if http {
            let (client, peer) =
                MemoryHttpClient::new("2025-11-25", InitEncoding::Json, log.clone())
                    .with_callback_stream();
            let connection =
                McpConnection::connect_authorized_http(&http_config(Arc::new(client)), handler)
                    .await
                    .unwrap();
            peer.send(McpSse {
                data: Some(request.to_string()),
                ..McpSse::default()
            })
            .unwrap();
            wait_for_delivery(&delivery).await;
            connection.close().await.unwrap();
        } else {
            let transport = MemoryTransport::new("2025-11-25", log.clone());
            let peer = transport.sender();
            let connection = McpConnection::connect_kit_authorized_transport(
                McpServerId::new("guarded-roots-stdio"),
                transport,
                handler,
            )
            .await
            .unwrap();
            peer.send(serde_json::from_value(request).unwrap()).unwrap();
            wait_for_delivery(&delivery).await;
            connection.close().await.unwrap();
        }
        assert_eq!(delivery.load(Ordering::Acquire), 2);
        assert!(log.response_ids().is_empty());
    }
}

#[tokio::test]
async fn stdio_and_http_roots_send_failures_settle_delivery_unknown() {
    for http in [false, true] {
        let log = WireLog::new();
        let delivery = Arc::new(AtomicUsize::new(0));
        let handler = McpHandlerConfig::new().with_roots_provider(Arc::new(GuardedRoots {
            authority: Arc::new(AtomicBool::new(true)),
            delivery: Arc::clone(&delivery),
        }));
        let request = json!({"jsonrpc":"2.0", "id":103, "method":"roots/list", "params":{}});
        if http {
            let (client, peer) =
                MemoryHttpClient::new("2025-11-25", InitEncoding::Json, log.clone())
                    .with_failed_responses()
                    .with_callback_stream();
            let connection =
                McpConnection::connect_authorized_http(&http_config(Arc::new(client)), handler)
                    .await
                    .unwrap();
            peer.send(McpSse {
                data: Some(request.to_string()),
                ..McpSse::default()
            })
            .unwrap();
            wait_for_delivery(&delivery).await;
            let _ = connection.close().await;
        } else {
            let transport = MemoryTransport::new("2025-11-25", log.clone()).with_failed_responses();
            let peer = transport.sender();
            let connection = McpConnection::connect_kit_authorized_transport(
                McpServerId::new("failed-roots-stdio"),
                transport,
                handler,
            )
            .await
            .unwrap();
            peer.send(serde_json::from_value(request).unwrap()).unwrap();
            wait_for_delivery(&delivery).await;
            let _ = connection.close().await;
        }
        assert_eq!(delivery.load(Ordering::Acquire), 2);
        assert_eq!(log.response_ids(), [json!(103)]);
    }
}

#[tokio::test]
async fn http_pages_are_single_wire_calls_with_exact_cursor_and_cleanup() {
    let log = WireLog::new();
    let client = Arc::new(MemoryHttpClient::new(
        "2025-11-25",
        InitEncoding::Json,
        log.clone(),
    ));
    let connection = McpConnection::connect_authorized_http(
        &http_config(Arc::clone(&client)),
        McpHandlerConfig::new(),
    )
    .await
    .unwrap();

    let (first, cursor) = connection.list_tools_page(None).await.unwrap();
    assert_eq!(first[0].name, "first");
    assert_eq!(cursor.as_deref(), Some("page-2"));
    let (second, cursor) = connection.list_tools_page(cursor).await.unwrap();
    assert_eq!(second[0].name, "second");
    assert!(cursor.is_none());

    let pages = log
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|message| message.get("method") == Some(&json!("tools/list")))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(pages.len(), 2);
    assert!(pages[0].pointer("/params/cursor").is_none());
    assert_eq!(pages[1].pointer("/params/cursor"), Some(&json!("page-2")));

    connection.close().await.unwrap();
    assert_eq!(client.deletes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn http_auth_and_session_failures_remain_typed() {
    let auth = Arc::new(
        MemoryHttpClient::new("2025-11-25", InitEncoding::Json, WireLog::new())
            .with_auth_initialize(),
    );
    let error =
        match McpConnection::connect_authorized_http(&http_config(auth), McpHandlerConfig::new())
            .await
        {
            Ok(_) => panic!("auth challenge connected"),
            Err(error) => error,
        };
    assert!(matches!(error, McpError::AuthRequired(_)));

    let expired = Arc::new(
        MemoryHttpClient::new("2025-11-25", InitEncoding::Json, WireLog::new())
            .with_expired_tools(),
    );
    let connection =
        McpConnection::connect_authorized_http(&http_config(expired), McpHandlerConfig::new())
            .await
            .unwrap();
    assert!(matches!(
        connection.list_tools_page(None).await,
        Err(McpError::SessionExpired)
    ));
}

#[test]
fn root_manifest_enables_the_sealed_agentkit_feature() {
    let manifest: toml::Value = toml::from_str(include_str!("../../Cargo.toml")).unwrap();
    let features = manifest["dependencies"]["agentkit-mcp"]["features"]
        .as_array()
        .unwrap();
    assert_eq!(
        features,
        &[toml::Value::String("kit-authorized".to_owned())]
    );
}

#[test]
fn raw_agentkit_mcp_use_is_confined_to_the_transport_adapter() {
    use syn::visit::Visit as _;

    #[derive(Default)]
    struct RawMcpUse {
        paths: Vec<String>,
    }

    fn raw_use_root(tree: &syn::UseTree) -> Option<String> {
        match tree {
            syn::UseTree::Path(path)
                if matches!(path.ident.to_string().as_str(), "agentkit_mcp" | "rmcp") =>
            {
                Some(path.ident.to_string())
            }
            syn::UseTree::Path(path) => raw_use_root(&path.tree),
            syn::UseTree::Group(group) => group.items.iter().find_map(raw_use_root),
            syn::UseTree::Name(name)
                if matches!(name.ident.to_string().as_str(), "agentkit_mcp" | "rmcp") =>
            {
                Some(name.ident.to_string())
            }
            syn::UseTree::Rename(rename)
                if matches!(rename.ident.to_string().as_str(), "agentkit_mcp" | "rmcp") =>
            {
                Some(rename.ident.to_string())
            }
            _ => None,
        }
    }

    impl<'ast> syn::visit::Visit<'ast> for RawMcpUse {
        fn visit_path(&mut self, path: &'ast syn::Path) {
            if path.segments.first().is_some_and(|segment| {
                matches!(segment.ident.to_string().as_str(), "agentkit_mcp" | "rmcp")
            }) {
                self.paths.push(
                    path.segments
                        .iter()
                        .map(|segment| segment.ident.to_string())
                        .collect::<Vec<_>>()
                        .join("::"),
                );
            }
            syn::visit::visit_path(self, path);
        }

        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            if let Some(name) = raw_use_root(&item.tree) {
                self.paths.push(name);
            }
            syn::visit::visit_item_use(self, item);
        }
    }

    fn raw_uses(source: &str) -> Vec<String> {
        let syntax = syn::parse_file(source).unwrap();
        let mut visitor = RawMcpUse::default();
        visitor.visit_file(&syntax);
        visitor.paths
    }

    fn rust_files(directory: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, files);
            } else if path.extension().and_then(std::ffi::OsStr::to_str) == Some("rs") {
                files.push(path);
            }
        }
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let transport = root.join("src/protocols/mcp/transport");
    let responders = root.join("src/protocols/mcp/responders");
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    let offenders = files
        .into_iter()
        .filter(|path| !path.starts_with(&transport) && !path.starts_with(&responders))
        .filter_map(|path| {
            let uses = raw_uses(&std::fs::read_to_string(&path).unwrap());
            (!uses.is_empty()).then_some((path, uses))
        })
        .collect::<Vec<_>>();
    assert!(offenders.is_empty(), "raw AgentKit MCP use: {offenders:?}");

    for path in ["mod.rs", "http.rs", "stdio.rs"] {
        let source = std::fs::read_to_string(transport.join(path)).unwrap();
        let syntax = syn::parse_file(&source).unwrap();
        for item in syntax.items {
            if let syn::Item::Use(item) = item
                && matches!(item.vis, syn::Visibility::Public(_))
            {
                assert!(
                    raw_use_root(&item.tree).is_none(),
                    "raw MCP re-export in {path}"
                );
            }
        }
    }

    let negative = "pub use agentkit_mcp::McpConnection; fn bypass() { rmcp::serve_client(()); }";
    let detected = raw_uses(negative);
    assert!(detected.iter().any(|path| path == "agentkit_mcp"));
    assert!(detected.iter().any(|path| path.starts_with("rmcp")));

    let stdio = std::fs::read_to_string(transport.join("stdio.rs")).unwrap();
    assert!(!stdio.contains("trait AuthorizedStdioLauncher"));
    assert!(!stdio.contains("from_authorized_parts"));
}
