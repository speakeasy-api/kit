//! ACP HTTP connection adapter. Only the resident actor owns execution.
use super::*;
use agent_client_protocol::{
    Agent, BoundedChannel, ChannelLimits, ChargedFrame, ConnectTo, TransportFrame,
};
use futures_util::StreamExt;

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
            let mut capabilities = serde_json::to_value(agentkit_acp::v2::agent_capabilities())
                .map_err(|e| Failure::unavailable(e.to_string()))?;
            if let Some(session) = capabilities["session"].as_object_mut() {
                for name in ["close", "delete", "fork", "mcp"] {
                    session.remove(name);
                }
                session.insert("list".into(), object([]));
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
        let id = message["params"]["sessionId"]
            .as_str()
            .ok_or_else(|| Failure::bad("sessionId required"))?;
        let control = self
            .controls
            .get_mut(id)
            .ok_or_else(|| Failure::conflict("resume session before controlling it"))?;
        control.send(message, Arc::clone(charge)).await?;
        Ok(None)
    }

    async fn flush(&mut self, channel: &BoundedChannel) -> agent_client_protocol::Result<()> {
        let mut failed = Vec::new();
        for (id, control) in &mut self.controls {
            match control.poll().await {
                Ok(messages) => {
                    for message in messages {
                        send(channel, message)?;
                    }
                }
                Err(error) => failed.push((id.clone(), error)),
            }
        }
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
                )?;
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
                    )?;
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
                // Retain admission through every decoded batch entry and actor await.
                // Child-bound messages share this lease until writer serialization.
                let messages = match charged_frame.decode() {
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
                    if let Some(response) = response {
                        send(&channel, response)?;
                    }
                    self.flush(&channel).await?;
                }
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
fn send(channel: &BoundedChannel, message: Value) -> agent_client_protocol::Result<()> {
    let message =
        serde_json::from_value(message).map_err(|error| channel.tx.fail(&error.to_string()))?;
    // Wake the terminal failure observer even if request setup converts this
    // SDK error to Failure. Saturation never silently drops an accepted frame.
    channel
        .tx
        .try_send(TransportFrame::Single(message))
        .map_err(|error| channel.tx.fail(&error.to_string()))
}

impl ConnectTo<agent_client_protocol::Client> for Connection {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> agent_client_protocol::Result<()> {
        let (channel, driver) = client.into_bounded_channel_and_future(ChannelLimits::default())?;
        let failure = channel.tx.failure();
        // Channel's driver can return immediately while its endpoint remains
        // live. Drive both to completion, rather than treating that as EOF.
        let running = std::pin::pin!(futures_util::future::try_join(self.run(channel), driver));
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

    #[tokio::test]
    async fn dequeued_frame_retains_admission_until_dropped() {
        let (channel, mut peer) = BoundedChannel::duplex(limits()).unwrap();
        let message = json!({"jsonrpc":"2.0","id":1,"result":{}});
        send(&channel, message.clone()).unwrap();
        let charged = peer.rx.next().await.unwrap();
        let decoded = charged.decode();
        assert!(matches!(decoded, TransportFrame::Single(_)));
        // Removing a frame from the queue does not release its reservation.
        let rejected = send(&channel, message.clone()).unwrap_err();
        assert_eq!(channel.tx.failure().await.to_string(), rejected.to_string());
        drop(decoded);
        drop(charged);
        assert!(
            send(&channel, message).is_err(),
            "saturation stays terminal"
        );
    }

    #[tokio::test]
    async fn dropping_consumed_frame_releases_current_admission() {
        let (channel, mut peer) = BoundedChannel::duplex(limits()).unwrap();
        let message = json!({"jsonrpc":"2.0","id":1,"result":{}});
        send(&channel, message.clone()).unwrap();
        let charged = peer.rx.next().await.unwrap();
        drop(charged);
        send(&channel, message).unwrap();
        assert!(peer.rx.next().await.is_some());
    }

    #[tokio::test]
    async fn healthy_cumulative_output_exceeds_former_lifetime_limit() {
        let (channel, mut peer) = BoundedChannel::duplex(ChannelLimits::default()).unwrap();
        let message = json!({"jsonrpc":"2.0","id":1,"result":{"text":"x".repeat(64 * 1024)}});
        let mut total = 0;
        while total <= MAX_REPLAY {
            send(&channel, message.clone()).unwrap();
            let charged = peer.rx.next().await.unwrap();
            total += charged.as_bytes().len();
            drop(charged);
        }
        send(&channel, message).unwrap();
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
        serde_json::from_value::<agent_client_protocol::schema::v2::UpdateSessionNotification>(
            message["params"].clone(),
        )
        .unwrap();
    }

    #[tokio::test]
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
            pending: (0..ChannelLimits::default().max_pending_requests)
                .map(|id| (id.to_string(), json!(id)))
                .collect(),
        };
        let (channel, mut peer) = BoundedChannel::duplex(limits()).unwrap();
        send(&channel, json!({"jsonrpc":"2.0","id":1,"result":{}})).unwrap();
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
        assert!(send(&channel, json!({"jsonrpc":"2.0","id":2,"result":{}})).is_err());
        drop(accepted);
    }
}
