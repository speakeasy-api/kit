//! Connection-local admission and ownership for interactive forms.
//!
//! The existing session-route mutex is read only here. Its writers are startup,
//! transition_route and gateway recovery; we clone a complete route while locked
//! and never await, send, or drop user-owned state under that guard. Generation
//! filtering rejects a request racing a session transition. App::start_session
//! drops already-visible forms. No answer can be redirected to a new session.
use std::sync::{Arc, Mutex, OnceLock};

use agent_client_protocol::{Responder, schema::v2 as wire};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use super::{ActiveSessionRoute, QueuedUpdate, app::Update, elicitation::Form};

const MAX_PENDING: usize = 8;

// Preserve the original schema: ACP's typed schemas intentionally discard
// unknown keywords, which is not safe for a form validator.
#[derive(
    Clone, Debug, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcRequest,
)]
#[serde(transparent)]
#[request(method = "elicitation/create", response = wire::CreateElicitationResponse)]
pub(super) struct RawRequest(pub serde_json::Value);

#[derive(Debug)]
pub(super) struct Pending {
    pub form: Form,
    pub reply: oneshot::Sender<wire::CreateElicitationResponse>,
    // Admission follows queued/visible ownership, not the handler future. A
    // cancelled peer cannot flood the unbounded update channel with dead forms.
    _permit: OwnedSemaphorePermit,
}

pub(super) struct Ingress {
    updates: mpsc::UnboundedSender<QueuedUpdate>,
    route: Arc<Mutex<ActiveSessionRoute>>,
    capacity: Arc<Semaphore>,
    pub origin: Arc<OnceLock<String>>,
}

impl Ingress {
    pub fn new(
        updates: mpsc::UnboundedSender<QueuedUpdate>,
        route: Arc<Mutex<ActiveSessionRoute>>,
    ) -> Self {
        Self {
            updates,
            route,
            capacity: Arc::new(Semaphore::new(MAX_PENDING)),
            origin: Arc::new(OnceLock::new()),
        }
    }

    fn prepare(
        &self,
        request: RawRequest,
    ) -> Result<
        (
            QueuedUpdate,
            oneshot::Receiver<wire::CreateElicitationResponse>,
        ),
        String,
    > {
        if request.0["mode"] != "form" {
            return Err("only form elicitation is supported".into());
        }
        let session = request.0["sessionId"]
            .as_str()
            .ok_or("form elicitation requires an active session")?;
        if request.0.get("requestId").is_some() {
            return Err("ambiguous elicitation scope".into());
        }
        let route = self
            .route
            .lock()
            .map_err(|_| "session routing unavailable")?
            .clone();
        if route.id.is_empty() || route.id != session {
            return Err("elicitation does not belong to the active session".into());
        }
        let _origin = self
            .origin
            .get()
            .ok_or("agent initialization is incomplete")?;
        let permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| "too many pending user requests")?;
        // Validate the original wire budget before adding local presentation text.
        let form = Form::from_value(request.0)?;
        let (reply, receiver) = oneshot::channel();
        let pending = Pending {
            form,
            reply,
            _permit: permit,
        };
        Ok((
            QueuedUpdate::for_session(route.generation, Update::Elicitation(pending)),
            receiver,
        ))
    }

    // ACP dispatch callbacks must return immediately: awaiting user input here
    // would prevent the same connection from dispatching cancellation/updates.
    pub fn dispatch(
        self: &Arc<Self>,
        request: RawRequest,
        responder: Responder<wire::CreateElicitationResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        let (update, receiver) = match self.prepare(request) {
            Ok(pending) => pending,
            Err(message) => {
                return responder.respond_with_error(
                    agent_client_protocol::Error::invalid_params().data(message),
                );
            }
        };
        let generation = update.generation;
        if self.updates.send(update).is_err() {
            return responder.respond(wire::CreateElicitationResponse::new(
                wire::ElicitationAction::Cancel,
            ));
        }
        let ingress = Arc::clone(self);
        tokio::spawn(async move {
            let _ = ingress.answer(receiver, generation, responder).await;
        });
        Ok(())
    }

    async fn answer(
        &self,
        receiver: oneshot::Receiver<wire::CreateElicitationResponse>,
        generation: Option<u64>,
        responder: Responder<wire::CreateElicitationResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        let cancellation = responder.cancellation();
        let response = {
            let cancelled = std::pin::pin!(cancellation.cancelled());
            match futures_util::future::select(cancelled, receiver).await {
                futures_util::future::Either::Left(_) => {
                    wire::CreateElicitationResponse::new(wire::ElicitationAction::Cancel)
                }
                futures_util::future::Either::Right((response, _)) => {
                    response.unwrap_or_else(|_| {
                        wire::CreateElicitationResponse::new(wire::ElicitationAction::Cancel)
                    })
                }
            }
        };
        // The response decision is fenced against recovery/session transitions.
        // The route guard is released before invoking the responder or waking
        // transport work. A transition after this snapshot follows this decision.
        let current = self
            .route
            .lock()
            .is_ok_and(|route| Some(route.generation) == generation);
        let response = if current {
            response
        } else {
            wire::CreateElicitationResponse::new(wire::ElicitationAction::Cancel)
        };
        responder.respond(response)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_macros, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::tui::{accept_queued_update, app::App, transition_route};

    fn request(session: &str) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "mode": "form", "sessionId": session, "message": "Choose a scope",
            "requestedSchema": {"type":"object", "properties": {"scope": {"type":"string", "enum":["small","large"]}}, "required":["scope"]}
        })).unwrap()
    }

    fn ingress() -> Ingress {
        let (sender, _) = mpsc::unbounded_channel();
        let ingress = Ingress::new(
            sender,
            Arc::new(Mutex::new(ActiveSessionRoute {
                id: "session".into(),
                generation: 1,
            })),
        );
        ingress.origin.set("test-agent".into()).unwrap();
        ingress
    }

    fn app() -> App {
        let mut app = App::new(".".into(), "test".into(), "test".into(), String::new());
        app.start_session("session".into());
        app
    }

    #[test]
    fn elicitation_wire_budget_excludes_local_origin_label() {
        let ingress = ingress();
        let mut request = request("session");
        request.0["_meta"] = serde_json::json!({"padding": vec!["x".repeat(4096); 32]});
        let size = serde_json::to_vec(&request.0).unwrap().len();
        let excess = size - 128 * 1024;
        request.0["_meta"]["padding"][31] = serde_json::Value::String("x".repeat(4096 - excess));
        assert_eq!(serde_json::to_vec(&request.0).unwrap().len(), 128 * 1024);
        assert!(ingress.prepare(RawRequest(request.0.clone())).is_ok());
        request.0["_meta"]["padding"][31] = serde_json::Value::String("x".repeat(4097 - excess));
        assert!(ingress.prepare(request).is_err());
    }

    #[test]
    fn elicitation_clipboard_shortcuts_are_text_only_and_fenced_to_field() {
        use crate::tui::app::{Action, ClipboardRoute};
        use crate::tui::{ClipboardResult, apply_clipboard_completion, handle_inner};
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let ingress = ingress();
        let mut raw = request("session");
        raw.0["requestedSchema"] = serde_json::json!({"type":"object","properties":{
            "first":{"type":"string"},"second":{"type":"string"}
        }});
        let (queued, _receiver) = ingress.prepare(raw.clone()).unwrap();
        let mut app = app();
        app.editor.insert_str("parked draft");
        app.apply(queued.update);
        for key in [
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Insert, KeyModifiers::SHIFT),
        ] {
            let Action::ReadClipboard(route, _) = handle_inner(&mut app, Event::Key(key)) else {
                panic!("expected clipboard request");
            };
            assert!(matches!(route, ClipboardRoute::Elicitation { .. }));
            assert_eq!(
                apply_clipboard_completion(
                    &mut app,
                    &ingress.route,
                    1,
                    route,
                    ClipboardResult::Text("pasted ".into())
                ),
                Some(true)
            );
        }
        let stale = app.clipboard_route();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            apply_clipboard_completion(
                &mut app,
                &ingress.route,
                1,
                stale,
                ClipboardResult::Text("wrong field".into())
            ),
            None
        );
        let stale = app.clipboard_route();
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(
            apply_clipboard_completion(
                &mut app,
                &ingress.route,
                1,
                stale,
                ClipboardResult::Text("after edit".into())
            ),
            None
        );
        let old_form = app.clipboard_route();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let (queued, _next_receiver) = ingress.prepare(raw).unwrap();
        app.apply(queued.update);
        assert_eq!(
            apply_clipboard_completion(
                &mut app,
                &ingress.route,
                1,
                old_form,
                ClipboardResult::Text("wrong form".into())
            ),
            None
        );
        assert_eq!(app.editor.text(), "parked draft");
    }

    #[test]
    fn elicitation_panel_keeps_conversation_visible_without_generated_chrome() {
        let ingress = ingress();
        let mut request = request("session");
        request.0["requestedSchema"] = serde_json::json!({"type":"object","properties":{
            "question_0":{"type":"string","title":"Scope","default":"Small patch"},
            "question_1":{"type":"string","default":"Notes"}
        }});
        let (queued, _receiver) = ingress.prepare(request).unwrap();
        let mut app = app();
        app.blocks.push(crate::tui::app::Block::Agent(
            "Conversation context remains visible".into(),
        ));
        app.editor.insert_str("parked draft");
        app.apply(queued.update);
        let mut images = crate::tui::image::ImageRuntime::disabled();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 40)).unwrap();
        terminal
            .draw(|frame| crate::tui::ui::draw(frame, &mut app, &mut images))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Conversation context remains visible"));
        assert!(screen.contains("Choose a scope"));
        assert!(!screen.contains("parked draft"));
        assert!(!screen.contains("Agent request"));
        assert!(!screen.contains("Request from"));
        assert!(!screen.contains("question_0"));
        assert!(!crate::tui::ui::native_links_obscured(&app));
        assert!(
            app.viewport >= 18,
            "conversation must retain at least half the available height"
        );
        for _ in 0..2 {
            app.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        terminal
            .draw(|frame| crate::tui::ui::draw(frame, &mut app, &mut images))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Scope: \"Small patch\""));
        assert!(screen.contains("Answer 2: \"Notes\""));
        assert!(!screen.contains("question_0"));
        assert!(!screen.contains("question_1"));
        assert!(screen.contains("Conversation context remains visible"));
        for (width, height) in [(1, 1), (8, 3), (30, 10)] {
            let mut tiny =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            tiny.draw(|frame| crate::tui::ui::draw(frame, &mut app, &mut images))
                .unwrap();
        }
    }

    #[test]
    fn elicitation_admission_is_bounded_until_queued_forms_are_dropped() {
        let ingress = ingress();
        let queued: Vec<_> = (0..MAX_PENDING)
            .map(|_| ingress.prepare(request("session")).unwrap())
            .collect();
        assert!(ingress.prepare(request("session")).is_err());
        let queued: Vec<_> = queued
            .into_iter()
            .map(|(update, receiver)| {
                drop(receiver);
                update
            })
            .collect();
        assert!(ingress.prepare(request("session")).is_err());
        drop(queued);
        assert!(ingress.prepare(request("session")).is_ok());
    }

    #[test]
    fn elicitation_rejects_wrong_session_and_stale_generation() {
        let ingress = ingress();
        assert!(ingress.prepare(request("other")).is_err());
        let (queued, mut receiver) = ingress.prepare(request("session")).unwrap();
        transition_route(&ingress.route, "other".into());
        assert!(accept_queued_update(&ingress.route, queued).is_none());
        assert!(matches!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn elicitation_session_switch_and_process_exit_close_pending_answers() {
        let ingress = ingress();
        let mut app = app();
        let (queued, mut receiver) = ingress.prepare(request("session")).unwrap();
        app.apply(queued.update);
        app.start_session("other".into());
        assert!(app.elicitations.is_empty());
        assert!(matches!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        let (queued, mut receiver) = ingress.prepare(request("session")).unwrap();
        app.apply(queued.update);
        app.apply(Update::ProcessExited("disconnected".into()));
        assert!(app.elicitations.is_empty());
        assert!(matches!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn elicitation_cancellation_prunes_only_closed_request() {
        let ingress = ingress();
        let mut app = app();
        let (first, first_answer) = ingress.prepare(request("session")).unwrap();
        let (second, _second_answer) = ingress.prepare(request("session")).unwrap();
        app.apply(first.update);
        app.apply(second.update);
        drop(first_answer);
        assert!(app.tick());
        assert_eq!(app.elicitations.len(), 1);
    }

    #[tokio::test]
    async fn elicitation_roundtrip_and_individual_cancellation_over_acp() {
        use agent_client_protocol::Channel;
        let exercise = async {
            let (updates, mut incoming) = mpsc::unbounded_channel();
            let ingress = Ingress::new(
                updates,
                Arc::new(Mutex::new(ActiveSessionRoute {
                    id: "session".into(),
                    generation: 1,
                })),
            );
            ingress.origin.set("test-agent".into()).unwrap();
            let route = Arc::clone(&ingress.route);
            let ingress = Arc::new(ingress);
            let client = agent_client_protocol::Client.v2().on_receive_request(
                async move |request: RawRequest, responder, _cx| {
                    ingress.dispatch(request, responder)
                },
                agent_client_protocol::on_receive_request!(),
            );
            let (client_transport, agent_transport) = Channel::duplex();
            let (ready_tx, ready_rx) = oneshot::channel();
            let (done_tx, done_rx) = oneshot::channel();
            let client_task = tokio::spawn(async move {
                client
                    .connect_with(client_transport, async move |connection| {
                        connection
                            .send_request(wire::InitializeRequest::new(
                                agent_client_protocol::schema::ProtocolVersion::V2,
                                wire::Implementation::new("test-client", "0"),
                            ))
                            .block_task()
                            .await?;
                        let _ = ready_tx.send(());
                        let _ = done_rx.await;
                        Ok::<_, agent_client_protocol::Error>(())
                    })
                    .await
            });
            agent_client_protocol::Agent
                .v2()
                .on_receive_request(
                    async move |_request: wire::InitializeRequest, responder, _cx| {
                        responder.respond(wire::InitializeResponse::new(
                            agent_client_protocol::schema::ProtocolVersion::V2,
                            wire::Implementation::new("test-agent", "0"),
                        ))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(agent_transport, async move |connection| {
                    ready_rx.await.unwrap();
                    let sent = connection.send_request(request("session"));
                    let update = incoming.recv().await.unwrap();
                    let Update::Elicitation(pending) = update.update else {
                        panic!("expected form")
                    };
                    pending
                        .reply
                        .send(wire::CreateElicitationResponse::new(
                            wire::ElicitationAction::Decline,
                        ))
                        .unwrap();
                    assert!(matches!(
                        sent.block_task().await?.action,
                        wire::ElicitationAction::Decline
                    ));

                    let sent = connection.send_request(request("session"));
                    let update = incoming.recv().await.unwrap();
                    let Update::Elicitation(pending) = update.update else {
                        panic!("expected form")
                    };
                    sent.cancel()?;
                    assert!(matches!(
                        sent.block_task().await?.action,
                        wire::ElicitationAction::Cancel
                    ));
                    assert!(pending.reply.is_closed());

                    let sent = connection.send_request(request("session"));
                    let update = incoming.recv().await.unwrap();
                    let Update::Elicitation(pending) = update.update else {
                        panic!("expected form")
                    };
                    transition_route(&route, "replacement".into());
                    pending
                        .reply
                        .send(wire::CreateElicitationResponse::new(
                            wire::ElicitationAction::Decline,
                        ))
                        .unwrap();
                    assert!(matches!(
                        sent.block_task().await?.action,
                        wire::ElicitationAction::Cancel
                    ));
                    Ok::<_, agent_client_protocol::Error>(())
                })
                .await
                .unwrap();
            let _ = done_tx.send(());
            client_task.await.unwrap().unwrap();
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), exercise)
            .await
            .expect("ACP roundtrip stalled");
    }

    #[tokio::test]
    async fn ask_user_tool_roundtrips_through_tui_multiselect_custom_and_review() {
        use agent_client_protocol::{Channel, schema::ProtocolVersion};
        use agentkit_core::{CancellationController, MetadataMap, SessionId, ToolOutput, TurnId};
        use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext, Tool, ToolRequest};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use serde_json::json;

        fn screen(app: &mut App) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            terminal
                .draw(|frame| app.elicitations.front_mut().unwrap().form.render(frame))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect()
        }
        fn press(app: &mut App, key: KeyCode) {
            assert!(matches!(
                app.handle_key(KeyEvent::new(key, KeyModifiers::NONE)),
                crate::tui::app::Action::Redraw
            ));
        }

        let exercise = async {
            let (updates, mut incoming) = mpsc::unbounded_channel();
            let ingress = Arc::new(Ingress::new(
                updates,
                Arc::new(Mutex::new(ActiveSessionRoute {
                    id: "session".into(),
                    generation: 1,
                })),
            ));
            let route = ingress.route.clone();
            let origin = ingress.origin.clone();
            let client = agent_client_protocol::Client
                .v2()
                .on_receive_request(
                    async move |request: RawRequest, responder, _cx| {
                        ingress.dispatch(request, responder)
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |_notification: wire::UpdateSessionNotification, _cx| Ok(()),
                    agent_client_protocol::on_receive_notification!(),
                );
            let (client_transport, agent_transport) = Channel::duplex();
            let (ready_tx, ready_rx) = oneshot::channel();
            let (done_tx, done_rx) = oneshot::channel();
            let client_task = tokio::spawn(async move {
                client
                    .connect_with(client_transport, async move |connection| {
                        let initialized = connection
                            .send_request(
                                wire::InitializeRequest::new(
                                    ProtocolVersion::V2,
                                    wire::Implementation::new("test-tui", "0"),
                                )
                                .capabilities(
                                    crate::tui::client_capabilities(
                                        &crate::tools::mcp::CredentialStorage::Memory,
                                    ),
                                ),
                            )
                            .block_task()
                            .await?;
                        origin.set(initialized.info.name).unwrap();
                        let _ = ready_tx.send(());
                        let _ = done_rx.await;
                        Ok::<_, agent_client_protocol::Error>(())
                    })
                    .await
            });
            agent_client_protocol::Agent.v2()
                .on_receive_request(async move |request: wire::InitializeRequest, responder, _cx| {
                    assert!(request.capabilities.elicitation.unwrap().form.is_some());
                    responder.respond(wire::InitializeResponse::new(ProtocolVersion::V2, wire::Implementation::new("question-agent", "0")))
                }, agent_client_protocol::on_receive_request!())
                .connect_with(agent_transport, async move |connection| {
                    ready_rx.await.unwrap();
                    let tool = crate::tools::ask_user::AskUserTool::new(connection, wire::SessionId::new("session"));
                    let controller = CancellationController::new();
                    let context = OwnedToolContext {
                        session_id: SessionId::new("session"), turn_id: TurnId::new("turn"), metadata: MetadataMap::new(),
                        permissions: Arc::new(AllowAllPermissions), resources: Arc::new(()), cancellation: Some(controller.handle().checkpoint()),
                        execution_scope: None, approved_request: None,
                    };
                    let arguments = json!({"message":"Choose the implementation.","questions":[
                        {"id":"scope","title":"Scope","prompt":"Which parts?","multiple":true,"allow_custom":true,"options":[{"id":"tui","label":"Terminal UI"},{"id":"runtime","label":"Agent runtime"}]},
                        {"id":"strategy","title":"Strategy","prompt":"How should we proceed?","options":[{"id":"small","label":"Small patch"},{"id":"full","label":"Full implementation"}]}
                    ]});
                    // Deliberately omit both optional flags on the second question.
                    // The tool must default to single selection with no custom field.
                    for include_selections in [true, false] {
                    let operation = async {
                        tool.invoke(ToolRequest::new("call", "ask_user", arguments.clone(), "session", "turn"), &mut context.borrowed()).await.unwrap()
                    };
                    let interaction = async {
                        let queued = incoming.recv().await.unwrap();
                        let mut app = app();
                        app.apply(queued.update);
                        let visible = screen(&mut app);
                        assert!(!visible.contains("Request from"));
                        assert!(!visible.contains("Agent request"));
                        assert!(visible.contains("Question 1/2"));
                        assert!(visible.contains("Terminal UI"));
                        assert!(visible.contains("Other"));
                        // Choice focus does not accept text or paste.
                        app.paste("not an answer");
                        press(&mut app, KeyCode::Char('z'));
                        if include_selections {
                            for key in [KeyCode::Char(' '), KeyCode::Down, KeyCode::Char(' ')] { press(&mut app, key); }
                            press(&mut app, KeyCode::Tab);
                        } else {
                            press(&mut app, KeyCode::Enter);
                            assert!(screen(&mut app).contains("non-blank"));
                            app.paste("   ");
                            press(&mut app, KeyCode::Enter);
                            assert!(screen(&mut app).contains("non-blank"));
                            app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
                        }
                        assert!(screen(&mut app).contains("Focus: Other"));
                        app.paste("Also docs, please");
                        let stale = app.clipboard_route();
                        press(&mut app, KeyCode::BackTab);
                        assert!(screen(&mut app).contains("Focus: choices"));
                        assert_eq!(crate::tui::apply_clipboard_completion(&mut app, &route, 1, stale,
                            crate::tui::ClipboardResult::Text("stale".into())), None);
                        press(&mut app, KeyCode::Tab);
                        // Moving the cursor and inserting paste uses the same editor.
                        press(&mut app, KeyCode::Home);
                        app.paste("X");
                        press(&mut app, KeyCode::Backspace);
                        press(&mut app, KeyCode::Enter);
                        let visible = screen(&mut app);
                        assert!(visible.contains("Question 2/2"));
                        assert!(visible.contains("Strategy"));
                        press(&mut app, KeyCode::BackTab);
                        assert!(screen(&mut app).contains("Also docs, please"));
                        press(&mut app, KeyCode::Enter);
                        // A second single selection replaces the first.
                        for key in [KeyCode::Down, KeyCode::Char(' '), KeyCode::Up, KeyCode::Char(' '), KeyCode::Enter] { press(&mut app, key); }
                        press(&mut app, KeyCode::BackTab);
                        press(&mut app, KeyCode::Enter);
                        let review = screen(&mut app);
                        assert!(review.contains("Review answers"));
                        if include_selections { assert!(review.contains("Terminal UI, Agent runtime")); }
                        assert!(review.contains("Also docs, please"));
                        assert!(review.contains("Small patch"));
                        assert_eq!(app.elicitations.len(), 1);
                        press(&mut app, KeyCode::Enter);
                        assert!(app.elicitations.is_empty());
                    };
                    let (result, ()) = tokio::join!(operation, interaction);
                    assert!(!result.result.is_error);
                    assert_eq!(result.result.output, ToolOutput::structured(json!({"action":"accept","answers":{
                        "scope":{"selected":if include_selections {vec!["tui","runtime"]} else {vec![]},"custom":"Also docs, please"},
                        "strategy":{"selected":["small"]}
                    }})));
                    }
                    Ok::<_, agent_client_protocol::Error>(())
                }).await.unwrap();
            let _ = done_tx.send(());
            client_task.await.unwrap().unwrap();
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), exercise)
            .await
            .expect("tool/form roundtrip stalled");
    }

    #[test]
    fn elicitation_capability_is_form_only_for_all_credential_modes() {
        use crate::tools::mcp::CredentialStorage;
        for credentials in [CredentialStorage::Memory, CredentialStorage::Keychain] {
            let capabilities = crate::tui::client_capabilities(&credentials);
            let elicitation = capabilities.elicitation.unwrap();
            assert!(elicitation.form.is_some());
            assert!(elicitation.url.is_none());
        }
    }

    #[test]
    fn elicitation_consumes_input_without_editing_the_composer() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let ingress = ingress();
        let mut app = app();
        let (queued, mut answer) = ingress.prepare(request("session")).unwrap();
        app.apply(queued.update);
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        crate::tui::handle_paste(&mut app, "some/path.txt");
        assert_eq!(app.editor.text(), "");
        assert!(app.attachments.is_empty());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            answer.try_recv().unwrap().action,
            wire::ElicitationAction::Cancel
        ));
        assert!(app.elicitations.is_empty());
    }

    #[test]
    fn elicitation_idle_cancelled_snapshot_does_not_cancel_user_request() {
        let ingress = ingress();
        let mut app = app();
        let (queued, mut receiver) = ingress.prepare(request("session")).unwrap();
        app.apply(queued.update);
        app.apply(Update::State(wire::StateUpdate::Idle(
            wire::IdleStateUpdate::new().stop_reason(wire::StopReason::Cancelled),
        )));
        assert_eq!(app.elicitations.len(), 1);
        assert!(matches!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn elicitation_raw_schema_constraints_are_not_silently_discarded() {
        let ingress = ingress();
        let mut input = request("session");
        input.0["requestedSchema"]["allOf"] = serde_json::json!([]);
        assert!(ingress.prepare(input).is_err());
        let mut input = request("session");
        input.0["requestedSchema"]["type"] = serde_json::json!("array");
        assert!(ingress.prepare(input).is_err());
        let mut input = request("session");
        input.0["requestedSchema"]["properties"]["scope"]["not"] =
            serde_json::json!({"const":"small"});
        assert!(ingress.prepare(input).is_err());
    }

    #[test]
    fn elicitation_cancelled_modal_consumes_key_before_exposing_composer() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let ingress = ingress();
        let mut app = app();
        app.editor.insert_str("unsent draft");
        let (queued, receiver) = ingress.prepare(request("session")).unwrap();
        app.apply(queued.update);
        assert!(app.needs_redraw_tick());
        drop(receiver);
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            crate::tui::app::Action::Redraw
        ));
        assert_eq!(app.editor.text(), "unsent draft");
        assert!(app.elicitations.is_empty());
    }

    #[test]
    fn elicitation_poisoned_route_fails_closed() {
        let ingress = ingress();
        let route = ingress.route.clone();
        let _ = std::thread::spawn(move || {
            let _guard = route.lock().unwrap();
            panic!("route writer failed");
        })
        .join();
        assert!(ingress.prepare(request("session")).is_err());
    }
}
