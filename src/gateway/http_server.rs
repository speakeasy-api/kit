//! ACP HTTP connection adapter. Only the resident actor owns execution.
use super::*;
use agent_client_protocol::{Agent, Channel, ConnectTo, TransportFrame};
use futures_util::StreamExt;

// Reserved in addition to ordinary lifetime output; never grows with session
// count, request IDs, or the size of the message that exhausted the budget.
const TERMINAL_ALLOWANCE: usize = 4096;
const OUTPUT_LIMIT_ERROR: &str = "experimental 8 MiB lifetime output limit exceeded; resident session continues; live replay may be unavailable; reconnect with session/resume without replay";

pub(super) struct Connection {
    gateway: Arc<Gateway>,
    boundary: Arc<http_boundary::Boundary>,
    initialize: Option<Value>,
    controls: HashMap<String, Control>,
}

struct Control {
    session: String,
    attachment: String,
    entry: Entry,
    cursor: u64,
    pending: HashMap<String, Value>,
}

impl Drop for Control {
    fn drop(&mut self) {
        // Best-effort release is scoped to this generation. A full mailbox leaves
        // the lease to expire; it never cancels work or revokes a newer owner.
        let (reply, _) = oneshot::channel();
        let _ = self.entry.sender.try_send(Envelope {
            request: Request::Detach {
                session: self.session.clone(),
                attachment: self.attachment.clone(),
            },
            reply,
        });
    }
}

impl Control {
    async fn call(&self, request: Request) -> ResultValue {
        let (reply, result) = oneshot::channel();
        self.entry
            .sender
            .send(Envelope { request, reply })
            .await
            .map_err(|_| Failure::unavailable("resident child exited"))?;
        result
            .await
            .map_err(|_| Failure::unavailable("resident child exited"))?
    }

    async fn send(&mut self, message: Value) -> ResultValue {
        let id = message.get("id").cloned();
        let result = self
            .call(Request::Send {
                session: self.session.clone(),
                attachment: self.attachment.clone(),
                message,
            })
            .await?;
        if let Some(id) = id {
            self.pending.insert(id.to_string(), id);
        }
        Ok(result)
    }

    async fn poll(&mut self) -> Result<Vec<Value>, Failure> {
        let result = self
            .call(Request::Poll {
                session: self.session.clone(),
                attachment: self.attachment.clone(),
                cursor: self.cursor,
            })
            .await?;
        let mut messages = Vec::new();
        if let Some(events) = result["events"].as_array() {
            for event in events {
                self.cursor = event["cursor"]
                    .as_u64()
                    .ok_or_else(|| Failure::unavailable("invalid actor cursor"))?;
                let message = event["message"].clone();
                if message.get("method").is_none()
                    && let Some(id) = message.get("id")
                {
                    self.pending.remove(&id.to_string());
                }
                messages.push(message);
            }
        }
        Ok(messages)
    }
}

impl Connection {
    pub(super) fn new(gateway: Arc<Gateway>, boundary: Arc<http_boundary::Boundary>) -> Self {
        Self {
            gateway,
            boundary,
            initialize: None,
            controls: HashMap::new(),
        }
    }

    async fn operation(&self, request: Request) -> ResultValue {
        super::handle(State(self.gateway.clone()), Json(request))
            .await
            .map(|Json(value)| value)
    }

    async fn request(
        &mut self,
        mut message: Value,
        channel: &Channel,
        output_bytes: &mut usize,
    ) -> Result<Option<Value>, Failure> {
        let method = message["method"]
            .as_str()
            .ok_or_else(|| Failure::bad("ACP method required"))?
            .to_owned();
        if method == http_boundary::DRAIN_METHOD {
            let token = message["params"]["token"]
                .as_str()
                .ok_or_else(|| Failure::bad("drain token required"))?;
            self.boundary.acknowledge(token).await;
            return Ok(None);
        }
        if method == "initialize" {
            if self.initialize.is_some() {
                return Err(Failure::conflict("connection already initialized"));
            }
            // Use the same pinned ACP v2 schema/capabilities as the local agent.
            let mut capabilities = serde_json::to_value(agentkit_acp::v2::agent_capabilities())
                .map_err(|e| Failure::unavailable(e.to_string()))?;
            if let Some(session) = capabilities["session"].as_object_mut() {
                for name in ["close", "delete", "fork", "mcp"] {
                    session.remove(name);
                }
                session.insert("list".into(), object([]));
            }
            self.initialize = Some(message.clone());
            return Ok(Some(object([
                ("protocolVersion", 2.into()),
                (
                    "info",
                    object([
                        ("name", "kit-gateway".into()),
                        ("version", env!("CARGO_PKG_VERSION").into()),
                    ]),
                ),
                ("capabilities", capabilities),
                (
                    "_meta",
                    object([(
                        "kit/gateway",
                        object([
                            ("experimental", true.into()),
                            ("connectionOutputLimitBytes", MAX_REPLAY.into()),
                            ("connectionOutputLimitScope", "lifetime".into()),
                            ("terminalOutputAllowanceBytes", TERMINAL_ALLOWANCE.into()),
                            ("liveReplayLimitBytes", MAX_REPLAY.into()),
                            ("liveReplayLimitEvents", (MAX_QUEUE - 2).into()),
                        ]),
                    )]),
                ),
            ])));
        }
        if self.initialize.is_none() {
            return Err(Failure::bad("initialize is required"));
        }
        if method == "$/cancel_request" {
            return Ok(None);
        }
        if method == "session/list" {
            if message["params"]
                .get("cursor")
                .is_some_and(|v| !v.is_null())
            {
                return Err(Failure::bad("unsupported session list cursor"));
            }
            let result = self.operation(Request::List).await?;
            let rows = result["sessions"]
                .as_array()
                .ok_or_else(|| Failure::unavailable("invalid catalog"))?;
            let cwd = message["params"]["cwd"].as_str();
            let rows: Vec<_> = rows
                .iter()
                .filter(|row| cwd.is_none_or(|cwd| row["root"] == cwd))
                .map(|row| {
                    object([
                        ("sessionId", row["session"].clone()),
                        ("cwd", row["root"].clone()),
                        (
                            "_meta",
                            object([("kit.gateway.state", row["state"].clone())]),
                        ),
                        (
                            "title",
                            row["title"].as_str().unwrap_or("New session").into(),
                        ),
                    ])
                })
                .collect();
            return Ok(Some(object([("sessions", rows.into())])));
        }
        if matches!(method.as_str(), "session/new" | "session/resume") {
            let resume = method == "session/resume";
            let id = if resume {
                Some(
                    message["params"]["sessionId"]
                        .as_str()
                        .ok_or_else(|| Failure::bad("sessionId required"))?
                        .to_owned(),
                )
            } else {
                None
            };
            if id.as_ref().is_some_and(|id| self.controls.contains_key(id)) {
                return Err(Failure::conflict("connection already controls session"));
            }
            for field in ["mcpServers", "additionalDirectories"] {
                if message["params"]
                    .get(field)
                    .is_some_and(|v| !v.is_null() && v != &Value::Array(vec![]))
                {
                    return Err(Failure::bad(
                        "client directories and MCP servers are not supported",
                    ));
                }
            }
            if message["params"]
                .get("replayFrom")
                .is_some_and(Value::is_null)
                && let Some(params) = message["params"].as_object_mut()
            {
                params.remove("replayFrom");
            }
            if message["params"]
                .get("replayFrom")
                .is_some_and(|v| v != &object([("type", "start".into())]))
            {
                return Err(Failure::bad("unsupported replay cursor"));
            }
            let root = match message["params"]["cwd"].as_str() {
                Some(root) => PathBuf::from(root),
                None if resume => {
                    let catalog = self.operation(Request::List).await?;
                    catalog["sessions"]
                        .as_array()
                        .and_then(|rows| {
                            rows.iter()
                                .find(|row| row["session"].as_str() == id.as_deref())
                        })
                        .and_then(|row| row["root"].as_str())
                        .map(PathBuf::from)
                        .ok_or_else(|| Failure::bad("session not found in approved projects"))?
                }
                None => return Err(Failure::bad("cwd required")),
            };
            let created = self
                .operation(Request::Create {
                    root,
                    session: id,
                    force: false,
                })
                .await?;
            let session = created["session"]
                .as_str()
                .ok_or_else(|| Failure::unavailable("missing session id"))?
                .to_owned();
            let attached = self
                .operation(Request::Attach {
                    session: session.clone(),
                    replace: resume,
                    replay: !resume || message["params"].get("replayFrom").is_some(),
                })
                .await?;
            let attachment = attached["attachment"]
                .as_str()
                .ok_or_else(|| Failure::unavailable("missing controller id"))?
                .to_owned();
            let entry = self
                .gateway
                .sessions
                .lock()
                .await
                .get(&session)
                .cloned()
                .ok_or_else(|| Failure::unavailable("resident child exited"))?;
            let mut control = Control {
                session: session.clone(),
                attachment,
                entry,
                cursor: 0,
                pending: HashMap::new(),
            };
            let mut initialize = self
                .initialize
                .clone()
                .ok_or_else(|| Failure::bad("initialize required"))?;
            initialize["id"] = Value::String(crate::session::new_id());
            let initialize_id = initialize["id"].clone();
            control.send(initialize).await?;
            // Setup is serialized before the session request; the actor, not
            // this wait, owns the child request. Cancellation only drops a lease.
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    self.flush(channel, output_bytes)
                        .await
                        .map_err(|error| Failure::unavailable(error.to_string()))?;
                    for output in control.poll().await? {
                        if output["id"] == initialize_id {
                            if output.get("error").is_some() {
                                return Err(Failure::unavailable("child initialization failed"));
                            }
                            return Ok(());
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .map_err(|_| Failure::unavailable("child initialization timed out"))??;
            message["params"]["cwd"] = created["root"].clone();
            control.send(message).await?;
            self.controls.insert(session, control);
            return Ok(None);
        }
        let id = message["params"]["sessionId"]
            .as_str()
            .ok_or_else(|| Failure::bad("sessionId required"))?;
        let control = self
            .controls
            .get_mut(id)
            .ok_or_else(|| Failure::conflict("resume session before controlling it"))?;
        control.send(message).await?;
        Ok(None)
    }

    async fn flush(
        &mut self,
        channel: &Channel,
        output_bytes: &mut usize,
    ) -> agent_client_protocol::Result<()> {
        let mut failed = Vec::new();
        for (id, control) in &mut self.controls {
            match control.poll().await {
                Ok(messages) => {
                    for message in messages {
                        send(channel, message, output_bytes)?;
                    }
                }
                Err(error) => failed.push((id.clone(), error)),
            }
        }
        // A replacement affects one session, not the multiplexed connection.
        // Settle that controller's pending calls explicitly before forgetting it.
        for (id, error) in failed {
            if let Some(mut control) = self.controls.remove(&id) {
                for (_, request_id) in control.pending.drain() {
                    send(
                        channel,
                        object([
                            ("jsonrpc", "2.0".into()),
                            ("id", request_id),
                            (
                                "error",
                                object([
                                    ("code", (-32000).into()),
                                    ("message", error.1.clone().into()),
                                ]),
                            ),
                        ]),
                        output_bytes,
                    )?;
                }
            }
        }
        Ok(())
    }

    async fn run(mut self, mut channel: Channel) -> agent_client_protocol::Result<()> {
        // The pinned SDK mailbox is unbounded. Bound the total bytes produced
        // by one connection, even when an SSE reader disappears without DELETE.
        // The SDK has further unbounded queues and no delivery acknowledgement:
        // channel.tx.len() cannot measure outstanding HTTP output. This is an
        // explicitly advertised experimental lifetime limit, NOT backpressure.
        let mut output_bytes = 0;
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        loop {
            let frame = {
                let received = std::pin::pin!(channel.rx.next());
                let ticked = std::pin::pin!(tick.tick());
                match select(received, ticked).await {
                    Either::Left((frame, _)) => Some(frame),
                    Either::Right(_) => None,
                }
            };
            if let Some(frame) = frame {
                let Some(frame) = frame else {
                    return Ok(());
                };
                let messages = match frame {
                    TransportFrame::Single(message) => vec![message],
                    TransportFrame::Batch(batch) => batch
                        .into_entries()
                        .map(|entry| match entry {
                            agent_client_protocol::TransportBatchEntry::Message(message) => {
                                Ok(message)
                            }
                            agent_client_protocol::TransportBatchEntry::Malformed {
                                error, ..
                            } => Err(error),
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    TransportFrame::Malformed { error, .. } => return Err(error),
                };
                for message in messages {
                    let message = serde_json::to_value(message).map_err(sdk_error)?;
                    let id = message.get("id").cloned();
                    let response = match self.request(message, &channel, &mut output_bytes).await {
                        Ok(Some(result)) => id.map(|id| {
                            object([("jsonrpc", "2.0".into()), ("id", id), ("result", result)])
                        }),
                        Ok(None) => None,
                        Err(error) => id.map(|id| {
                            object([
                                ("jsonrpc", "2.0".into()),
                                ("id", id),
                                (
                                    "error",
                                    object([
                                        ("code", (-32000).into()),
                                        ("message", error.1.into()),
                                    ]),
                                ),
                            ])
                        }),
                    };
                    if let Some(response) = response {
                        send(&channel, response, &mut output_bytes)?;
                    }
                    self.flush(&channel, &mut output_bytes).await?;
                }
            }
            // Drain after every inbound frame as well as idle ticks; a busy
            // client cannot starve updates or let its own controller lease lapse.
            self.flush(&channel, &mut output_bytes).await?;
        }
    }
}

fn sdk_error(error: impl std::fmt::Display) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(error.to_string())
}
fn send(
    channel: &Channel,
    message: Value,
    output_bytes: &mut usize,
) -> agent_client_protocol::Result<()> {
    if *output_bytes > MAX_REPLAY {
        return Err(sdk_error(OUTPUT_LIMIT_ERROR));
    }
    let bytes = message.to_string().len();
    if bytes > MAX_REPLAY - *output_bytes {
        // Send one final, bounded protocol frame before closing. A response can
        // settle its original call; an unsolicited update uses ACP's advisory
        // notice rather than falsely claiming resident execution has stopped.
        let error = |id| {
            object([
                ("jsonrpc", "2.0".into()),
                ("id", id),
                (
                    "error",
                    object([
                        ("code", (-32000).into()),
                        ("message", OUTPUT_LIMIT_ERROR.into()),
                    ]),
                ),
            ])
        };
        let mut terminal = if let Some(id) = message.get("id") {
            error(id.clone())
        } else if let Some(session) = message["params"].get("sessionId") {
            object([
                ("jsonrpc", "2.0".into()),
                ("method", "session/update".into()),
                (
                    "params",
                    object([
                        ("sessionId", session.clone()),
                        (
                            "update",
                            object([
                                ("sessionUpdate", "notice".into()),
                                ("severity", "error".into()),
                                ("title", "Gateway connection output limit reached".into()),
                                ("description", OUTPUT_LIMIT_ERROR.into()),
                            ]),
                        ),
                    ]),
                ),
            ])
        } else {
            error(Value::Null)
        };
        // A client-supplied request ID must not consume an unbounded reserve.
        if terminal.to_string().len() > TERMINAL_ALLOWANCE {
            terminal = error(Value::Null);
        }
        *output_bytes = MAX_REPLAY + terminal.to_string().len();
        channel
            .tx
            .unbounded_send(TransportFrame::Single(
                serde_json::from_value(terminal).map_err(sdk_error)?,
            ))
            .map_err(sdk_error)?;
        eprintln!("Experimental gateway connection closed: {OUTPUT_LIMIT_ERROR}");
        return Err(sdk_error(OUTPUT_LIMIT_ERROR));
    }
    *output_bytes += bytes;
    channel
        .tx
        .unbounded_send(TransportFrame::Single(
            serde_json::from_value(message).map_err(sdk_error)?,
        ))
        .map_err(sdk_error)
}

impl ConnectTo<agent_client_protocol::Client> for Connection {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> agent_client_protocol::Result<()> {
        let (channel, driver) = client.into_channel_and_future();
        // Channel's driver can return immediately while its endpoint remains
        // live. Drive both to completion, rather than treating that as EOF.
        futures_util::future::try_join(self.run(channel), driver).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::disallowed_methods,
        clippy::disallowed_macros
    )]
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn experimental_lifetime_limit_rejects_before_publishing_frame() {
        let (channel, mut peer) = Channel::duplex();
        let message = json!({"jsonrpc":"2.0","id":1,"result":{}});
        let mut bytes = MAX_REPLAY - message.to_string().len();
        send(&channel, message.clone(), &mut bytes).unwrap();
        assert_eq!(bytes, MAX_REPLAY);
        assert!(peer.rx.next().await.is_some());
        // Draining the mailbox intentionally does not reset a lifetime limit.
        let error = send(&channel, message, &mut bytes).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("experimental 8 MiB lifetime output limit")
        );
        let terminal = peer.rx.next().await.unwrap();
        let TransportFrame::Single(terminal) = terminal else {
            panic!("expected a single terminal frame")
        };
        let terminal = serde_json::to_value(terminal).unwrap();
        assert_eq!(terminal["id"], 1);
        assert_eq!(terminal["error"]["code"], -32000);
        assert!(bytes <= MAX_REPLAY + TERMINAL_ALLOWANCE);
        drop(channel);
        assert!(peer.rx.next().await.is_none());
    }

    #[tokio::test]
    async fn terminal_notice_is_bounded_and_preserves_running_session_semantics() {
        for message in [
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"resident","update":{"sessionUpdate":"agent_message_chunk"}}}),
            json!({"jsonrpc":"2.0","id":"x".repeat(TERMINAL_ALLOWANCE),"result":{}}),
        ] {
            let (channel, mut peer) = Channel::duplex();
            let mut bytes = MAX_REPLAY;
            assert!(send(&channel, message.clone(), &mut bytes).is_err());
            let TransportFrame::Single(terminal) = peer.rx.next().await.unwrap() else {
                panic!("expected a single terminal frame")
            };
            let terminal = serde_json::to_value(terminal).unwrap();
            assert!(terminal.to_string().len() <= TERMINAL_ALLOWANCE);
            if message.get("method").is_some() {
                assert_eq!(terminal["params"]["sessionId"], "resident");
                assert_eq!(terminal["params"]["update"]["sessionUpdate"], "notice");
                assert_eq!(terminal["params"]["update"]["severity"], "error");
                assert!(terminal["params"]["update"].get("state").is_none());
                // Validate against the pinned schema, including its extension
                // fallback when unstable notices are not enabled by a consumer.
                let _: agentkit_acp::v2::wire::SessionUpdate =
                    serde_json::from_value(terminal["params"]["update"].clone()).unwrap();
            } else {
                assert!(terminal["id"].is_null());
                assert_eq!(terminal["error"]["code"], -32000);
            }
            assert!(send(&channel, message, &mut bytes).is_err());
            drop(channel);
            assert!(
                peer.rx.next().await.is_none(),
                "only one terminal frame is allowed"
            );
        }
    }

    #[tokio::test]
    async fn sdk_http_drains_terminal_notice_before_closing_stream() {
        // Fake only the ACP agent boundary. The budget writer, SDK mailboxes,
        // HTTP router, session SSE routing, and drain-on-close are real.
        struct ExhaustedAgent;
        impl ConnectTo<agent_client_protocol::Client> for ExhaustedAgent {
            async fn connect_to(
                self,
                client: impl ConnectTo<Agent>,
            ) -> agent_client_protocol::Result<()> {
                let (mut channel, driver) = client.into_channel_and_future();
                let run = async move {
                    let _initialize = channel.rx.next().await.unwrap();
                    let mut output_bytes = 0;
                    send(
                        &channel,
                        json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":2,"info":{"name":"limit-fixture","version":"1"},"capabilities":{}}}),
                        &mut output_bytes,
                    )?;
                    let _trigger = channel.rx.next().await.unwrap();
                    output_bytes = MAX_REPLAY;
                    send(
                        &channel,
                        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"limited","update":{"sessionUpdate":"agent_message_chunk"}}}),
                        &mut output_bytes,
                    )
                };
                futures_util::future::try_join(run, driver).await?;
                Ok(())
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/acp/v2", listener.local_addr().unwrap());
        let router = agent_client_protocol_http::AcpHttpServer::new(|| ExhaustedAgent)
            .with_options(agent_client_protocol_http::ServerOptions {
                path: "/acp/v2".into(),
                ..Default::default()
            })
            .into_router();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let initialized = client.post(&endpoint).json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"info":{"name":"test","version":"1"},"capabilities":{}}})).send().await.unwrap();
        assert_eq!(initialized.status(), reqwest::StatusCode::OK);
        let id = initialized.headers()["acp-connection-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let stream = client
            .get(&endpoint)
            .header("Acp-Connection-Id", &id)
            .header("Acp-Session-Id", "limited")
            .header("Accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(stream.status(), reqwest::StatusCode::OK);
        let trigger = client.post(&endpoint).header("Acp-Connection-Id", &id).json(&json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"limited"}})).send().await.unwrap();
        assert_eq!(trigger.status(), reqwest::StatusCode::ACCEPTED);
        let body = stream.text().await.unwrap();
        let data = body
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        let notice: Value = serde_json::from_str(data).unwrap();
        assert_eq!(notice["params"]["update"]["severity"], "error");
        assert_eq!(
            notice["params"]["update"]["description"],
            OUTPUT_LIMIT_ERROR
        );
        server.abort();
        let _ = server.await;
    }
}
