use super::*;
use agentkit_core::{CancellationController, MetadataMap, SessionId, TurnId};
use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext};
use serde_json::json;

fn example() -> Value {
    json!({"message":"Choose the implementation.","questions":[
        {"id":"scope","title":"Scope","prompt":"Which parts?","multiple":true,"allow_custom":true,"options":[{"id":"tui","label":"TUI"},{"id":"runtime","label":"Runtime"}]},
        {"id":"strategy","title":"Strategy","prompt":"How?","options":[{"id":"small","label":"Small patch"},{"id":"full","label":"Full implementation"}]}
    ]})
}
fn input() -> Input {
    serde_json::from_value(example()).unwrap()
}
fn response(value: Value) -> wire::CreateElicitationResponse {
    serde_json::from_value(value).unwrap()
}

#[test]
fn form_preserves_order_ids_labels_and_custom_fields() {
    let schema = serde_json::to_value(input().schema().unwrap()).unwrap();
    assert_eq!(
        schema["_meta"]["kit/questions"]["order"],
        json!(["question_0", "question_0_custom", "question_1"])
    );
    assert_eq!(schema["required"], json!(["question_1"]));
    assert_eq!(
        schema["properties"]["question_0"]["items"]["anyOf"][0]["const"],
        "tui"
    );
    assert_eq!(
        schema["properties"]["question_1"]["oneOf"][0]["title"],
        "Small patch"
    );
}

#[test]
fn accepted_answers_are_keyed_by_stable_ids_and_custom_is_separate() {
    let result = input().answer(response(json!({"action":"accept","content":{"question_0":["runtime","tui"],"question_0_custom":"Also docs, please","question_1":"small"}}))).unwrap();
    assert_eq!(
        result,
        json!({"action":"accept","answers":{"scope":{"selected":["runtime","tui"],"custom":"Also docs, please"},"strategy":{"selected":["small"]}}})
    );
    assert!(input().answer(response(json!({"action":"accept","content":{"question_0_custom":"Different approach","question_1":"small"}}))).is_ok());
}

#[test]
fn rejects_missing_unknown_duplicate_or_wrong_typed_answers() {
    for content in [
        json!({"question_0":[],"question_1":"small"}),
        json!({"question_0":["tui","tui"],"question_1":"small"}),
        json!({"question_0":["missing"],"question_1":"small"}),
        json!({"question_0":"tui","question_1":"small"}),
        json!({"question_0":["tui"],"question_1":["small"]}),
        json!({"question_0":["tui"]}),
        json!({"question_0":["tui"],"question_1":"small","extra":"ignored?"}),
        json!({"question_0":["tui"],"question_1":"small","question_1_custom":"not allowed"}),
    ] {
        assert!(
            input()
                .answer(response(json!({"action":"accept","content":content})))
                .is_err(),
            "{content}"
        );
    }
    assert!(
        input()
            .answer(response(json!({"action":"accept"})))
            .is_err()
    );
    for action in ["cancel", "decline"] {
        assert_eq!(
            input().answer(response(json!({"action":action}))).unwrap(),
            json!({"action":action})
        );
    }
}

#[test]
fn rejects_duplicate_ids_and_bounded_input_violations() {
    for value in [
        {
            let mut v = example();
            v["questions"][1]["id"] = json!("scope");
            v
        },
        {
            let mut v = example();
            v["questions"][0]["options"][1]["id"] = json!("tui");
            v
        },
        {
            let mut v = example();
            v["questions"][0]["id"] = json!("invalid.id");
            v
        },
        {
            let mut v = example();
            v["questions"][0]["options"] = json!([]);
            v
        },
        {
            let mut v = example();
            v["message"] = json!("x".repeat(2001));
            v
        },
        {
            let mut v = example();
            v["questions"] = json!([]);
            v
        },
    ] {
        assert!(
            serde_json::from_value::<Input>(value)
                .unwrap()
                .schema()
                .is_err()
        );
    }
}

fn context(controller: &CancellationController) -> OwnedToolContext {
    OwnedToolContext {
        session_id: SessionId::new("session"),
        turn_id: TurnId::new("turn"),
        metadata: MetadataMap::new(),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: Some(controller.handle().checkpoint()),
        execution_scope: None,
        approved_request: None,
    }
}

#[tokio::test]
async fn real_acp_request_returns_answer_and_restores_running() {
    let (client_transport, agent_transport) = agent_client_protocol::Channel::duplex();
    let (states, mut received) = tokio::sync::mpsc::unbounded_channel();
    let client = agent_client_protocol::Client.v2()
        .on_receive_notification(async move |notification: wire::UpdateSessionNotification, _cx| {
            if let wire::SessionUpdate::StateUpdate(state) = notification.update { states.send(state).unwrap(); }
            Ok(())
        }, agent_client_protocol::on_receive_notification!())
        .on_receive_request(async move |request: wire::CreateElicitationRequest, responder, _cx| {
            assert_eq!(request.message,"Choose the implementation.");
            let wire::ElicitationMode::Form(form) = request.mode else { panic!("expected form") };
            let wire::ElicitationScope::Session(scope) = form.scope else { panic!("expected session") };
            assert_eq!(scope.session_id.to_string(),"session");
            assert_eq!(scope.tool_call_id.unwrap().to_string(),"call");
            responder.respond(response(json!({"action":"accept","content":{"question_0":["tui"],"question_1":"small"}})))
        }, agent_client_protocol::on_receive_request!());
    let (ready, initialized) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        client
            .connect_with(client_transport, async move |connection| {
                connection
                    .send_request(
                        wire::InitializeRequest::new(
                            agentkit_acp::v2::wire::ProtocolVersion::V2,
                            wire::Implementation::new("questions-test", "0"),
                        )
                        .capabilities(
                            wire::ClientCapabilities::new().elicitation(
                                wire::ElicitationCapabilities::new()
                                    .form(wire::ElicitationFormCapabilities::new()),
                            ),
                        ),
                    )
                    .block_task()
                    .await?;
                let _ = ready.send(());
                std::future::pending::<Result<(), agent_client_protocol::Error>>().await
            })
            .await
    });
    agent_client_protocol::Agent
        .v2()
        .on_receive_request(
            async move |_request: wire::InitializeRequest, responder, _cx| {
                responder.respond(wire::InitializeResponse::new(
                    agentkit_acp::v2::wire::ProtocolVersion::V2,
                    wire::Implementation::new("questions-test-agent", "0"),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent_transport, async move |connection| {
            initialized.await.unwrap();
            let tool = AskUserTool::new(connection, wire::SessionId::new("session"));
            let cancelled_controller = CancellationController::new();
            let cancelled_context = context(&cancelled_controller);
            cancelled_controller.interrupt();
            assert!(matches!(
                tool.invoke(
                    ToolRequest::new(
                        "already-cancelled",
                        "ask_user",
                        example(),
                        "session",
                        "turn"
                    ),
                    &mut cancelled_context.borrowed(),
                )
                .await,
                Err(ToolError::Cancelled)
            ));
            let controller = CancellationController::new();
            let context = context(&controller);
            let wrong_session = tool
                .invoke(
                    ToolRequest::new("wrong", "ask_user", example(), "another-session", "turn"),
                    &mut context.borrowed(),
                )
                .await;
            assert!(matches!(wrong_session, Err(ToolError::ExecutionFailed(_))));
            let result = tool
                .invoke(
                    ToolRequest::new("call", "ask_user", example(), "session", "turn"),
                    &mut context.borrowed(),
                )
                .await
                .unwrap();
            assert!(!result.result.is_error);
            assert!(matches!(
                received.recv().await,
                Some(wire::StateUpdate::RequiresAction(_))
            ));
            assert!(matches!(
                received.recv().await,
                Some(wire::StateUpdate::Running(_))
            ));
            Ok::<_, agent_client_protocol::Error>(())
        })
        .await
        .unwrap();
    task.abort();
}

#[tokio::test]
async fn turn_cancellation_cancels_individual_acp_request() {
    let (client_transport, agent_transport) = agent_client_protocol::Channel::duplex();
    let controller = CancellationController::new();
    let interrupt = controller.clone();
    let (cancelled, cancellation_seen) = tokio::sync::oneshot::channel();
    let cancelled = Arc::new(tokio::sync::Mutex::new(Some(cancelled)));
    let (states, mut received_states) = tokio::sync::mpsc::unbounded_channel();
    let client = agent_client_protocol::Client
        .v2()
        .on_receive_notification(
            async move |notification: wire::UpdateSessionNotification, _cx| {
                if let wire::SessionUpdate::StateUpdate(state) = notification.update {
                    states.send(state).unwrap();
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |_request: wire::CreateElicitationRequest, responder, cx| {
                let cancellation = responder.cancellation();
                let cancelled = cancelled.clone();
                interrupt.interrupt();
                cx.spawn(async move {
                    cancellation.cancelled().await;
                    if let Some(sender) = cancelled.lock().await.take() {
                        let _ = sender.send(());
                    }
                    responder.respond(response(json!({"action":"cancel"})))
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        );
    let (ready, initialized) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        client
            .connect_with(client_transport, async move |connection| {
                connection
                    .send_request(
                        wire::InitializeRequest::new(
                            agentkit_acp::v2::wire::ProtocolVersion::V2,
                            wire::Implementation::new("questions-test", "0"),
                        )
                        .capabilities(
                            wire::ClientCapabilities::new().elicitation(
                                wire::ElicitationCapabilities::new()
                                    .form(wire::ElicitationFormCapabilities::new()),
                            ),
                        ),
                    )
                    .block_task()
                    .await?;
                let _ = ready.send(());
                std::future::pending::<Result<(), agent_client_protocol::Error>>().await
            })
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent_client_protocol::Agent
            .v2()
            .on_receive_request(
                async move |_request: wire::InitializeRequest, responder, _cx| {
                    responder.respond(wire::InitializeResponse::new(
                        agentkit_acp::v2::wire::ProtocolVersion::V2,
                        wire::Implementation::new("questions-test-agent", "0"),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent_transport, async move |connection| {
                initialized.await.unwrap();
                let tool = AskUserTool::new(connection, wire::SessionId::new("session"));
                let context = context(&controller);
                let result = tool
                    .invoke(
                        ToolRequest::new("call", "ask_user", example(), "session", "turn"),
                        &mut context.borrowed(),
                    )
                    .await;
                assert!(matches!(result, Err(ToolError::Cancelled)));
                cancellation_seen.await.unwrap();
                assert!(matches!(
                    received_states.recv().await,
                    Some(wire::StateUpdate::RequiresAction(_))
                ));
                assert!(matches!(
                    received_states.recv().await,
                    Some(wire::StateUpdate::Running(_))
                ));
                Ok::<_, agent_client_protocol::Error>(())
            }),
    )
    .await
    .unwrap()
    .unwrap();
    task.abort();
}

#[tokio::test]
async fn cancelled_waiter_does_not_publish_and_admission_is_reusable() {
    let (client_transport, agent_transport) = agent_client_protocol::Channel::duplex();
    let (requests, mut received_requests) = tokio::sync::mpsc::unbounded_channel();
    let (states, mut received_states) = tokio::sync::mpsc::unbounded_channel();
    let client = agent_client_protocol::Client
        .v2()
        .on_receive_notification(
            async move |notification: wire::UpdateSessionNotification, _cx| {
                if let wire::SessionUpdate::StateUpdate(state) = notification.update {
                    states.send(state).unwrap();
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |_request: wire::CreateElicitationRequest, responder, _cx| {
                requests.send(responder).unwrap();
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        );
    let (ready, initialized) = tokio::sync::oneshot::channel();
    let client_task = tokio::spawn(async move {
        client
            .connect_with(client_transport, async move |connection| {
                connection
                    .send_request(
                        wire::InitializeRequest::new(
                            agentkit_acp::v2::wire::ProtocolVersion::V2,
                            wire::Implementation::new("questions-test", "0"),
                        )
                        .capabilities(
                            wire::ClientCapabilities::new().elicitation(
                                wire::ElicitationCapabilities::new()
                                    .form(wire::ElicitationFormCapabilities::new()),
                            ),
                        ),
                    )
                    .block_task()
                    .await?;
                let _ = ready.send(());
                std::future::pending::<Result<(), agent_client_protocol::Error>>().await
            })
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent_client_protocol::Agent
            .v2()
            .on_receive_request(
                async move |_request: wire::InitializeRequest, responder, _cx| {
                    responder.respond(wire::InitializeResponse::new(
                        agentkit_acp::v2::wire::ProtocolVersion::V2,
                        wire::Implementation::new("questions-test-agent", "0"),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent_transport, async move |connection| {
                initialized.await.unwrap();
                let tool = AskUserTool::new(connection, wire::SessionId::new("session"));
                let clone = tool.clone();
                let first_controller = CancellationController::new();
                let first_context = context(&first_controller);
                let mut first_context = first_context.borrowed();
                let first = tool.invoke(
                    ToolRequest::new("first", "ask_user", example(), "session", "turn"),
                    &mut first_context,
                );
                tokio::pin!(first);
                assert!(futures_util::poll!(&mut first).is_pending());
                let first_request = received_requests.recv().await.unwrap();
                assert!(matches!(
                    received_states.recv().await,
                    Some(wire::StateUpdate::RequiresAction(_))
                ));

                let second_controller = CancellationController::new();
                let second_context = context(&second_controller);
                let mut second_context = second_context.borrowed();
                let second = clone.invoke(
                    ToolRequest::new("second", "ask_user", example(), "session", "turn"),
                    &mut second_context,
                );
                tokio::pin!(second);
                assert!(futures_util::poll!(&mut second).is_pending());
                second_controller.interrupt();
                assert!(matches!(second.await, Err(ToolError::Cancelled)));
                first_request.respond(response(json!({"action":"decline"})))?;
                assert!(first.await.is_ok());
                assert!(matches!(
                    received_states.recv().await,
                    Some(wire::StateUpdate::Running(_))
                ));

                // The cancelled waiter neither steals admission nor leaves a stale RPC.
                let third_controller = CancellationController::new();
                let third_context = context(&third_controller);
                let mut third_context = third_context.borrowed();
                let (third, ()) = tokio::join!(
                    tool.invoke(
                        ToolRequest::new("third", "ask_user", example(), "session", "turn"),
                        &mut third_context
                    ),
                    async {
                        let request = received_requests.recv().await.unwrap();
                        request
                            .respond(response(json!({"action":"decline"})))
                            .unwrap();
                    }
                );
                assert!(third.is_ok());
                assert!(matches!(
                    received_states.recv().await,
                    Some(wire::StateUpdate::RequiresAction(_))
                ));
                assert!(matches!(
                    received_states.recv().await,
                    Some(wire::StateUpdate::Running(_))
                ));
                Ok::<_, agent_client_protocol::Error>(())
            }),
    )
    .await
    .unwrap()
    .unwrap();
    client_task.abort();
}

#[tokio::test]
async fn cancelled_turn_does_not_accept_a_racing_response() {
    let (client_transport, agent_transport) = agent_client_protocol::Channel::duplex();
    let controller = CancellationController::new();
    let interrupt = controller.clone();
    let client = agent_client_protocol::Client.v2()
        .on_receive_notification(async move |_notification:wire::UpdateSessionNotification,_cx| Ok(()),agent_client_protocol::on_receive_notification!())
        .on_receive_request(async move |_request:wire::CreateElicitationRequest,responder,_cx| {
            interrupt.interrupt();
            responder.respond(response(json!({"action":"accept","content":{"question_0":["tui"],"question_1":"small"}})))
        },agent_client_protocol::on_receive_request!());
    let (ready, initialized) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        client
            .connect_with(client_transport, async move |connection| {
                connection
                    .send_request(
                        wire::InitializeRequest::new(
                            agentkit_acp::v2::wire::ProtocolVersion::V2,
                            wire::Implementation::new("questions-test", "0"),
                        )
                        .capabilities(
                            wire::ClientCapabilities::new().elicitation(
                                wire::ElicitationCapabilities::new()
                                    .form(wire::ElicitationFormCapabilities::new()),
                            ),
                        ),
                    )
                    .block_task()
                    .await?;
                let _ = ready.send(());
                std::future::pending::<Result<(), agent_client_protocol::Error>>().await
            })
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent_client_protocol::Agent
            .v2()
            .on_receive_request(
                async move |_request: wire::InitializeRequest, responder, _cx| {
                    responder.respond(wire::InitializeResponse::new(
                        agentkit_acp::v2::wire::ProtocolVersion::V2,
                        wire::Implementation::new("questions-test-agent", "0"),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent_transport, async move |connection| {
                initialized.await.unwrap();
                let tool = AskUserTool::new(connection, wire::SessionId::new("session"));
                let context = context(&controller);
                let result = tool
                    .invoke(
                        ToolRequest::new("call", "ask_user", example(), "session", "turn"),
                        &mut context.borrowed(),
                    )
                    .await;
                assert!(matches!(result, Err(ToolError::Cancelled)));
                Ok::<_, agent_client_protocol::Error>(())
            }),
    )
    .await
    .unwrap()
    .unwrap();
    task.abort();
}

#[test]
fn custom_answer_requirements_are_advertised_for_client_validation() {
    let schema = serde_json::to_value(input().schema().unwrap()).unwrap();
    assert_eq!(
        schema["_meta"]["kit/questions"]["require_answer"],
        json!([
            {"selection":"question_0", "custom":"question_0_custom"}
        ])
    );
    for (selection, custom) in [(json!([]), json!("")), (json!([]), json!("   "))] {
        assert!(
            input()
                .answer(response(json!({"action":"accept", "content":{
                    "question_0":selection, "question_0_custom":custom, "question_1":"small"
                }})))
                .is_err()
        );
    }
}

#[test]
fn aggregate_request_limit_counts_serialized_utf8_not_characters() {
    let make = |character: &str| -> Input {
        serde_json::from_value(json!({"message":"Choose options", "questions":(0..8).map(|q| json!({
            "id":format!("q_{q}"),"title":"Scope","prompt":"Which parts?",
            "options":(0..32).map(|i| json!({"id":format!("option_{i}"),"label":"Option","description":character.repeat(300)})).collect::<Vec<_>>()
        })).collect::<Vec<_>>()})).unwrap()
    };
    let ascii = make("a");
    let unicode = make("🦀");
    assert!(ascii.validate().is_ok());
    assert!(
        unicode.validate().is_ok(),
        "all per-field character limits are satisfied"
    );
    let accepted = ascii
        .request(
            wire::SessionId::new("session"),
            wire::ToolCallId::new("call"),
        )
        .unwrap();
    assert!(serde_json::to_vec(&accepted).unwrap().len() <= MAX_FORM_REQUEST_BYTES);
    let rejected = unicode.request(
        wire::SessionId::new("session"),
        wire::ToolCallId::new("call"),
    );
    assert!(
        matches!(rejected, Err(ToolError::InvalidInput(message)) if message.contains("128 KiB") && message.contains("split"))
    );
}
