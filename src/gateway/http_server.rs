//! ACP HTTP connection adapter. Only the resident actor owns execution.
use super::*;
use agent_client_protocol::v2::schema as wire;
use agent_client_protocol::{
    Agent, BoundedChannel, ChannelLimits, ChargedFrame, ConnectTo, TransportFrame,
};
use futures_util::StreamExt;

// One budget for a response and all controller polling/output in a drain.
// Progress does not restart it or renew leases while another controller waits.
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct Connection {
    gateway: Arc<Gateway>,
    initialize: Option<(Value, Arc<ChargedFrame>)>,
    controls: HashMap<String, Control>,
}

struct Control {
    session: String,
    attachment: String,
    entry: Entry,
    cursor: u64,
    closed: bool,
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

    async fn send(&mut self, message: Value, charge: Arc<ChargedFrame>) -> ResultValue {
        // Responses and notifications never occupy pending request capacity.
        let id = message
            .get("id")
            .filter(|_| message.get("method").is_some())
            .cloned();
        if let Some(id) = &id {
            if self.pending.contains_key(&id.to_string()) {
                return Err(Failure::conflict("request id already pending"));
            }
            if self.pending.len() >= ChannelLimits::default().max_pending_requests {
                return Err(Failure::unavailable("pending request capacity exhausted"));
            }
        }
        let result = self
            .call(Request::Send {
                session: self.session.clone(),
                attachment: self.attachment.clone(),
                message,
                charge: Some(charge),
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
        self.closed = result["closed"] == true;
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
    pub(super) fn new(gateway: Arc<Gateway>) -> Self {
        Self {
            gateway,
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
        channel: &BoundedChannel,
        charge: &Arc<ChargedFrame>,
    ) -> Result<Option<Value>, Failure> {
        let method = message["method"]
            .as_str()
            .ok_or_else(|| Failure::bad("ACP method required"))?
            .to_owned();
        if method == "initialize" {
            if self.initialize.is_some() {
                return Err(Failure::conflict("connection already initialized"));
            }
            // Use the same pinned ACP v2 schema/capabilities as the local agent.
            let mut capabilities = agentkit_acp::v2::agent_capabilities();
            capabilities
                .session
                .get_or_insert_with(wire::SessionCapabilities::new)
                .prompt
                .get_or_insert_with(wire::PromptCapabilities::new)
                .delivery = Some(wire::PromptDeliveryCapabilities::new(true, true));
            let mut capabilities = serde_json::to_value(capabilities)
                .map_err(|e| Failure::unavailable(e.to_string()))?;
            capabilities["_meta"] = object([("kit/session/status", object([]))]);
            if let Some(session) = capabilities["session"].as_object_mut() {
                for name in ["delete", "fork"] {
                    session.remove(name);
                }
                session.insert("list".into(), object([]));
                session.insert(
                    "mcp".into(),
                    object([
                        ("stdio", object([])),
                        ("http", object([])),
                        (
                            "_meta",
                            object([(
                                "kit/updateMcp",
                                object([
                                    ("method", "_kit/session/update_mcp".into()),
                                    ("upsert", true.into()),
                                ]),
                            )]),
                        ),
                    ]),
                );
            }
            self.initialize = Some((message.clone(), Arc::clone(charge)));
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
                            ("transport", "bounded-http".into()),
                            ("maxFrameBytes", 1_048_576.into()),
                            ("coreBufferedBytesPerDirection", 16_777_216.into()),
                            ("httpEgressBytesPerConnection", 4_194_304.into()),
                            ("liveReplayLimitBytes", MAX_REPLAY.into()),
                            ("liveReplayLimitEvents", (MAX_QUEUE - 3).into()),
                        ]),
                    )]),
                ),
            ])));
        }
        if self.initialize.is_none() {
            return Err(Failure::bad("initialize is required"));
        }
        if method == "$/cancel_request" {
            if message.get("id").is_some() {
                return Err(Failure::bad("request cancellation must be a notification"));
            }
            let Some(id) = message["params"]
                .get("requestId")
                .filter(|id| id.is_string() || id.as_i64().is_some() || id.as_u64().is_some())
            else {
                return Err(Failure::bad("valid cancellation requestId required"));
            };
            // Unlike session operations this notification has no sessionId.
            // Find the pending request in this connection, never by a global ID.
            let key = id.to_string();
            let mut matching = self
                .controls
                .values_mut()
                .filter(|control| control.pending.contains_key(&key));
            let Some(control) = matching.next() else {
                return Ok(None);
            };
            if matching.next().is_some() {
                return Err(Failure::conflict("ambiguous cancellation request id"));
            }
            // A late withdrawal cannot affect a replaced or exited resident.
            // Let the normal flush settle only this session's pending calls.
            match control.send(message, Arc::clone(charge)).await {
                Ok(_) | Err(Failure(_, _, "controller_replaced" | "unavailable")) => {}
                Err(error) => return Err(error),
            }
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
            if message["params"]
                .get("additionalDirectories")
                .is_some_and(|v| !v.is_null() && v != &Value::Array(vec![]))
            {
                return Err(Failure::bad("client directories are not supported"));
            }
            // ACP startup decoding skips invalid MCP entries by default.
            // Validate the list directly so forwarded configuration is never
            // silently dropped; the runtime still validates supported transports
            // and server configuration.
            if let Some(servers) = message["params"].get("mcpServers")
                && !servers.is_null()
            {
                serde_json::from_value::<Vec<agent_client_protocol::v2::schema::McpServer>>(
                    servers.clone(),
                )
                .map_err(|_| Failure::bad("invalid mcpServers configuration"))?;
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
            let entry = self
                .gateway
                .sessions
                .lock()
                .await
                .get(&session)
                .cloned()
                .ok_or_else(|| Failure::unavailable("resident child exited"))?;
            // Pin the same resident actor for claim and subsequent control.
            let (reply, accepted) = oneshot::channel();
            entry
                .sender
                .send(Envelope {
                    request: Request::Attach {
                        session: session.clone(),
                        replace: resume,
                        replay: !resume || message["params"].get("replayFrom").is_some(),
                        startup: Some(message.clone()),
                    },
                    reply,
                })
                .await
                .map_err(|_| Failure::unavailable("resident child exited"))?;
            let attached = accepted
                .await
                .map_err(|_| Failure::unavailable("resident child exited"))??;
            let attachment = attached["attachment"]
                .as_str()
                .ok_or_else(|| Failure::unavailable("missing controller id"))?
                .to_owned();
            let mut control = Control {
                session: session.clone(),
                attachment,
                entry,
                cursor: 0,
                closed: false,
                pending: HashMap::new(),
            };
            if attached["started"] == true {
                if let Some(id) = message.get("id") {
                    control.pending.insert(id.to_string(), id.clone());
                }
                self.controls.insert(session, control);
                return Ok(None);
            }
            let (mut initialize, initialize_charge) = self
                .initialize
                .clone()
                .ok_or_else(|| Failure::bad("initialize required"))?;
            initialize["id"] = Value::String(crate::session::new_id());
            let initialize_id = initialize["id"].clone();
            control.send(initialize, initialize_charge).await?;
            // Setup is serialized before the session request; the actor, not
            // this wait, owns the child request. Cancellation only drops a lease.
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    self.flush(channel)
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
            control.send(message, Arc::clone(charge)).await?;
            self.controls.insert(session, control);
            return Ok(None);
        }
        // Kit's existing background extension uses snake_case on the wire.
        let session_field = if method == "kit/background/cancel" {
            "session_id"
        } else {
            "sessionId"
        };
        let id = message["params"][session_field]
            .as_str()
            .ok_or_else(|| Failure::bad(format!("{session_field} required")))?;
        let control = self
            .controls
            .get_mut(id)
            .ok_or_else(|| Failure::conflict("resume session before controlling it"))?;
        control.send(message, Arc::clone(charge)).await?;
        Ok(None)
    }

    async fn flush(&mut self, channel: &BoundedChannel) -> agent_client_protocol::Result<()> {
        self.flush_with_response(channel, None).await
    }

    async fn flush_with_response(
        &mut self,
        channel: &BoundedChannel,
        response: Option<Value>,
    ) -> agent_client_protocol::Result<()> {
        let deadline = tokio::time::Instant::now() + OUTPUT_DRAIN_TIMEOUT;
        let drain = async {
            if let Some(response) = response {
                send(channel, response, deadline).await?;
            }
            self.flush_before(channel, deadline).await
        };
        let timeout = std::pin::pin!(tokio::time::sleep_until(deadline));
        let drain = std::pin::pin!(drain);
        // Poll the deadline first, including when actor output is already ready.
        match select(timeout, drain).await {
            Either::Left(_) => Err(channel.tx.fail("gateway HTTP output admission stalled")),
            Either::Right((result, _)) => result,
        }
    }

    async fn flush_before(
        &mut self,
        channel: &BoundedChannel,
        deadline: tokio::time::Instant,
    ) -> agent_client_protocol::Result<()> {
        let mut failed = Vec::new();
        for (id, control) in &mut self.controls {
            match control.poll().await {
                Ok(messages) => {
                    for message in messages {
                        send(channel, message, deadline).await?;
                    }
                    if control.closed {
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
                                            (
                                                "message",
                                                "session closed before request completed".into(),
                                            ),
                                        ]),
                                    ),
                                ]),
                                deadline,
                            )
                            .await?;
                        }
                    }
                }
                Err(error) => failed.push((id.clone(), error)),
            }
        }
        self.controls.retain(|_, control| !control.closed);
        // A replacement affects one session, not the multiplexed connection.
        // Settle that controller's pending calls explicitly before forgetting it.
        for (id, error) in failed {
            if let Some(mut control) = self.controls.remove(&id) {
                send(
                    channel,
                    object([
                        ("jsonrpc", "2.0".into()),
                        ("method", "session/update".into()),
                        (
                            "params",
                            object([
                                ("sessionId", id.clone().into()),
                                (
                                    "update",
                                    object([
                                        ("sessionUpdate", "_gateway_controller".into()),
                                        ("_meta", object([("kit/gateway", error.data())])),
                                    ]),
                                ),
                            ]),
                        ),
                    ]),
                    deadline,
                )
                .await?;
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
                                    ("data", error.data()),
                                ]),
                            ),
                        ]),
                        deadline,
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    async fn run(mut self, mut channel: BoundedChannel) -> agent_client_protocol::Result<()> {
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
                let Some(charged_frame) = frame else {
                    return Ok(());
                };
                let charged_frame = Arc::new(charged_frame);
                // Retain admission through the actor await and child writer handoff.
                let message = match charged_frame.decode() {
                    TransportFrame::Single(message) => message,
                    TransportFrame::Malformed { error, .. } => return Err(error),
                    _ => return Err(channel.tx.fail("unsupported ACP frame")),
                };
                let message = serde_json::to_value(message).map_err(sdk_error)?;
                let id = message.get("id").cloned();
                let response = match self.request(message, &channel, &charged_frame).await {
                    Ok(Some(result)) => id.map(|id| {
                        object([("jsonrpc", "2.0".into()), ("id", id), ("result", result)])
                    }),
                    Ok(None) => None,
                    Err(error) => {
                        // A notification has no reply ID. If actor admission
                        // rejects it, fail the transport instead of hiding
                        // accepted-but-unperformed work from the sender.
                        let Some(id) = id else {
                            return Err(channel.tx.fail(&error.1));
                        };
                        Some(object([
                            ("jsonrpc", "2.0".into()),
                            ("id", id),
                            (
                                "error",
                                object([
                                    ("code", (-32000).into()),
                                    ("data", error.data()),
                                    ("message", error.1.into()),
                                ]),
                            ),
                        ]))
                    }
                };
                self.flush_with_response(&channel, response).await?;
                drop(charged_frame);
            }
            // Drain after every inbound frame as well as idle ticks; a busy
            // client cannot starve updates or let its own controller lease lapse.
            self.flush(&channel).await?;
        }
    }
}

fn sdk_error(error: impl std::fmt::Display) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(error.to_string())
}
async fn send(
    channel: &BoundedChannel,
    message: Value,
    deadline: tokio::time::Instant,
) -> agent_client_protocol::Result<()> {
    // Ready sends must also respect the deadline; timeout_at alone can finish
    // a ready future even when its timer has already elapsed.
    if tokio::time::Instant::now() >= deadline {
        return Err(channel.tx.fail("gateway HTTP output admission stalled"));
    }
    let message =
        serde_json::from_value(message).map_err(|error| channel.tx.fail(&error.to_string()))?;
    // Wake the terminal failure observer even if request setup converts this
    // SDK error to Failure. Saturation never silently drops an accepted frame.
    // Admission stays charged through SDK routing and HTTP body handoff, not
    // merely dequeue. Wait for actual release instead of relying on scheduling.
    tokio::time::timeout_at(deadline, channel.tx.send(TransportFrame::Single(message)))
        .await
        .map_err(|_| channel.tx.fail("gateway HTTP output admission stalled"))?
        .map_err(|error| channel.tx.fail(&error.to_string()))
}

impl ConnectTo<agent_client_protocol::Client> for Connection {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> agent_client_protocol::Result<()> {
        let (channel, driver) = client.into_bounded_channel_and_future(ChannelLimits::default())?;
        let failure = channel.tx.failure();
        // A direct channel has no driver. A transport driver can also finish
        // while its endpoint remains live; neither case means channel EOF.
        let drive = async move {
            if let Some(driver) = driver {
                driver.await
            } else {
                Ok(())
            }
        };
        let running = std::pin::pin!(futures_util::future::try_join(self.run(channel), drive));
        match select(failure, running).await {
            Either::Left((error, _)) => Err(error),
            Either::Right((result, _)) => result.map(|_| ()),
        }
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

    // Single-send tests use the same production deadline path. Connection tests
    // below exercise sharing that deadline across complete output drains.
    async fn send(channel: &BoundedChannel, message: Value) -> agent_client_protocol::Result<()> {
        super::send(
            channel,
            message,
            tokio::time::Instant::now() + OUTPUT_DRAIN_TIMEOUT,
        )
        .await
    }

    fn limits() -> ChannelLimits {
        ChannelLimits {
            max_frame_bytes: 1024,
            max_buffered_bytes: 1024,
            max_buffered_frames: 1,
            ..ChannelLimits::default()
        }
    }

    #[tokio::test]
    async fn rejected_notification_terminates_instead_of_disappearing() {
        let directory = tempfile::tempdir().unwrap();
        let token = directory.path().join("token");
        std::fs::write(&token, "test-token").unwrap();
        let gateway = Arc::new(Gateway {
            stopping: AtomicBool::new(false),
            token: BearerToken::load(&token).unwrap(),
            roots: Vec::new(),
            sessions: Mutex::new(HashMap::new()),
        });
        let connection = Connection::new(gateway);
        let (peer, endpoint) = BoundedChannel::duplex(ChannelLimits::default()).unwrap();
        peer.tx
            .try_send(TransportFrame::parse_json(
                r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"missing"}}"#,
            ))
            .unwrap();
        // The endpoint's no-op driver is not EOF; the adapter processes the
        // admitted notification and exposes its rejection as transport failure.
        let error = tokio::time::timeout(Duration::from_secs(5), connection.connect_to(endpoint))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("initialize is required"));
    }

    #[tokio::test(start_paused = true)]
    async fn dequeued_frame_retains_admission_until_dropped() {
        let (channel, mut peer) = BoundedChannel::duplex(limits()).unwrap();
        let message = json!({"jsonrpc":"2.0","id":1,"result":{}});
        send(&channel, message.clone()).await.unwrap();
        let charged = peer.rx.next().await.unwrap();
        let decoded = charged.decode();
        assert!(matches!(decoded, TransportFrame::Single(_)));
        // Removing a frame from the queue does not release its reservation.
        let rejected = send(&channel, message.clone()).await.unwrap_err();
        assert!(rejected.to_string().contains("output admission stalled"));
        assert_eq!(channel.tx.failure().await.to_string(), rejected.to_string());
        drop(decoded);
        drop(charged);
        assert!(
            send(&channel, message).await.is_err(),
            "saturation stays terminal"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn output_waits_for_http_body_charge_release_before_stall_deadline() {
        let (channel, mut peer) = BoundedChannel::duplex(limits()).unwrap();
        let message = json!({"jsonrpc":"2.0","id":1,"result":{}});
        send(&channel, message.clone()).await.unwrap();
        // HTTP routing/body ownership can retain a dequeued frame. Progress is
        // tied to that ownership ending, not a producer yield or queue dequeue.
        let held = Arc::new(peer.rx.next().await.unwrap());
        let body = held.clone();
        drop(held);
        let mut sending = Box::pin(send(&channel, message));
        assert!(futures_util::poll!(&mut sending).is_pending());
        tokio::time::advance(OUTPUT_DRAIN_TIMEOUT / 2).await;
        assert!(futures_util::poll!(&mut sending).is_pending());
        drop(body);
        sending.await.unwrap();
        assert!(peer.rx.next().await.is_some());
    }

    #[tokio::test]
    async fn cancelled_output_wait_releases_registration_without_failing_channel() {
        let (channel, mut peer) = BoundedChannel::duplex(limits()).unwrap();
        let message = json!({"jsonrpc":"2.0","id":1,"result":{}});
        send(&channel, message.clone()).await.unwrap();
        let held = peer.rx.next().await.unwrap();
        let mut sending = Box::pin(send(&channel, message.clone()));
        assert!(futures_util::poll!(&mut sending).is_pending());
        drop(sending);
        drop(held);
        send(&channel, message).await.unwrap();
        assert!(peer.rx.next().await.is_some());
    }

    #[tokio::test]
    async fn dropping_consumed_frame_releases_current_admission() {
        let (channel, mut peer) = BoundedChannel::duplex(limits()).unwrap();
        let message = json!({"jsonrpc":"2.0","id":1,"result":{}});
        send(&channel, message.clone()).await.unwrap();
        let charged = peer.rx.next().await.unwrap();
        drop(charged);
        send(&channel, message).await.unwrap();
        assert!(peer.rx.next().await.is_some());
    }

    #[tokio::test]
    async fn healthy_cumulative_output_exceeds_former_lifetime_limit() {
        let (channel, mut peer) = BoundedChannel::duplex(ChannelLimits::default()).unwrap();
        let message = json!({"jsonrpc":"2.0","id":1,"result":{"text":"x".repeat(64 * 1024)}});
        let mut total = 0;
        while total <= MAX_REPLAY {
            send(&channel, message.clone()).await.unwrap();
            let charged = peer.rx.next().await.unwrap();
            total += charged.as_bytes().len();
            drop(charged);
        }
        send(&channel, message).await.unwrap();
    }

    async fn assert_slow_progress_has_one_drain_deadline(
        batches: &[usize],
        poll_delay: Duration,
        with_response: bool,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let token = directory.path().join("token");
        std::fs::write(&token, "test-token").unwrap();
        let gateway = Arc::new(Gateway {
            stopping: AtomicBool::new(false),
            token: BearerToken::load(&token).unwrap(),
            roots: Vec::new(),
            sessions: Mutex::new(HashMap::new()),
        });
        let mut connection = Connection::new(gateway);
        let mut actors = Vec::new();
        for (index, count) in batches.iter().copied().enumerate() {
            let session = format!("session-{index}");
            let (sender, mut receiver) = mpsc::channel::<Envelope>(2);
            let (_stopped, stopped) = watch::channel(());
            connection.controls.insert(
                session.clone(),
                Control {
                    session,
                    attachment: "current".into(),
                    entry: Entry {
                        root: PathBuf::new(),
                        sender,
                        stopped,
                    },
                    cursor: 0,
                    closed: false,
                    pending: HashMap::new(),
                },
            );
            // The resident actor mailbox is the real adapter boundary. Each
            // controller contributes a batch to the same HTTP output channel.
            actors.push(tokio::spawn(async move {
                let envelope = receiver.recv().await.unwrap();
                assert!(matches!(envelope.request, Request::Poll { .. }));
                tokio::time::sleep(poll_delay).await;
                let events = (1..=count)
                    .map(|cursor| {
                        json!({
                            "cursor":cursor,
                            "message":{"jsonrpc":"2.0","id":cursor,"result":{}}
                        })
                    })
                    .collect::<Vec<_>>();
                let _ = envelope.reply.send(Ok(json!({"events":events})));
            }));
        }
        let (channel, mut peer) = BoundedChannel::duplex(limits()).unwrap();
        let response = with_response.then(|| json!({"jsonrpc":"2.0","id":0,"result":{}}));
        if with_response {
            // A prior HTTP body owns the only slot when the response arrives.
            send(&channel, json!({"jsonrpc":"2.0","id":-1,"result":{}}))
                .await
                .unwrap();
        }
        let started = tokio::time::Instant::now();
        let output = async {
            let result = if response.is_some() {
                connection.flush_with_response(&channel, response).await
            } else {
                connection.flush(&channel).await
            };
            channel.tx.close_channel();
            result
        };
        let consumer = async {
            while let Some(charged) = peer.rx.next().await {
                // Every individual frame makes progress well before five
                // seconds, but the complete batch exceeds the drain budget.
                tokio::time::sleep(Duration::from_secs(1)).await;
                drop(charged);
            }
        };
        let (result, ()) = tokio::join!(output, consumer);
        for actor in actors {
            actor.abort();
            let _ = actor.await;
        }
        let error = result.expect_err("slow progress reset the output drain deadline");
        assert!(
            error.to_string().contains("output admission stalled"),
            "{error}"
        );
        assert!(started.elapsed() <= OUTPUT_DRAIN_TIMEOUT + Duration::from_secs(1));
        assert!(started.elapsed() < LEASE);
        assert_eq!(
            connection.controls.len(),
            batches.len(),
            "drain timeout must not be reported as controller replacement"
        );
        assert_eq!(channel.tx.failure().await.to_string(), error.to_string());
    }

    #[tokio::test(start_paused = true)]
    async fn slow_progress_cannot_extend_one_controller_output_drain() {
        assert_slow_progress_has_one_drain_deadline(&[128], Duration::ZERO, false).await;
    }

    #[tokio::test(start_paused = true)]
    async fn slow_progress_cannot_reset_output_drain_between_controllers() {
        // Each controller fits individually inside the budget. Only a shared
        // deadline across the multiplexed connection bounds the whole drain.
        assert_slow_progress_has_one_drain_deadline(&[4, 4], Duration::ZERO, false).await;
    }

    #[tokio::test(start_paused = true)]
    async fn slow_progress_response_and_replay_share_output_drain_deadline() {
        assert_slow_progress_has_one_drain_deadline(&[4], Duration::ZERO, true).await;
    }

    #[tokio::test(start_paused = true)]
    async fn slow_actor_polling_consumes_the_shared_output_drain_budget() {
        assert_slow_progress_has_one_drain_deadline(&[0, 0, 0], Duration::from_secs(2), false)
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn ready_output_cannot_bypass_expired_drain_deadline() {
        let (channel, mut peer) = BoundedChannel::duplex(limits()).unwrap();
        let error = super::send(
            &channel,
            json!({"jsonrpc":"2.0","id":1,"result":{}}),
            tokio::time::Instant::now(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("output admission stalled"));
        assert!(peer.rx.next().await.is_none());
    }

    #[tokio::test]
    async fn revoked_idle_controller_emits_terminal_notice_without_pending_calls() {
        let directory = tempfile::tempdir().unwrap();
        let token = directory.path().join("token");
        std::fs::write(&token, "test-token").unwrap();
        let gateway = Arc::new(Gateway {
            stopping: AtomicBool::new(false),
            token: BearerToken::load(&token).unwrap(),
            roots: Vec::new(),
            sessions: Mutex::new(HashMap::new()),
        });
        let (sender, mut receiver) = mpsc::channel::<Envelope>(2);
        let (_stopped, stopped) = watch::channel(());
        let mut connection = Connection::new(gateway);
        connection.controls.insert(
            "session".into(),
            Control {
                session: "session".into(),
                attachment: "old".into(),
                entry: Entry {
                    root: PathBuf::new(),
                    sender,
                    stopped,
                },
                cursor: 0,
                closed: false,
                pending: HashMap::new(),
            },
        );
        let actor = async {
            let envelope = receiver.recv().await.unwrap();
            assert!(matches!(envelope.request, Request::Poll { .. }));
            envelope
                .reply
                .send(Err(Failure(
                    StatusCode::CONFLICT,
                    "replaced".into(),
                    "controller_replaced",
                )))
                .unwrap();
        };
        let (channel, mut peer) = BoundedChannel::duplex(ChannelLimits::default()).unwrap();
        let (result, ()) = tokio::join!(connection.flush(&channel), actor);
        result.unwrap();
        assert!(connection.controls.is_empty());
        let frame = peer.rx.next().await.unwrap();
        let TransportFrame::Single(message) = frame.decode() else {
            panic!("expected notification")
        };
        let message = serde_json::to_value(message).unwrap();
        assert_eq!(
            message["params"]["update"]["_meta"]["kit/gateway"],
            json!({"reason":"controller_replaced","terminal":true})
        );
        serde_json::from_value::<agent_client_protocol::v2::schema::UpdateSessionNotification>(
            message["params"].clone(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn stop_with_pending_withdrawal_keeps_other_sessions_connected() {
        use super::super::tests::{isolated_actor, resident_claim};

        let directory = tempfile::tempdir().unwrap();
        let token = directory.path().join("token");
        std::fs::write(&token, "test-token").unwrap();
        let gateway = Arc::new(Gateway {
            stopping: AtomicBool::new(false),
            token: BearerToken::load(&token).unwrap(),
            roots: Vec::new(),
            sessions: Mutex::new(HashMap::new()),
        });
        let mut connection = Connection::new(gateway);
        let [resident, unrelated] = ["resident", "unrelated"].map(|session| {
            let mut actor = isolated_actor();
            actor.id = session.into();
            actor.session_result = Some(json!({"configOptions":[]}));
            let (writes, mut writer) = mpsc::channel(1);
            let attached = actor.command(resident_claim(&actor, false, 60), &writes).unwrap();
            let (sender, mut receiver) = mpsc::channel::<Envelope>(2);
            let (stopped_tx, stopped) = watch::channel(());
            connection.controls.insert(session.into(), Control {
                session: session.into(),
                attachment: attached["attachment"].as_str().unwrap().into(),
                entry: Entry { root: actor.root.clone(), sender, stopped },
                cursor: actor.attachment.as_ref().unwrap().next,
                closed: false,
                pending: HashMap::new(),
            });
            async move {
                let _stopped = stopped_tx;
                while let Some(envelope) = receiver.recv().await {
                    let method = match &envelope.request {
                        Request::Send { message, .. } => message["method"].as_str().map(str::to_owned),
                        _ => None,
                    };
                    // Emulate the child writer freeing exactly one slot after
                    // the actor has selected Stop rather than the retry future.
                    let held = (method.as_deref() == Some("session/cancel"))
                        .then(|| writer.try_recv().unwrap().message);
                    let result = actor.command(envelope.request, &writes);
                    if let Some(held) = held {
                        if result.is_ok() {
                            let item = writer.try_recv().unwrap();
                            let mut bytes = Vec::new();
                            item.write_to(&mut bytes).await.unwrap();
                            let messages = std::str::from_utf8(&bytes).unwrap().lines()
                                .map(|line| serde_json::from_str::<Value>(line).unwrap()).collect::<Vec<_>>();
                            assert_eq!(messages, [
                                json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":held["id"]}}),
                                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}),
                            ]);
                            assert!(writer.try_recv().is_err());
                            actor.output(json!({"jsonrpc":"2.0","id":held["id"],"result":{"stopReason":"cancelled"}}));
                            assert!(actor.pending.is_empty(), "rejected prompt was tracked");
                        }
                    } else if session == "unrelated" && method.as_deref() == Some("session/prompt") {
                        let prompt = writer.try_recv().unwrap().message;
                        actor.output(json!({"jsonrpc":"2.0","id":prompt["id"],"result":{"stopReason":"end_turn"}}));
                    }
                    let _ = envelope.reply.send(result);
                }
            }
        });
        let (channel, mut peer) = BoundedChannel::duplex(ChannelLimits::default()).unwrap();
        let prompt = |id, session| json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{"sessionId":session,"delivery":"steer","prompt":[]}});
        let client = async move {
            for message in [
                json!({"jsonrpc":"2.0","method":"initialize","id":1,"params":{}}),
                prompt(2, "resident"),
                json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":2}}),
                prompt(3, "resident"),
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"resident"}}),
                prompt(4, "unrelated"),
            ] {
                send(&peer, message).await.unwrap();
            }
            let mut responses = HashMap::new();
            while responses.len() < 4 {
                let frame = peer
                    .rx
                    .next()
                    .await
                    .expect("Stop closed the entire connection");
                let TransportFrame::Single(message) = frame.decode() else {
                    panic!("expected single message");
                };
                let message = serde_json::to_value(message).unwrap();
                if let Some(id) = message["id"].as_u64() {
                    assert!(responses.insert(id, message).is_none());
                }
            }
            assert_eq!(responses[&2]["result"]["stopReason"], "cancelled");
            assert_eq!(responses[&3]["error"]["data"]["reason"], "unavailable");
            assert_eq!(responses[&4]["result"]["stopReason"], "end_turn");
        };
        let (result, (), (), ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(connection.run(channel), resident, unrelated, client)
        })
        .await
        .unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn stale_withdrawal_does_not_disconnect_another_session() {
        assert_late_withdrawal_is_session_local(false, "controller_replaced").await;
    }

    #[tokio::test]
    async fn resident_exit_during_withdrawal_does_not_disconnect_another_session() {
        assert_late_withdrawal_is_session_local(true, "unavailable").await;
    }

    async fn assert_late_withdrawal_is_session_local(exited: bool, reason: &str) {
        let directory = tempfile::tempdir().unwrap();
        let token = directory.path().join("token");
        std::fs::write(&token, "test-token").unwrap();
        let gateway = Arc::new(Gateway {
            stopping: AtomicBool::new(false),
            token: BearerToken::load(&token).unwrap(),
            roots: Vec::new(),
            sessions: Mutex::new(HashMap::new()),
        });
        let (mut channel, mut peer) = BoundedChannel::duplex(ChannelLimits::default()).unwrap();
        send(
            &peer,
            json!({"jsonrpc":"2.0","method":"initialize","id":1,"params":{}}),
        )
        .await
        .unwrap();
        let charge = Arc::new(channel.rx.next().await.unwrap());
        let mut connection = Connection::new(gateway);
        connection
            .request(
                json!({"jsonrpc":"2.0","method":"initialize","id":1,"params":{}}),
                &channel,
                &charge,
            )
            .await
            .unwrap();

        let [mut replaced, mut active] = ["replaced", "active"].map(|session| {
            let (sender, receiver) = mpsc::channel::<Envelope>(2);
            let (_stopped, stopped) = watch::channel(());
            connection.controls.insert(
                session.into(),
                Control {
                    session: session.into(),
                    attachment: format!("{session}-attachment"),
                    entry: Entry {
                        root: PathBuf::new(),
                        sender,
                        stopped,
                    },
                    cursor: 0,
                    closed: false,
                    pending: HashMap::from([(json!(session).to_string(), json!(session))]),
                },
            );
            receiver
        });
        // Queue the late notification before the adapter can poll either actor.
        send(
            &peer,
            json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":"replaced"}}),
        )
        .await
        .unwrap();

        let replaced_actor = async move {
            if exited {
                return;
            }
            let withdrawal = replaced.recv().await.unwrap();
            assert!(
                matches!(&withdrawal.request, Request::Send { attachment, message, .. }
                if attachment == "replaced-attachment" && message["method"] == "$/cancel_request")
            );
            withdrawal
                .reply
                .send(Err(Failure(
                    StatusCode::CONFLICT,
                    "replaced".into(),
                    "controller_replaced",
                )))
                .unwrap();
            let poll = replaced.recv().await.unwrap();
            assert!(matches!(poll.request, Request::Poll { .. }));
            poll.reply
                .send(Err(Failure(
                    StatusCode::CONFLICT,
                    "replaced".into(),
                    "controller_replaced",
                )))
                .unwrap();
        };
        let active_actor = async {
            while let Some(envelope) = active.recv().await {
                match envelope.request {
                    Request::Poll { cursor, .. } => {
                        let events = if cursor == 0 {
                            json!([{"cursor":1,"message":{"jsonrpc":"2.0","id":"active","result":{}}}])
                        } else {
                            json!([])
                        };
                        let _ = envelope.reply.send(Ok(json!({"events": events})));
                    }
                    Request::Detach { .. } => break,
                    _ => panic!("unexpected operation for the active session"),
                }
            }
        };
        let client = async {
            let mut responses = HashMap::new();
            for _ in 0..3 {
                let frame = peer
                    .rx
                    .next()
                    .await
                    .expect("connection closed before pending calls settled");
                let TransportFrame::Single(message) = frame.decode() else {
                    panic!("expected single message")
                };
                let message = serde_json::to_value(message).unwrap();
                if message["method"] == "session/update" {
                    assert_eq!(message["params"]["sessionId"], "replaced");
                } else {
                    responses.insert(message["id"].as_str().unwrap().to_owned(), message);
                }
            }
            assert_eq!(responses["replaced"]["error"]["data"]["reason"], reason);
            assert_eq!(responses["active"]["result"], json!({}));
            drop(peer);
        };
        let (result, (), (), ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                connection.run(channel),
                replaced_actor,
                active_actor,
                client
            )
        })
        .await
        .unwrap();
        result.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn pending_request_capacity_preserves_response_lane() {
        let (sender, mut receiver) = mpsc::channel::<Envelope>(1);
        let (_stopped, stopped) = watch::channel(());
        let mut control = Control {
            session: "session".into(),
            attachment: "attachment".into(),
            entry: Entry {
                root: PathBuf::new(),
                sender,
                stopped,
            },
            cursor: 0,
            closed: false,
            pending: (0..ChannelLimits::default().max_pending_requests)
                .map(|id| (id.to_string(), json!(id)))
                .collect(),
        };
        let (channel, mut peer) = BoundedChannel::duplex(limits()).unwrap();
        send(&channel, json!({"jsonrpc":"2.0","id":1,"result":{}}))
            .await
            .unwrap();
        let charge = Arc::new(peer.rx.next().await.unwrap());
        let pending = control.pending.len();
        assert!(
            control
                .send(
                    json!({"jsonrpc":"2.0","id":"overflow","method":"session/prompt"}),
                    Arc::clone(&charge)
                )
                .await
                .is_err()
        );
        assert!(receiver.try_recv().is_err());
        // The fake is the actor mailbox boundary; no production instrumentation.
        let actor = async {
            let envelope = receiver.recv().await.unwrap();
            assert!(matches!(
                envelope.request,
                Request::Send {
                    charge: Some(_),
                    ..
                }
            ));
            envelope.reply.send(Ok(json!({}))).unwrap();
            envelope.request
        };
        let response = control.send(
            json!({"jsonrpc":"2.0","id":"permission","result":{}}),
            Arc::clone(&charge),
        );
        let (result, accepted) = tokio::join!(response, actor);
        result.unwrap();
        assert_eq!(control.pending.len(), pending);
        drop(charge);
        // Actor acceptance does not release SDK admission while the accepted
        // child-bound request still owns decoded data.
        assert!(
            send(&channel, json!({"jsonrpc":"2.0","id":2,"result":{}}))
                .await
                .is_err()
        );
        drop(accepted);
    }
}
